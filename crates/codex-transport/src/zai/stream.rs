//! Synthesizes an OpenAI Responses event stream from Z.ai chat-completions SSE.
//!
//! Stock Muse and the gateway's SSE validator both speak Responses events, so
//! the translation happens here rather than anywhere downstream. Every event
//! emitted below is in the gateway's known-event allowlist and satisfies its
//! per-type shape checks; a malformed upstream chunk becomes a terminal error
//! event instead of a truncated stream, because a bare EOF makes stock Muse
//! retry a turn whose tool calls already ran.

use super::request::ToolNames;
use crate::Error;
use crate::RawBody;
use crate::Result;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::collections::VecDeque;

const MAX_BUFFERED_BYTES: usize = 8 * 1024 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 64;

/// Wraps a Z.ai chat-completions body as a Responses event stream.
pub(crate) fn responses_stream(
    upstream: RawBody,
    model: String,
    request_key: String,
    tools: ToolNames,
) -> RawBody {
    let state = State::new(model, request_key, tools);
    Box::pin(futures::stream::unfold(
        Some((upstream, state)),
        |carry| async move {
            let (mut upstream, mut state) = carry?;
            loop {
                if let Some(frame) = state.pending.pop_front() {
                    return Some((Ok(frame), Some((upstream, state))));
                }
                if state.finished {
                    return None;
                }
                match upstream.next().await {
                    Some(Ok(chunk)) => state.push(&chunk),
                    Some(Err(error)) => state.fail(&format!("upstream stream failed: {error}")),
                    None => state.end_of_stream(),
                }
            }
        },
    ))
}

#[derive(Debug, Default)]
struct TextItem {
    id: String,
    output_index: usize,
    text: String,
}

#[derive(Debug, Default, Clone)]
struct PendingCall {
    id: String,
    output_index: usize,
    call_id: String,
    encoded_name: String,
    name: String,
    arguments: String,
    freeform: bool,
    announced: bool,
}

struct State {
    model: String,
    response_id: String,
    /// The sanitized upstream id, without the `resp_` prefix, so item ids do
    /// not read as `rs_resp_...`.
    id_stem: String,
    request_key: String,
    tools: ToolNames,
    buffer: Vec<u8>,
    pending: VecDeque<Bytes>,
    output: Vec<Value>,
    calls: Vec<PendingCall>,
    reasoning: Option<TextItem>,
    message: Option<TextItem>,
    usage: Option<Value>,
    next_output_index: usize,
    sequence: u64,
    started: bool,
    finished: bool,
    terminated: bool,
}

impl State {
    fn new(model: String, request_key: String, tools: ToolNames) -> Self {
        Self {
            model,
            response_id: String::new(),
            id_stem: String::new(),
            request_key,
            tools,
            buffer: Vec::new(),
            pending: VecDeque::new(),
            output: Vec::new(),
            calls: Vec::new(),
            reasoning: None,
            message: None,
            usage: None,
            next_output_index: 0,
            sequence: 0,
            started: false,
            finished: false,
            terminated: false,
        }
    }

    fn emit(&mut self, kind: &str, mut body: Map<String, Value>) {
        body.insert("type".into(), Value::String(kind.to_string()));
        body.insert("sequence_number".into(), Value::from(self.sequence));
        self.sequence += 1;
        let payload = Value::Object(body).to_string();
        self.pending
            .push_back(Bytes::from(format!("event: {kind}\ndata: {payload}\n\n")));
    }

    fn fail(&mut self, message: &str) {
        if self.terminated {
            self.finished = true;
            return;
        }
        let mut body = Map::new();
        body.insert("message".into(), Value::String(message.to_string()));
        body.insert("code".into(), Value::String("invalid_request".into()));
        if !self.response_id.is_empty() {
            body.insert(
                "response_id".into(),
                Value::String(self.response_id.clone()),
            );
        }
        self.emit("error", body);
        self.terminated = true;
        self.finished = true;
    }

    fn push(&mut self, chunk: &[u8]) {
        if self.terminated {
            return;
        }
        if self.buffer.len().saturating_add(chunk.len()) > MAX_BUFFERED_BYTES {
            self.fail("the Z.ai stream exceeded the buffered event limit");
            return;
        }
        self.buffer.extend_from_slice(chunk);
        while let Some(length) = complete_frame_len(&self.buffer) {
            let frame: Vec<u8> = self.buffer.drain(..length).collect();
            if let Err(error) = self.handle_frame(&frame) {
                self.fail(&error.to_string());
                return;
            }
            if self.terminated {
                return;
            }
        }
    }

    fn end_of_stream(&mut self) {
        if self.terminated {
            self.finished = true;
            return;
        }
        // A stream that stops without `[DONE]` or a finish reason is not a
        // completed turn. Emit a terminal failure so the session stays
        // resumable instead of presenting truncated output as final.
        self.fail("the Z.ai stream ended before a terminal event");
        self.finished = true;
    }

    fn handle_frame(&mut self, frame: &[u8]) -> Result<()> {
        let text = std::str::from_utf8(frame)
            .map_err(|_| Error::InvalidUpstreamResponse("the Z.ai stream was not UTF-8".into()))?;
        let mut data = String::new();
        for line in text.lines() {
            let line = line.strip_prefix('\u{feff}').unwrap_or(line);
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            if field != "data" {
                continue;
            }
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
        }
        if data.is_empty() {
            return Ok(());
        }
        if data.trim() == "[DONE]" {
            if !self.terminated {
                if self.started {
                    self.complete(None);
                } else {
                    self.fail("the Z.ai stream ended before any content arrived");
                }
            }
            self.finished = true;
            return Ok(());
        }

        let chunk: Value = serde_json::from_str(&data).map_err(|error| {
            Error::InvalidUpstreamResponse(format!("the Z.ai stream sent invalid JSON: {error}"))
        })?;
        self.handle_chunk(&chunk)
    }

    fn handle_chunk(&mut self, chunk: &Value) -> Result<()> {
        if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the Z.ai stream reported an error");
            self.fail(message);
            return Ok(());
        }

        self.start(chunk);
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            self.usage = Some(map_usage(usage));
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };

        if let Some(delta) = choice.get("delta").and_then(Value::as_object) {
            if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
                && !reasoning.is_empty()
            {
                self.push_reasoning(reasoning);
            }
            if let Some(content) = delta.get("content").and_then(Value::as_str)
                && !content.is_empty()
            {
                self.push_text(content);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.push_tool_call(call)?;
                }
            }
        }

        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty())
        {
            self.complete(Some(reason));
        }
        Ok(())
    }

    fn start(&mut self, chunk: &Value) {
        if self.started {
            return;
        }
        self.started = true;
        self.id_stem = self.derive_id_stem(chunk);
        self.response_id = format!("resp_{}", self.id_stem);
        if let Some(model) = chunk
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
        {
            self.model = model.to_string();
        }
        let response = self.response_envelope("in_progress", None, false);
        let mut created = Map::new();
        created.insert("response".into(), response.clone());
        self.emit("response.created", created);
        let mut in_progress = Map::new();
        in_progress.insert("response".into(), response);
        self.emit("response.in_progress", in_progress);
    }

    /// Ids must be stable for a given upstream response so a retry cannot
    /// produce two different identities for the same turn. Prefer Z.ai's own
    /// chunk id and fall back to the request key the caller derived.
    fn derive_id_stem(&self, chunk: &Value) -> String {
        let raw = chunk
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .unwrap_or(&self.request_key);
        sanitize_identifier(raw)
    }

    fn item_id(&self, prefix: &str, output_index: usize) -> String {
        format!("{prefix}_{}_{output_index}", self.id_stem)
    }

    fn take_output_index(&mut self) -> usize {
        let index = self.next_output_index;
        self.next_output_index += 1;
        index
    }

    fn push_reasoning(&mut self, delta: &str) {
        // Text and reasoning cannot interleave in one Responses item; a message
        // that already started means this reasoning belongs after it.
        self.close_message();
        if self.reasoning.is_none() {
            let output_index = self.take_output_index();
            let id = self.item_id("rs", output_index);
            let item = json!({
                "id": id,
                "type": "reasoning",
                "summary": []
            });
            let mut added = Map::new();
            added.insert("output_index".into(), Value::from(output_index));
            added.insert("item".into(), item);
            self.emit("response.output_item.added", added);

            let mut part = Map::new();
            part.insert("item_id".into(), Value::String(id.clone()));
            part.insert("output_index".into(), Value::from(output_index));
            part.insert("summary_index".into(), Value::from(0));
            part.insert("part".into(), json!({"type": "summary_text", "text": ""}));
            self.emit("response.reasoning_summary_part.added", part);
            self.reasoning = Some(TextItem {
                id,
                output_index,
                text: String::new(),
            });
        }
        let Some(current) = self.reasoning.as_mut() else {
            return;
        };
        current.text.push_str(delta);
        let id = current.id.clone();
        let output_index = current.output_index;
        let mut event = Map::new();
        event.insert("item_id".into(), Value::String(id));
        event.insert("output_index".into(), Value::from(output_index));
        event.insert("summary_index".into(), Value::from(0));
        event.insert("delta".into(), Value::String(delta.to_string()));
        self.emit("response.reasoning_summary_text.delta", event);
    }

    fn close_reasoning(&mut self) {
        let Some(current) = self.reasoning.take() else {
            return;
        };
        let mut done = Map::new();
        done.insert("item_id".into(), Value::String(current.id.clone()));
        done.insert("output_index".into(), Value::from(current.output_index));
        done.insert("summary_index".into(), Value::from(0));
        done.insert("text".into(), Value::String(current.text.clone()));
        self.emit("response.reasoning_summary_text.done", done);

        let summary = json!({"type": "summary_text", "text": current.text});
        let mut part_done = Map::new();
        part_done.insert("item_id".into(), Value::String(current.id.clone()));
        part_done.insert("output_index".into(), Value::from(current.output_index));
        part_done.insert("summary_index".into(), Value::from(0));
        part_done.insert("part".into(), summary.clone());
        self.emit("response.reasoning_summary_part.done", part_done);

        let item = json!({
            "id": current.id,
            "type": "reasoning",
            "summary": [summary]
        });
        let mut event = Map::new();
        event.insert("output_index".into(), Value::from(current.output_index));
        event.insert("item".into(), item.clone());
        self.emit("response.output_item.done", event);
        self.output.push(item);
    }

    fn push_text(&mut self, delta: &str) {
        self.close_reasoning();
        if self.message.is_none() {
            let output_index = self.take_output_index();
            let id = self.item_id("msg", output_index);
            let item = json!({
                "id": id,
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": []
            });
            let mut added = Map::new();
            added.insert("output_index".into(), Value::from(output_index));
            added.insert("item".into(), item);
            self.emit("response.output_item.added", added);

            let mut part = Map::new();
            part.insert("item_id".into(), Value::String(id.clone()));
            part.insert("output_index".into(), Value::from(output_index));
            part.insert("content_index".into(), Value::from(0));
            part.insert(
                "part".into(),
                json!({"type": "output_text", "text": "", "annotations": []}),
            );
            self.emit("response.content_part.added", part);
            self.message = Some(TextItem {
                id,
                output_index,
                text: String::new(),
            });
        }
        let Some(current) = self.message.as_mut() else {
            return;
        };
        current.text.push_str(delta);
        let id = current.id.clone();
        let output_index = current.output_index;
        let mut event = Map::new();
        event.insert("item_id".into(), Value::String(id));
        event.insert("output_index".into(), Value::from(output_index));
        event.insert("content_index".into(), Value::from(0));
        event.insert("delta".into(), Value::String(delta.to_string()));
        self.emit("response.output_text.delta", event);
    }

    fn close_message(&mut self) {
        let Some(current) = self.message.take() else {
            return;
        };
        let mut done = Map::new();
        done.insert("item_id".into(), Value::String(current.id.clone()));
        done.insert("output_index".into(), Value::from(current.output_index));
        done.insert("content_index".into(), Value::from(0));
        done.insert("text".into(), Value::String(current.text.clone()));
        self.emit("response.output_text.done", done);

        let part = json!({"type": "output_text", "text": current.text, "annotations": []});
        let mut part_done = Map::new();
        part_done.insert("item_id".into(), Value::String(current.id.clone()));
        part_done.insert("output_index".into(), Value::from(current.output_index));
        part_done.insert("content_index".into(), Value::from(0));
        part_done.insert("part".into(), part.clone());
        self.emit("response.content_part.done", part_done);

        let item = json!({
            "id": current.id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [part]
        });
        let mut event = Map::new();
        event.insert("output_index".into(), Value::from(current.output_index));
        event.insert("item".into(), item.clone());
        self.emit("response.output_item.done", event);
        self.output.push(item);
    }

    fn push_tool_call(&mut self, call: &Value) -> Result<()> {
        self.close_reasoning();
        self.close_message();

        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        while self.calls.len() <= index {
            self.calls.push(PendingCall::default());
        }
        let function = call.get("function").and_then(Value::as_object);

        if self.calls[index].id.is_empty() {
            let output_index = self.take_output_index();
            let entry = &mut self.calls[index];
            entry.output_index = output_index;
            entry.id = format!("fc_{}_{output_index}", self.id_stem);
        }
        if let Some(call_id) = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            && self.calls[index].call_id.is_empty()
        {
            self.calls[index].call_id = sanitize_identifier(call_id);
        }
        if let Some(name) = function
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            && self.calls[index].encoded_name.is_empty()
        {
            self.calls[index].encoded_name = name.to_string();
            self.calls[index].name = self.tools.decode(name).to_string();
            self.calls[index].freeform = self.tools.is_freeform(name);
        }

        // Nothing can be announced until the name and call id are known; Z.ai
        // sends both in the first fragment of each call.
        if !self.calls[index].announced
            && !self.calls[index].encoded_name.is_empty()
            && !self.calls[index].call_id.is_empty()
        {
            self.calls[index].announced = true;
            let entry = self.calls[index].clone();
            let item = if entry.freeform {
                json!({
                    "id": entry.id,
                    "type": "custom_tool_call",
                    "status": "in_progress",
                    "call_id": entry.call_id,
                    "name": entry.name,
                    "input": ""
                })
            } else {
                json!({
                    "id": entry.id,
                    "type": "function_call",
                    "status": "in_progress",
                    "call_id": entry.call_id,
                    "name": entry.name,
                    "arguments": ""
                })
            };
            let mut added = Map::new();
            added.insert("output_index".into(), Value::from(entry.output_index));
            added.insert("item".into(), item);
            self.emit("response.output_item.added", added);
        }

        if let Some(fragment) = function
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
            .filter(|fragment| !fragment.is_empty())
        {
            self.calls[index].arguments.push_str(fragment);
            // A freeform call arrives as JSON that wraps the real input, so it
            // can only be unwrapped once the whole argument object is present.
            if !self.calls[index].freeform && self.calls[index].announced {
                let entry = self.calls[index].clone();
                let mut event = Map::new();
                event.insert("item_id".into(), Value::String(entry.id));
                event.insert("output_index".into(), Value::from(entry.output_index));
                event.insert("delta".into(), Value::String(fragment.to_string()));
                self.emit("response.function_call_arguments.delta", event);
            }
        }
        Ok(())
    }

    fn close_tool_calls(&mut self) {
        let calls: Vec<PendingCall> = self
            .calls
            .drain(..)
            .filter(|call| call.announced)
            .collect::<Vec<_>>();
        for call in calls {
            if call.freeform {
                let input = freeform_input(&call.arguments);
                let mut delta = Map::new();
                delta.insert("item_id".into(), Value::String(call.id.clone()));
                delta.insert("output_index".into(), Value::from(call.output_index));
                delta.insert("delta".into(), Value::String(input.clone()));
                self.emit("response.custom_tool_call_input.delta", delta);

                let mut done = Map::new();
                done.insert("item_id".into(), Value::String(call.id.clone()));
                done.insert("output_index".into(), Value::from(call.output_index));
                done.insert("input".into(), Value::String(input.clone()));
                self.emit("response.custom_tool_call_input.done", done);

                let item = json!({
                    "id": call.id,
                    "type": "custom_tool_call",
                    "status": "completed",
                    "call_id": call.call_id,
                    "name": call.name,
                    "input": input
                });
                let mut event = Map::new();
                event.insert("output_index".into(), Value::from(call.output_index));
                event.insert("item".into(), item.clone());
                self.emit("response.output_item.done", event);
                self.output.push(item);
            } else {
                let arguments = if call.arguments.is_empty() {
                    "{}".to_string()
                } else {
                    call.arguments.clone()
                };
                let mut done = Map::new();
                done.insert("item_id".into(), Value::String(call.id.clone()));
                done.insert("output_index".into(), Value::from(call.output_index));
                done.insert("arguments".into(), Value::String(arguments.clone()));
                self.emit("response.function_call_arguments.done", done);

                let item = json!({
                    "id": call.id,
                    "type": "function_call",
                    "status": "completed",
                    "call_id": call.call_id,
                    "name": call.name,
                    "arguments": arguments
                });
                let mut event = Map::new();
                event.insert("output_index".into(), Value::from(call.output_index));
                event.insert("item".into(), item.clone());
                self.emit("response.output_item.done", event);
                self.output.push(item);
            }
        }
    }

    fn complete(&mut self, finish_reason: Option<&str>) {
        if self.terminated {
            return;
        }
        if !self.started {
            self.fail("the Z.ai stream ended before any content arrived");
            return;
        }
        self.close_reasoning();
        self.close_message();
        self.close_tool_calls();

        let truncated = finish_reason == Some("length");
        let status = if truncated { "incomplete" } else { "completed" };
        let response = self.response_envelope(status, finish_reason, true);
        let mut event = Map::new();
        event.insert("response".into(), response);
        if truncated {
            self.emit("response.incomplete", event);
        } else {
            self.emit("response.completed", event);
        }
        self.terminated = true;
        self.finished = true;
    }

    fn response_envelope(
        &self,
        status: &str,
        finish_reason: Option<&str>,
        include_output: bool,
    ) -> Value {
        let mut response = Map::new();
        response.insert("id".into(), Value::String(self.response_id.clone()));
        response.insert("object".into(), Value::String("response".into()));
        response.insert("status".into(), Value::String(status.to_string()));
        response.insert("model".into(), Value::String(self.model.clone()));
        response.insert(
            "output".into(),
            Value::Array(if include_output {
                self.output.clone()
            } else {
                Vec::new()
            }),
        );
        if let Some(usage) = self.usage.clone() {
            response.insert("usage".into(), usage);
        }
        if finish_reason == Some("length") {
            response.insert(
                "incomplete_details".into(),
                json!({"reason": "max_output_tokens"}),
            );
        }
        Value::Object(response)
    }
}

/// Unwraps the single string property a freeform tool is declared with. If the
/// model ignored that shape, hand the raw arguments through rather than losing
/// the call.
fn freeform_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("input")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| arguments.to_string())
}

fn map_usage(usage: &Value) -> Value {
    let input = usage.get("prompt_tokens").and_then(Value::as_u64);
    let output = usage.get("completion_tokens").and_then(Value::as_u64);
    let total = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .or_else(|| Some(input.unwrap_or(0) + output.unwrap_or(0)));
    let mut mapped = Map::new();
    if let Some(input) = input {
        mapped.insert("input_tokens".into(), Value::from(input));
    }
    if let Some(output) = output {
        mapped.insert("output_tokens".into(), Value::from(output));
    }
    if let Some(total) = total {
        mapped.insert("total_tokens".into(), Value::from(total));
    }
    Value::Object(mapped)
}

/// The gateway rejects identifiers containing control characters or whitespace,
/// so upstream ids are reduced to a conservative alphabet before use.
fn sanitize_identifier(value: &str) -> String {
    let filtered: String = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        .take(MAX_IDENTIFIER_BYTES)
        .collect();
    if filtered.is_empty() {
        "unknown".to_string()
    } else {
        filtered
    }
}

/// Returns the byte length through the first blank SSE line.
fn complete_frame_len(buffer: &[u8]) -> Option<usize> {
    let mut line_start = 0;
    let mut index = 0;
    while index < buffer.len() {
        let byte = buffer[index];
        if byte != b'\n' && byte != b'\r' {
            index += 1;
            continue;
        }
        let line_end = index;
        let next = if byte == b'\r' && buffer.get(index + 1) == Some(&b'\n') {
            index + 2
        } else {
            index + 1
        };
        if line_end == line_start {
            return Some(next);
        }
        line_start = next;
        index = next;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zai::request::ToolNames;

    /// Every kind the synthesizer can emit. The gateway rejects any event
    /// outside its own allowlist, so this list is asserted against that
    /// allowlist by a companion test in the gateway crate.
    pub(crate) const EMITTED_EVENT_KINDS: &[&str] = &[
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

    fn upstream(bytes: &'static [u8]) -> RawBody {
        // Odd chunk boundaries prove the frame reader does not rely on one
        // chunk holding one event.
        let chunks: Vec<Result<Bytes>> = bytes
            .chunks(7)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        Box::pin(futures::stream::iter(chunks))
    }

    async fn synthesize(bytes: &'static [u8], tools: ToolNames) -> Vec<Value> {
        let mut stream = responses_stream(
            upstream(bytes),
            "glm-5.3".to_string(),
            "process:1:1".to_string(),
            tools,
        );
        let mut events = Vec::new();
        while let Some(frame) = stream.next().await {
            let frame = String::from_utf8(frame.expect("frame").to_vec()).expect("utf-8");
            let (name, data) = frame.split_once('\n').expect("event line");
            let name = name.strip_prefix("event: ").expect("event name");
            let data = data
                .trim_end()
                .strip_prefix("data: ")
                .expect("data line")
                .to_string();
            let value: Value = serde_json::from_str(&data).expect("event JSON");
            assert_eq!(value["type"], name, "SSE name must match the JSON type");
            events.push(value);
        }
        events
    }

    fn kinds(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .map(|event| event["type"].as_str().expect("type").to_string())
            .collect()
    }

    fn tool_names() -> ToolNames {
        let mut names = ToolNames::default();
        names.record("muse.read_file", false).expect("tool name");
        names
    }

    #[tokio::test]
    async fn synthesizes_reasoning_text_and_a_tool_call() {
        let events = synthesize(
            include_bytes!("../../tests/fixtures/zai-stream.sse"),
            tool_names(),
        )
        .await;

        assert_eq!(
            kinds(&events),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta",
                "response.reasoning_summary_text.delta",
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
                "response.output_item.done",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );

        let completed = events.last().expect("terminal event");
        assert_eq!(completed["response"]["status"], "completed");
        assert_eq!(completed["response"]["id"], "resp_20260905fixture01");
        assert_eq!(completed["response"]["model"], "glm-5.3");
        assert_eq!(completed["response"]["usage"]["input_tokens"], 31);
        assert_eq!(completed["response"]["usage"]["output_tokens"], 12);
        assert_eq!(completed["response"]["usage"]["total_tokens"], 43);

        let output = completed["response"]["output"]
            .as_array()
            .expect("output items");
        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[0]["summary"][0]["text"], "Weighing the options.");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "Reading the file.");
        assert_eq!(output[2]["type"], "function_call");
        assert_eq!(output[2]["call_id"], "call_fixture");
        // The wire-safe name is decoded back into Muse's own vocabulary.
        assert_eq!(output[2]["name"], "muse.read_file");
        assert_eq!(output[2]["arguments"], "{\"path\":\"fixture.txt\"}");

        let sequences: Vec<u64> = events
            .iter()
            .map(|event| event["sequence_number"].as_u64().expect("sequence"))
            .collect();
        assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[tokio::test]
    async fn length_truncation_becomes_response_incomplete() {
        let events = synthesize(
            include_bytes!("../../tests/fixtures/zai-stream-length.sse"),
            ToolNames::default(),
        )
        .await;
        let terminal = events.last().expect("terminal event");
        assert_eq!(terminal["type"], "response.incomplete");
        assert_eq!(terminal["response"]["status"], "incomplete");
        assert_eq!(
            terminal["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
        // The partial message is still delivered rather than discarded.
        assert_eq!(
            terminal["response"]["output"][0]["content"][0]["text"],
            "partial"
        );
    }

    #[tokio::test]
    async fn a_mid_stream_error_becomes_a_terminal_error_event() {
        let events = synthesize(
            include_bytes!("../../tests/fixtures/zai-stream-error.sse"),
            ToolNames::default(),
        )
        .await;
        let terminal = events.last().expect("terminal event");
        assert_eq!(terminal["type"], "error");
        assert!(
            terminal["message"]
                .as_str()
                .expect("message")
                .contains("Insufficient balance")
        );
        assert!(
            !kinds(&events).contains(&"response.completed".to_string()),
            "a failed stream must never report completion"
        );
    }

    #[tokio::test]
    async fn a_stream_that_stops_early_never_reports_completion() {
        let events = synthesize(
            b"data: {\"id\":\"x1\",\"model\":\"glm-5.3\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            ToolNames::default(),
        )
        .await;
        let terminal = events.last().expect("terminal event");
        assert_eq!(terminal["type"], "error");
        assert!(
            terminal["message"]
                .as_str()
                .expect("message")
                .contains("ended before a terminal event")
        );
    }

    /// The golden is also consumed by the gateway crate, which proves the same
    /// bytes survive its SSE validator unchanged.
    #[tokio::test]
    async fn matches_the_shared_golden_stream() {
        let mut stream = responses_stream(
            upstream(include_bytes!("../../tests/fixtures/zai-stream.sse")),
            "glm-5.3".to_string(),
            "process:1:1".to_string(),
            tool_names(),
        );
        let mut synthesized = Vec::new();
        while let Some(frame) = stream.next().await {
            synthesized.extend_from_slice(&frame.expect("frame"));
        }
        assert_eq!(
            String::from_utf8(synthesized).expect("utf-8"),
            include_str!("../../tests/fixtures/zai-responses.sse")
        );
    }

    #[tokio::test]
    async fn the_done_sentinel_is_never_forwarded() {
        let events = synthesize(
            include_bytes!("../../tests/fixtures/zai-stream.sse"),
            tool_names(),
        )
        .await;
        for event in &events {
            assert_ne!(event["type"], "[DONE]");
        }
    }

    #[tokio::test]
    async fn identifiers_are_deterministic_and_protocol_safe() {
        let first = synthesize(
            include_bytes!("../../tests/fixtures/zai-stream.sse"),
            tool_names(),
        )
        .await;
        let second = synthesize(
            include_bytes!("../../tests/fixtures/zai-stream.sse"),
            tool_names(),
        )
        .await;
        assert_eq!(first, second);

        for event in &first {
            for id in [&event["response"]["id"], &event["item_id"]] {
                if let Some(id) = id.as_str() {
                    assert!(!id.is_empty() && id.len() <= 512);
                    assert!(
                        !id.chars()
                            .any(|character| character.is_control() || character.is_whitespace())
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn every_emitted_kind_is_declared() {
        for fixture in [
            include_bytes!("../../tests/fixtures/zai-stream.sse").as_slice(),
            include_bytes!("../../tests/fixtures/zai-stream-length.sse").as_slice(),
            include_bytes!("../../tests/fixtures/zai-stream-error.sse").as_slice(),
        ] {
            let events = {
                let mut stream = responses_stream(
                    Box::pin(futures::stream::iter(vec![Ok(Bytes::from_static(fixture))])),
                    "glm-5.3".to_string(),
                    "process:1:1".to_string(),
                    tool_names(),
                );
                let mut events = Vec::new();
                while let Some(frame) = stream.next().await {
                    let frame = String::from_utf8(frame.expect("frame").to_vec()).expect("utf-8");
                    let name = frame
                        .lines()
                        .next()
                        .and_then(|line| line.strip_prefix("event: "))
                        .expect("event name")
                        .to_string();
                    events.push(name);
                }
                events
            };
            for kind in events {
                assert!(
                    EMITTED_EVENT_KINDS.contains(&kind.as_str()),
                    "{kind} is emitted but not declared"
                );
            }
        }
    }
}
