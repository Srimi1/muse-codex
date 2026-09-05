use std::collections::VecDeque;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use codex_transport::RawBody;
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};

const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
const EVENT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Validates the upstream Responses SSE stream while preserving valid frames.
///
/// Network, framing, UTF-8, JSON, oversized-frame, and unknown-event failures
/// are converted into a non-retryable `response.failed` event. This matters
/// because stock Muse retries a bare EOF even after observable output. A
/// terminal provider event instead leaves the session resumable without
/// replaying a completed tool call.
#[cfg(test)]
pub fn validated_stream(
    body: RawBody,
) -> impl Stream<Item = Result<Bytes, codex_transport::Error>> + Send + 'static {
    validated_stream_with_options_and_media_type(body, EVENT_IDLE_TIMEOUT, MAX_EVENT_BYTES, None)
}

/// Validates a Responses stream while retaining a pre-sanitized upstream MIME
/// type solely as a protocol-failure diagnostic. A mismatched header never
/// rejects an otherwise valid typed SSE stream.
pub fn validated_stream_with_media_type(
    body: RawBody,
    upstream_media_type: Option<String>,
) -> impl Stream<Item = Result<Bytes, codex_transport::Error>> + Send + 'static {
    validated_stream_with_options_and_media_type(
        body,
        EVENT_IDLE_TIMEOUT,
        MAX_EVENT_BYTES,
        upstream_media_type,
    )
}

#[cfg(test)]
fn validated_stream_with_options(
    body: RawBody,
    idle_timeout: Duration,
    max_event_bytes: usize,
) -> impl Stream<Item = Result<Bytes, codex_transport::Error>> + Send + 'static {
    validated_stream_with_options_and_media_type(body, idle_timeout, max_event_bytes, None)
}

fn validated_stream_with_options_and_media_type(
    body: RawBody,
    idle_timeout: Duration,
    max_event_bytes: usize,
    upstream_media_type: Option<String>,
) -> impl Stream<Item = Result<Bytes, codex_transport::Error>> + Send + 'static {
    futures_util::stream::unfold(
        State::new(body, idle_timeout, max_event_bytes, upstream_media_type),
        |mut state| async move {
            loop {
                if let Some(frame) = state.pending.pop_front() {
                    return Some((Ok(frame), state));
                }
                if state.finished {
                    return None;
                }

                state.drain_complete_frames();
                if let Some(frame) = state.pending.pop_front() {
                    return Some((Ok(frame), state));
                }
                if state.finished {
                    return None;
                }

                match tokio::time::timeout(state.idle_timeout, state.body.next()).await {
                    Ok(Some(Ok(chunk))) => {
                        state.buffer.extend_from_slice(&chunk);
                        state.drain_complete_frames();
                        if !state.finished && state.buffer.len() > state.max_event_bytes {
                            state.fail("upstream SSE event exceeded the safety limit");
                        }
                    }
                    Ok(Some(Err(_))) => {
                        state.fail("upstream response stream was interrupted");
                    }
                    Ok(None) => {
                        if state.buffer.iter().all(u8::is_ascii_whitespace) {
                            state.buffer.clear();
                            state.fail("upstream stream ended before a terminal response event");
                        } else {
                            state.fail("upstream stream ended with an incomplete SSE event");
                        }
                    }
                    Err(_) => {
                        state.fail("upstream response stream exceeded the idle timeout");
                    }
                }
            }
        },
    )
}

struct State {
    body: RawBody,
    buffer: BytesMut,
    pending: VecDeque<Bytes>,
    idle_timeout: Duration,
    max_event_bytes: usize,
    finished: bool,
    response_id: Option<String>,
    model: Option<String>,
    last_sequence: u64,
    upstream_media_type: Option<String>,
}

impl State {
    fn new(
        body: RawBody,
        idle_timeout: Duration,
        max_event_bytes: usize,
        upstream_media_type: Option<String>,
    ) -> Self {
        Self {
            body,
            buffer: BytesMut::new(),
            pending: VecDeque::new(),
            idle_timeout,
            max_event_bytes,
            finished: false,
            response_id: None,
            model: None,
            last_sequence: 0,
            upstream_media_type,
        }
    }

    fn drain_complete_frames(&mut self) {
        while !self.finished {
            let Some(frame_len) = complete_frame_len(&self.buffer) else {
                break;
            };
            if frame_len > self.max_event_bytes {
                self.fail("upstream SSE event exceeded the safety limit");
                break;
            }

            let frame = self.buffer.split_to(frame_len).freeze();
            match inspect_frame(&frame) {
                Ok(FrameDisposition::Keep(event)) => {
                    self.observe(&event);
                    if matches!(
                        event.kind.as_str(),
                        "error" | "response.failed" | "response.cancelled"
                    ) {
                        // Provider error messages can echo request or account
                        // data. Replace both supported failure shapes with a
                        // complete static terminal event, retaining only the
                        // small code/parameter allowlist Muse needs for error
                        // handling and compaction.
                        let failure = if event.kind == "response.cancelled" {
                            SanitizedStreamFailure {
                                code: "invalid_request",
                                message: "The Codex stream was cancelled; the request was not replayed and the session is resumable.",
                                parameter: None,
                            }
                        } else {
                            sanitize_stream_failure(&event.value)
                        };
                        let frame = failure_frame(
                            self.response_id.as_deref(),
                            self.model.as_deref(),
                            self.last_sequence,
                            failure.code,
                            failure.message,
                            failure.parameter.as_deref(),
                        );
                        self.pending.push_back(frame);
                        self.finished = true;
                        self.buffer.clear();
                        continue;
                    }
                    let terminal = is_terminal_event(&event.kind);
                    self.pending.push_back(frame);
                    if terminal {
                        self.finished = true;
                        self.buffer.clear();
                    }
                }
                Ok(FrameDisposition::Drop(event)) => {
                    // Metadata is intentionally not exposed to Muse 1.0.3, but
                    // it still participates in identity and sequence tracking.
                    self.observe(&event);
                }
                Err(reason) => {
                    self.fail(reason);
                }
            }
        }
    }

    fn observe(&mut self, event: &InspectedEvent) {
        if let Some(sequence) = event.sequence_number {
            self.last_sequence = self.last_sequence.max(sequence);
        }
        if let Some(response) = event.value.get("response") {
            if let Some(id) = response
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| safe_protocol_identifier(value))
            {
                self.response_id = Some(id.to_owned());
            }
            if let Some(model) = response
                .get("model")
                .and_then(Value::as_str)
                .filter(|value| safe_protocol_identifier(value))
            {
                self.model = Some(model.to_owned());
            }
        }
        if let Some(id) = event
            .value
            .get("response_id")
            .and_then(Value::as_str)
            .filter(|value| safe_protocol_identifier(value))
        {
            self.response_id = Some(id.to_owned());
        }
        if let Some(model) =
            event_reported_model(&event.value).filter(|value| safe_protocol_identifier(value))
        {
            self.model = Some(model.to_owned());
        }
    }

    fn fail(&mut self, reason: &str) {
        if self.finished {
            return;
        }
        let reason = self.upstream_media_type.as_deref().map_or_else(
            || reason.to_string(),
            |media_type| format!("{reason} (upstream media type {media_type})"),
        );
        self.pending.push_back(protocol_failure_frame(
            self.response_id.as_deref(),
            self.model.as_deref(),
            self.last_sequence.saturating_add(1),
            &reason,
        ));
        self.finished = true;
        self.buffer.clear();
    }
}

#[derive(Debug)]
struct InspectedEvent {
    kind: String,
    sequence_number: Option<u64>,
    value: Value,
}

enum FrameDisposition {
    Keep(InspectedEvent),
    Drop(InspectedEvent),
}

fn inspect_frame(frame: &[u8]) -> Result<FrameDisposition, &'static str> {
    let text = std::str::from_utf8(frame).map_err(|_| "upstream SSE event was not valid UTF-8")?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut event_name: Option<&str> = None;
    let mut data_lines = Vec::new();

    for raw_line in text.split_terminator(['\r', '\n']) {
        if raw_line.is_empty() || raw_line.starts_with(':') {
            continue;
        }
        let (field, value) = raw_line.split_once(':').unwrap_or((raw_line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event_name = Some(value),
            "data" => data_lines.push(value),
            // `id` and `retry` are valid SSE fields but do not affect the
            // Responses protocol. Unknown SSE fields are ignored by spec.
            _ => {}
        }
    }

    if data_lines.is_empty() {
        return if event_name.is_none() {
            Ok(FrameDisposition::Drop(InspectedEvent {
                kind: "sse.comment".to_string(),
                sequence_number: None,
                value: Value::Null,
            }))
        } else {
            Err("upstream SSE event contained no data")
        };
    }
    let data = data_lines.join("\n");
    if data == "[DONE]" {
        return Err("upstream sent [DONE] before response.completed");
    }

    let value: Value =
        serde_json::from_str(&data).map_err(|_| "upstream SSE event contained malformed JSON")?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or("upstream SSE event had no string type")?;
    if let Some(event_name) = event_name
        && event_name != "message"
        && event_name != kind
    {
        return Err("upstream SSE event name did not match its JSON type");
    }

    let metadata = is_metadata_event(kind);
    if !metadata && !is_known_event(kind) {
        return Err("upstream emitted an unknown non-metadata event");
    }

    let sequence_number = match value.get("sequence_number") {
        Some(sequence) => Some(
            sequence
                .as_u64()
                .ok_or("upstream SSE event had an invalid sequence_number")?,
        ),
        None => None,
    };
    validate_event_shape(kind, &value)?;
    let event = InspectedEvent {
        kind: kind.to_owned(),
        sequence_number,
        value,
    };
    if metadata {
        Ok(FrameDisposition::Drop(event))
    } else {
        Ok(FrameDisposition::Keep(event))
    }
}

fn validate_event_shape(kind: &str, value: &Value) -> Result<(), &'static str> {
    if matches!(
        kind,
        "response.created" | "response.completed" | "response.failed" | "response.incomplete"
    ) {
        let response = value
            .get("response")
            .and_then(Value::as_object)
            .ok_or("upstream response lifecycle event had no response object")?;
        response
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| safe_protocol_identifier(id))
            .ok_or("upstream response lifecycle event had no valid response id")?;
    }

    if kind == "response.cancelled" {
        value
            .get("response")
            .and_then(|response| response.get("id"))
            .or_else(|| value.get("response_id"))
            .and_then(Value::as_str)
            .filter(|id| safe_protocol_identifier(id))
            .ok_or("upstream response cancellation event had no valid response id")?;
    }

    if matches!(
        kind,
        "response.output_text.delta"
            | "response.refusal.delta"
            | "response.function_call_arguments.delta"
            | "response.custom_tool_call_input.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.audio.delta"
            | "response.audio.transcript.delta"
            | "response.output_audio.delta"
            | "response.output_audio_transcript.delta"
            | "response.code_interpreter_call_code.delta"
            | "response.mcp_call_arguments.delta"
    ) && !value.get("delta").is_some_and(Value::is_string)
    {
        return Err("upstream response delta event had no string delta");
    }

    if matches!(
        kind,
        "response.output_item.added" | "response.output_item.done"
    ) && !value.get("item").is_some_and(Value::is_object)
    {
        return Err("upstream response item event had no item object");
    }

    if kind == "error"
        && value
            .get("message")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err("upstream error event had no message");
    }
    Ok(())
}

fn safe_protocol_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}

struct SanitizedStreamFailure {
    code: &'static str,
    message: &'static str,
    parameter: Option<String>,
}

fn sanitize_stream_failure(value: &Value) -> SanitizedStreamFailure {
    let error = value
        .get("response")
        .and_then(|response| response.get("error"))
        .or_else(|| value.get("error"))
        .unwrap_or(value);
    let code = match error.get("code").and_then(Value::as_str) {
        Some("context_length_exceeded") => "context_length_exceeded",
        Some("rate_limit_exceeded" | "usage_limit_reached") => "rate_limit_exceeded",
        Some("invalid_api_key") => "invalid_api_key",
        Some("model_not_found") => "model_not_found",
        Some("insufficient_quota") => "insufficient_quota",
        Some("invalid_request") => "invalid_request",
        _ => "invalid_request",
    };
    let message = match code {
        "context_length_exceeded" => {
            "The Codex stream reported that the model context window was exceeded; the request was not replayed and the session is resumable."
        }
        "rate_limit_exceeded" => {
            "The Codex stream reported a rate limit; the request was not replayed and the session is resumable."
        }
        "invalid_api_key" => {
            "The Codex stream rejected authentication; the request was not replayed and login is required."
        }
        "model_not_found" => {
            "The Codex stream reported that the requested model was unavailable; the request was not replayed and the session is resumable."
        }
        "insufficient_quota" => {
            "The Codex stream reported insufficient quota; the request was not replayed and the session is resumable."
        }
        _ => {
            "The Codex stream reported a terminal provider error; the request was not replayed and the session is resumable."
        }
    };
    let parameter = error
        .get("param")
        .and_then(Value::as_str)
        .filter(|value| safe_error_parameter(value))
        .map(ToOwned::to_owned);
    SanitizedStreamFailure {
        code,
        message,
        parameter,
    }
}

fn safe_error_parameter(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'[' | b']')
        })
}

fn event_reported_model(value: &Value) -> Option<&str> {
    value
        .get("response")
        .and_then(|response| response.get("headers"))
        .and_then(header_model)
        .or_else(|| value.get("headers").and_then(header_model))
}

fn header_model(value: &Value) -> Option<&str> {
    let headers = value.as_object()?;
    headers.iter().find_map(|(name, value)| {
        (name.eq_ignore_ascii_case("openai-model") || name.eq_ignore_ascii_case("x-openai-model"))
            .then(|| json_string_or_first(value))
            .flatten()
    })
}

fn json_string_or_first(value: &Value) -> Option<&str> {
    match value {
        Value::String(value) => Some(value),
        Value::Array(values) => values.first().and_then(json_string_or_first),
        _ => None,
    }
}

fn is_terminal_event(kind: &str) -> bool {
    matches!(
        kind,
        "response.completed" | "response.failed" | "response.incomplete" | "response.cancelled"
    )
}

fn is_metadata_event(kind: &str) -> bool {
    matches!(
        kind,
        "response.metadata" | "codex.response.metadata" | "codex.rate_limits"
    )
}

fn is_known_event(kind: &str) -> bool {
    matches!(
        kind,
        "error"
            | "response.created"
            | "response.queued"
            | "response.in_progress"
            | "response.completed"
            | "response.failed"
            | "response.incomplete"
            | "response.cancelled"
            | "response.output_item.added"
            | "response.output_item.done"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.delta"
            | "response.output_text.done"
            | "response.output_text.annotation.added"
            | "response.refusal.delta"
            | "response.refusal.done"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.custom_tool_call_input.delta"
            | "response.custom_tool_call_input.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done"
            | "response.audio.delta"
            | "response.audio.done"
            | "response.audio.transcript.delta"
            | "response.audio.transcript.done"
            | "response.output_audio.delta"
            | "response.output_audio.done"
            | "response.output_audio_transcript.delta"
            | "response.output_audio_transcript.done"
            | "response.file_search_call.in_progress"
            | "response.file_search_call.searching"
            | "response.file_search_call.completed"
            | "response.web_search_call.in_progress"
            | "response.web_search_call.searching"
            | "response.web_search_call.completed"
            | "response.code_interpreter_call.in_progress"
            | "response.code_interpreter_call.interpreting"
            | "response.code_interpreter_call.completed"
            | "response.code_interpreter_call_code.delta"
            | "response.code_interpreter_call_code.done"
            | "response.mcp_call.in_progress"
            | "response.mcp_call.completed"
            | "response.mcp_call.failed"
            | "response.mcp_call_arguments.delta"
            | "response.mcp_call_arguments.done"
            | "response.mcp_list_tools.in_progress"
            | "response.mcp_list_tools.completed"
            | "response.mcp_list_tools.failed"
            | "response.image_generation_call.in_progress"
            | "response.image_generation_call.generating"
            | "response.image_generation_call.completed"
            | "response.image_generation_call.partial_image"
    )
}

/// Returns the byte length through the first blank SSE line.
fn complete_frame_len(buffer: &[u8]) -> Option<usize> {
    let mut line_start = 0;
    let mut index = 0;
    while index < buffer.len() {
        let line_end = match buffer[index] {
            b'\n' => index + 1,
            b'\r' if index + 1 == buffer.len() => return None,
            b'\r' if buffer[index + 1] == b'\n' => index + 2,
            b'\r' => index + 1,
            _ => {
                index += 1;
                continue;
            }
        };
        if index == line_start {
            return Some(line_end);
        }
        line_start = line_end;
        index = line_end;
    }
    None
}

fn protocol_failure_frame(
    response_id: Option<&str>,
    model: Option<&str>,
    sequence_number: u64,
    reason: &str,
) -> Bytes {
    let message = format!("muse_codex_protocol_error: {reason}; session is resumable");
    failure_frame(
        response_id,
        model,
        sequence_number,
        "invalid_request",
        &message,
        None,
    )
}

pub(crate) fn gateway_failure_frame(
    model: Option<&str>,
    code: &'static str,
    message: &str,
    parameter: Option<&str>,
) -> Bytes {
    failure_frame(None, model, 0, code, message, parameter)
}

fn failure_frame(
    response_id: Option<&str>,
    model: Option<&str>,
    sequence_number: u64,
    code: &'static str,
    message: &str,
    parameter: Option<&str>,
) -> Bytes {
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let response_id = response_id
        .filter(|value| safe_protocol_identifier(value))
        .unwrap_or("resp_muse_codex_protocol_error");
    let model = model
        .filter(|value| safe_protocol_identifier(value))
        .unwrap_or("muse-codex");
    let event = json!({
        "type": "response.failed",
        "sequence_number": sequence_number,
        "response": {
            "id": response_id,
            "object": "response",
            "created_at": created_at,
            "status": "failed",
            "completed_at": Value::Null,
            "output": [],
            "usage": Value::Null,
            "error": {
                "code": code,
                "message": message,
                "param": parameter,
            },
            "previous_response_id": Value::Null,
            "model": model,
            "metadata": {},
            "incomplete_details": Value::Null,
        }
    });
    Bytes::from(format!("event: response.failed\ndata: {event}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};

    fn body(chunks: Vec<Result<Bytes, codex_transport::Error>>) -> RawBody {
        Box::pin(futures_util::stream::iter(chunks))
    }

    async fn collect(stream: impl Stream<Item = Result<Bytes, codex_transport::Error>>) -> Vec<u8> {
        futures_util::pin_mut!(stream);
        let mut output = Vec::new();
        while let Some(chunk) = stream.next().await {
            output.extend_from_slice(
                &chunk.expect("validator emits protocol failures, not I/O errors"),
            );
        }
        output
    }

    fn event(kind: &str, extra: &str) -> String {
        format!("event: {kind}\ndata: {{\"type\":\"{kind}\"{extra}}}\n\n")
    }

    #[tokio::test]
    async fn preserves_fragmented_utf8_and_crlf_frames() {
        let original = concat!(
            "event: response.output_text.delta\r\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"héllo\"}\r\n\r\n",
            "event: response.completed\r\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\r\n\r\n"
        )
        .as_bytes();
        let chunks = original
            .chunks(3)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        let output = collect(validated_stream(body(chunks))).await;
        assert_eq!(output, original);
    }

    /// Every event kind `codex_transport`'s Z.ai synthesizer can emit. It
    /// mirrors `EMITTED_EVENT_KINDS` in that crate; both lists must stay in
    /// sync, and this test is what fails if the gateway allowlist drifts.
    const ZAI_SYNTHESIZED_EVENT_KINDS: &[&str] = &[
        "error",
        "response.created",
        "response.in_progress",
        "response.completed",
        "response.incomplete",
        "response.output_item.added",
        "response.output_item.done",
        "response.content_part.added",
        "response.content_part.done",
        "response.output_text.delta",
        "response.output_text.done",
        "response.reasoning_summary_part.added",
        "response.reasoning_summary_part.done",
        "response.reasoning_summary_text.delta",
        "response.reasoning_summary_text.done",
        "response.function_call_arguments.delta",
        "response.function_call_arguments.done",
        "response.custom_tool_call_input.delta",
        "response.custom_tool_call_input.done",
    ];

    #[test]
    fn every_zai_synthesized_event_kind_is_accepted_here() {
        for kind in ZAI_SYNTHESIZED_EVENT_KINDS {
            assert!(is_known_event(kind), "{kind} is not in the allowlist");
        }
    }

    /// The Z.ai backend hands this validator a stream it synthesized rather
    /// than one an upstream produced, so the two contracts have to agree
    /// byte for byte.
    #[tokio::test]
    async fn a_synthesized_zai_stream_passes_through_unchanged() {
        let golden =
            include_bytes!("../../codex-transport/tests/fixtures/zai-responses.sse").as_slice();
        // Odd chunk boundaries also exercise the frame reassembly path.
        let chunks = golden
            .chunks(3)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        let output = collect(validated_stream(body(chunks))).await;
        assert_eq!(output, golden);
    }

    #[tokio::test]
    async fn interleaved_reasoning_text_and_parallel_tools_are_preserved() {
        let frames = [
            event(
                "response.reasoning_summary_text.delta",
                ",\"summary_index\":0,\"delta\":\"thinking\"",
            ),
            event(
                "response.output_text.delta",
                ",\"item_id\":\"m1\",\"output_index\":0,\"content_index\":0,\"delta\":\"answer\"",
            ),
            event(
                "response.function_call_arguments.delta",
                ",\"item_id\":\"f1\",\"output_index\":1,\"delta\":\"{}\"",
            ),
            event(
                "response.function_call_arguments.delta",
                ",\"item_id\":\"f2\",\"output_index\":2,\"delta\":\"{}\"",
            ),
            event("response.completed", ",\"response\":{\"id\":\"r1\"}"),
        ]
        .concat();
        let output = collect(validated_stream(body(vec![Ok(Bytes::from(
            frames.clone(),
        ))])))
        .await;
        assert_eq!(output, frames.as_bytes());
    }

    #[tokio::test]
    async fn interrupted_stream_becomes_non_retryable_terminal_event() {
        let first = event(
            "response.output_text.delta",
            ",\"sequence_number\":7,\"delta\":\"partial\"",
        );
        let output = collect(validated_stream(body(vec![
            Ok(Bytes::from(first.clone())),
            Err(codex_transport::Error::InvalidUpstreamResponse(
                "fixture disconnect".to_string(),
            )),
        ])))
        .await;
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with(&first));
        assert!(text.contains("event: response.failed"));
        assert!(text.contains("\"code\":\"invalid_request\""));
        assert!(text.contains("\"sequence_number\":8"));
        assert!(!text.contains("fixture disconnect"));
    }

    #[tokio::test]
    async fn rejects_unknown_non_metadata_event_but_drops_metadata() {
        let input = format!(
            "{}{}{}{}",
            event(
                "response.metadata",
                ",\"metadata\":{\"future_secret\":\"must-not-cross\"}",
            ),
            event(
                "codex.response.metadata",
                ",\"metadata\":{\"backend_secret\":\"must-not-cross-either\"}",
            ),
            event("response.future_mutation", ",\"secret\":\"not reflected\""),
            event(
                "response.completed",
                ",\"response\":{\"id\":\"never-forwarded\"}",
            )
        );
        let output =
            String::from_utf8(collect(validated_stream(body(vec![Ok(Bytes::from(input))]))).await)
                .unwrap();
        assert!(!output.contains("response.metadata"));
        assert!(!output.contains("must-not-cross"));
        assert!(!output.contains("backend_secret"));
        assert!(!output.contains("response.future_mutation"));
        assert!(!output.contains("not reflected"));
        assert!(output.contains("muse_codex_protocol_error"));
        assert!(output.contains("unknown non-metadata event"));
    }

    #[tokio::test]
    async fn malformed_and_truncated_events_fail_closed() {
        for input in [
            "event: response.output_text.delta\ndata: {not-json}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\"}",
        ] {
            let output = String::from_utf8(
                collect(validated_stream(body(vec![Ok(Bytes::from(
                    input.to_string(),
                ))])))
                .await,
            )
            .unwrap();
            assert!(output.contains("response.failed"));
            assert!(output.contains("session is resumable"));
        }
    }

    #[tokio::test]
    async fn oversized_event_and_idle_stream_fail_closed() {
        let oversized = format!(
            "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{}\"}}\n\n",
            "x".repeat(256)
        );
        let output = String::from_utf8(
            collect(validated_stream_with_options(
                body(vec![Ok(Bytes::from(oversized))]),
                Duration::from_secs(1),
                128,
            ))
            .await,
        )
        .unwrap();
        assert!(output.contains("safety limit"));

        let pending: RawBody = Box::pin(futures_util::stream::pending());
        let output = String::from_utf8(
            collect(validated_stream_with_options(
                pending,
                Duration::from_millis(5),
                MAX_EVENT_BYTES,
            ))
            .await,
        )
        .unwrap();
        assert!(output.contains("idle timeout"));
    }

    #[tokio::test]
    async fn terminal_event_stops_without_forwarding_trailing_bytes() {
        let completed = event("response.completed", ",\"response\":{\"id\":\"r1\"}");
        let input = format!("{completed}this must not be forwarded");
        let output = collect(validated_stream(body(vec![Ok(Bytes::from(input))]))).await;
        assert_eq!(output, completed.as_bytes());
    }

    #[tokio::test]
    async fn response_cancelled_is_translated_to_a_resumable_failure() {
        let cancelled = event(
            "response.cancelled",
            ",\"sequence_number\":5,\"response_id\":\"resp-cancelled\"",
        );
        let input = format!(
            "{cancelled}{}",
            event(
                "response.output_text.delta",
                ",\"sequence_number\":6,\"delta\":\"must not cross\"",
            )
        );
        let output = collect(validated_stream(body(vec![Ok(Bytes::from(input))]))).await;
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("event: response.failed"));
        assert!(output.contains("\"id\":\"resp-cancelled\""));
        assert!(output.contains("stream was cancelled"));
        assert!(!output.contains("must not cross"));
    }

    #[tokio::test]
    async fn metadata_is_dropped_but_advances_failure_identity_and_sequence() {
        let input = format!(
            "{}{}",
            event(
                "response.metadata",
                ",\"sequence_number\":9,\"response_id\":\"resp-meta\",\"headers\":{\"OpenAI-Model\":\"gpt-rerouted\"}",
            ),
            event("response.future_mutation", ",\"sequence_number\":10"),
        );
        let output =
            String::from_utf8(collect(validated_stream(body(vec![Ok(Bytes::from(input))]))).await)
                .unwrap();
        assert!(!output.contains("response.metadata"));
        assert!(output.contains("\"sequence_number\":10"));
        assert!(output.contains("\"id\":\"resp-meta\""));
        assert!(output.contains("\"model\":\"gpt-rerouted\""));
    }

    #[tokio::test]
    async fn response_headers_override_top_level_metadata_model_arrays() {
        let input = format!(
            "{}{}",
            event(
                "response.created",
                ",\"sequence_number\":3,\"headers\":{\"openai-model\":\"top-level\"},\"response\":{\"id\":\"resp-meta\",\"headers\":{\"OpenAI-Model\":[\"gpt-effective\"]}}",
            ),
            event("response.future_mutation", ",\"sequence_number\":4"),
        );
        let output =
            String::from_utf8(collect(validated_stream(body(vec![Ok(Bytes::from(input))]))).await)
                .unwrap();
        assert!(output.contains("\"model\":\"gpt-effective\""));
        assert!(!output.contains("\"model\":\"top-level\""));
    }

    #[tokio::test]
    async fn raw_error_event_becomes_sanitized_terminal_failure() {
        let input = event(
            "error",
            ",\"sequence_number\":4,\"message\":\"sensitive provider detail\"",
        );
        let output =
            String::from_utf8(collect(validated_stream(body(vec![Ok(Bytes::from(input))]))).await)
                .unwrap();
        assert_eq!(output.matches("event: response.failed").count(), 1);
        assert!(output.contains("\"sequence_number\":4"));
        assert!(output.contains("terminal provider error"));
        assert!(!output.contains("sensitive provider detail"));
    }

    #[tokio::test]
    async fn response_failed_is_rebuilt_without_untrusted_error_details() {
        let input = event(
            "response.failed",
            concat!(
                ",\"sequence_number\":8,",
                "\"response\":{",
                "\"id\":\"resp-failed\",",
                "\"model\":\"gpt-effective\",",
                "\"error\":{",
                "\"code\":\"context_length_exceeded\",",
                "\"param\":\"input[0].content\",",
                "\"message\":\"secret echoed prompt and account detail\"",
                "},",
                "\"output\":[{\"secret\":\"must not cross failure boundary\"}]",
                "}",
            ),
        );
        let output =
            String::from_utf8(collect(validated_stream(body(vec![Ok(Bytes::from(input))]))).await)
                .unwrap();
        assert_eq!(output.matches("event: response.failed").count(), 1);
        assert!(output.contains("\"sequence_number\":8"));
        assert!(output.contains("\"id\":\"resp-failed\""));
        assert!(output.contains("\"model\":\"gpt-effective\""));
        assert!(output.contains("\"code\":\"context_length_exceeded\""));
        assert!(output.contains("\"param\":\"input[0].content\""));
        assert!(!output.contains("secret echoed prompt"));
        assert!(!output.contains("must not cross failure boundary"));
    }

    #[tokio::test]
    async fn malformed_known_event_shapes_fail_closed() {
        for input in [
            event("response.output_text.delta", ",\"delta\":null"),
            event("response.output_item.done", ",\"item\":null"),
            event("response.completed", ",\"response\":{}"),
            event(
                "response.output_text.delta",
                ",\"sequence_number\":-1,\"delta\":\"bad sequence\"",
            ),
        ] {
            let output = String::from_utf8(
                collect(validated_stream(body(vec![Ok(Bytes::from(input))]))).await,
            )
            .unwrap();
            assert!(output.contains("response.failed"), "{output}");
            assert!(output.contains("muse_codex_protocol_error"), "{output}");
        }
    }

    #[test]
    fn frame_scanner_accepts_all_sse_line_endings_and_odd_offsets() {
        assert_eq!(complete_frame_len(b"data: x\n\ntrailing"), Some(9));
        assert_eq!(complete_frame_len(b"data: x\r\n\r\ntrailing"), Some(11));
        assert_eq!(complete_frame_len(b"data: x\r\rtrailing"), Some(9));
        assert_eq!(complete_frame_len(b"odd: 1234\ndata: x\n\n"), Some(19));
        assert_eq!(complete_frame_len(b"data: x\r"), None);
    }

    struct DropObservedStream {
        chunk: Option<Bytes>,
        dropped: Arc<AtomicBool>,
    }

    impl Stream for DropObservedStream {
        type Item = Result<Bytes, codex_transport::Error>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            match self.chunk.take() {
                Some(chunk) => Poll::Ready(Some(Ok(chunk))),
                None => Poll::Pending,
            }
        }
    }

    impl Drop for DropObservedStream {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn dropping_downstream_stream_cancels_the_upstream_body() {
        let dropped = Arc::new(AtomicBool::new(false));
        let upstream: RawBody = Box::pin(DropObservedStream {
            chunk: Some(Bytes::from(event(
                "response.created",
                ",\"response\":{\"id\":\"resp-cancel\"}",
            ))),
            dropped: Arc::clone(&dropped),
        });
        let mut downstream = Box::pin(validated_stream(upstream));
        assert!(downstream.next().await.is_some());
        assert!(!dropped.load(Ordering::SeqCst));
        drop(downstream);
        assert!(dropped.load(Ordering::SeqCst));
    }
}
