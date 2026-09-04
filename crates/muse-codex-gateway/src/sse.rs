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
pub fn validated_stream(
    body: RawBody,
) -> impl Stream<Item = Result<Bytes, codex_transport::Error>> + Send + 'static {
    validated_stream_with_options(body, EVENT_IDLE_TIMEOUT, MAX_EVENT_BYTES)
}

fn validated_stream_with_options(
    body: RawBody,
    idle_timeout: Duration,
    max_event_bytes: usize,
) -> impl Stream<Item = Result<Bytes, codex_transport::Error>> + Send + 'static {
    futures_util::stream::unfold(
        State::new(body, idle_timeout, max_event_bytes),
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
}

impl State {
    fn new(body: RawBody, idle_timeout: Duration, max_event_bytes: usize) -> Self {
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
                    let terminal = is_terminal_event(&event.kind);
                    self.pending.push_back(frame);
                    if terminal {
                        self.finished = true;
                        self.buffer.clear();
                    }
                }
                Ok(FrameDisposition::Drop) => {}
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
            if let Some(id) = response.get("id").and_then(Value::as_str) {
                self.response_id = Some(id.to_owned());
            }
            if let Some(model) = response.get("model").and_then(Value::as_str) {
                self.model = Some(model.to_owned());
            }
        }
    }

    fn fail(&mut self, reason: &str) {
        if self.finished {
            return;
        }
        self.pending.push_back(protocol_failure_frame(
            self.response_id.as_deref(),
            self.model.as_deref(),
            self.last_sequence.saturating_add(1),
            reason,
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
    Drop,
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
            Ok(FrameDisposition::Drop)
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

    if is_metadata_event(kind) {
        return Ok(FrameDisposition::Drop);
    }
    if !is_known_event(kind) {
        return Err("upstream emitted an unknown non-metadata event");
    }

    let sequence_number = value.get("sequence_number").and_then(Value::as_u64);
    Ok(FrameDisposition::Keep(InspectedEvent {
        kind: kind.to_owned(),
        sequence_number,
        value,
    }))
}

fn is_terminal_event(kind: &str) -> bool {
    matches!(
        kind,
        "error" | "response.completed" | "response.failed" | "response.incomplete"
    )
}

fn is_metadata_event(kind: &str) -> bool {
    matches!(kind, "response.metadata" | "codex.rate_limits")
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
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let response_id = response_id.unwrap_or("resp_muse_codex_protocol_error");
    let model = model.unwrap_or("muse-codex");
    let message = format!("muse_codex_protocol_error: {reason}; session is resumable");
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
                "code": "invalid_request",
                "message": message,
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
            "{}{}{}",
            event("response.metadata", ",\"metadata\":{}"),
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
}
