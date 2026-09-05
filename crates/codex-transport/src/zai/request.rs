//! Translates a stock-Muse Responses request into a Z.ai chat-completions body.
//!
//! Muse always replays complete history, so this is a pure per-request mapping
//! with no server-side state. Anything the mapping cannot express is rejected
//! rather than dropped: a silently discarded tool or image would change what
//! the model is asked to do without telling the user.

use crate::Error;
use crate::Result;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Z.ai's documented function-name grammar is `^[a-zA-Z0-9_-]+$`, max 64
/// bytes. Muse flattens namespaced tools to `namespace.tool`, so the dot has
/// to be encoded and decoded again on the way back.
const MAX_FUNCTION_NAME_BYTES: usize = 64;
const NAMESPACE_SEPARATOR: char = '.';
const ENCODED_NAMESPACE_SEPARATOR: &str = "__";

/// Reverses the wire-safe tool names and remembers which tools were freeform,
/// so the synthesized Responses stream can restore Muse's own vocabulary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToolNames {
    decoded: BTreeMap<String, String>,
    freeform: BTreeSet<String>,
}

impl ToolNames {
    /// Returns the Muse-facing name for an encoded name the model produced.
    /// An unknown name is passed through unchanged so a hallucinated tool is
    /// rejected by Muse rather than silently rewritten here.
    pub(crate) fn decode<'a>(&'a self, encoded: &'a str) -> &'a str {
        self.decoded
            .get(encoded)
            .map(String::as_str)
            .unwrap_or(encoded)
    }

    pub(crate) fn is_freeform(&self, encoded: &str) -> bool {
        self.freeform.contains(encoded)
    }

    pub(crate) fn record(&mut self, original: &str, freeform: bool) -> Result<String> {
        let encoded = encode_function_name(original)?;
        if let Some(existing) = self.decoded.get(&encoded)
            && existing != original
        {
            return Err(Error::InvalidRequest(format!(
                "tools '{existing}' and '{original}' collide after Z.ai name encoding"
            )));
        }
        self.decoded.insert(encoded.clone(), original.to_string());
        if freeform {
            self.freeform.insert(encoded.clone());
        }
        Ok(encoded)
    }
}

fn encode_function_name(original: &str) -> Result<String> {
    let encoded = original.replace(NAMESPACE_SEPARATOR, ENCODED_NAMESPACE_SEPARATOR);
    if encoded.is_empty() || encoded.len() > MAX_FUNCTION_NAME_BYTES {
        return Err(Error::InvalidRequest(format!(
            "tool name '{original}' does not fit Z.ai's {MAX_FUNCTION_NAME_BYTES}-byte limit"
        )));
    }
    if !encoded
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(Error::InvalidRequest(format!(
            "tool name '{original}' contains characters Z.ai does not accept"
        )));
    }
    Ok(encoded)
}

/// Capabilities the translator needs from the pinned catalog entry.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ModelLimits {
    pub(crate) accepts_images: bool,
    pub(crate) max_output_tokens: u64,
}

#[derive(Debug)]
pub(crate) struct TranslatedRequest {
    pub(crate) body: Value,
    pub(crate) tools: ToolNames,
}

pub(crate) fn translate(request: &Value, limits: ModelLimits) -> Result<TranslatedRequest> {
    let object = request
        .as_object()
        .ok_or_else(|| Error::InvalidRequest("the Responses request must be an object".into()))?;

    let model = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| Error::InvalidRequest("the Responses request needs a model".into()))?;

    // Muse replays full history on every turn. A stateful handle would mean
    // context this adapter cannot see, so refuse instead of dropping it.
    for field in ["previous_response_id", "conversation"] {
        if object.get(field).is_some_and(|value| !value.is_null()) {
            return Err(Error::InvalidRequest(format!(
                "{field} is not supported by the Z.ai provider"
            )));
        }
    }

    let mut tools = ToolNames::default();
    let translated_tools = translate_tools(object.get("tools"), &mut tools)?;
    let messages = translate_messages(object, &mut tools, limits)?;

    let mut body = Map::new();
    body.insert("model".into(), Value::String(model.to_string()));
    body.insert("messages".into(), Value::Array(messages));
    if !translated_tools.is_empty() {
        body.insert("tools".into(), Value::Array(translated_tools));
        // `auto` is the only tool choice Z.ai documents.
        body.insert("tool_choice".into(), Value::String("auto".into()));
        body.insert("tool_stream".into(), Value::Bool(true));
    }
    if let Some(effort) = object
        .get("reasoning")
        .and_then(Value::as_object)
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
    {
        match map_reasoning_effort(effort)? {
            Some(effort) => {
                body.insert("reasoning_effort".into(), Value::String(effort.into()));
            }
            None => {
                body.insert("thinking".into(), json!({"type": "disabled"}));
            }
        }
    }
    if let Some(max_output_tokens) = object.get("max_output_tokens").and_then(Value::as_u64) {
        let clamped = max_output_tokens.clamp(1, limits.max_output_tokens);
        body.insert("max_tokens".into(), Value::from(clamped));
    }
    body.insert("stream".into(), Value::Bool(true));

    Ok(TranslatedRequest {
        body: Value::Object(body),
        tools,
    })
}

/// Muse's efforts and Z.ai's overlap almost exactly. `ultra` is Muse's
/// proactive-delegation mode rather than a wire effort, and the pinned Codex
/// contract sends the multi-agent effort for it, so both it and `max` request
/// Z.ai's deepest documented level.
fn map_reasoning_effort(effort: &str) -> Result<Option<&'static str>> {
    Ok(match effort {
        "none" => None,
        "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "max" | "ultra" => Some("xhigh"),
        other => {
            return Err(Error::InvalidRequest(format!(
                "reasoning effort '{other}' is not supported by the Z.ai provider"
            )));
        }
    })
}

fn translate_tools(tools: Option<&Value>, names: &mut ToolNames) -> Result<Vec<Value>> {
    let Some(tools) = tools else {
        return Ok(Vec::new());
    };
    if tools.is_null() {
        return Ok(Vec::new());
    }
    let tools = tools
        .as_array()
        .ok_or_else(|| Error::InvalidRequest("tools must be an array".into()))?;

    let mut translated = Vec::with_capacity(tools.len());
    for tool in tools {
        let object = tool
            .as_object()
            .ok_or_else(|| Error::InvalidRequest("each tool must be an object".into()))?;
        match object.get("type").and_then(Value::as_str) {
            Some("function") => translated.push(function_tool(object, None, names)?),
            Some("custom") => translated.push(custom_tool(object, None, names)?),
            Some("namespace") => {
                let namespace = object
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        Error::InvalidRequest("a tool namespace needs a name".to_string())
                    })?;
                let nested = object
                    .get("tools")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        Error::InvalidRequest("a tool namespace needs a tools array".to_string())
                    })?;
                for tool in nested {
                    let tool = tool.as_object().ok_or_else(|| {
                        Error::InvalidRequest("each namespaced tool must be an object".into())
                    })?;
                    match tool.get("type").and_then(Value::as_str) {
                        Some("function") => {
                            translated.push(function_tool(tool, Some(namespace), names)?);
                        }
                        Some("custom") => {
                            translated.push(custom_tool(tool, Some(namespace), names)?);
                        }
                        other => {
                            return Err(Error::InvalidRequest(format!(
                                "tool type '{}' is not supported by the Z.ai provider",
                                other.unwrap_or("(missing)")
                            )));
                        }
                    }
                }
            }
            other => {
                return Err(Error::InvalidRequest(format!(
                    "tool type '{}' is not supported by the Z.ai provider",
                    other.unwrap_or("(missing)")
                )));
            }
        }
    }
    Ok(translated)
}

fn qualified_name(tool: &Map<String, Value>, namespace: Option<&str>) -> Result<String> {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::InvalidRequest("a tool needs a name".to_string()))?;
    Ok(match namespace {
        Some(namespace) => format!("{namespace}{NAMESPACE_SEPARATOR}{name}"),
        None => name.to_string(),
    })
}

fn function_tool(
    tool: &Map<String, Value>,
    namespace: Option<&str>,
    names: &mut ToolNames,
) -> Result<Value> {
    let original = qualified_name(tool, namespace)?;
    let encoded = names.record(&original, false)?;
    let mut function = Map::new();
    function.insert("name".into(), Value::String(encoded));
    if let Some(description) = tool.get("description").and_then(Value::as_str) {
        function.insert("description".into(), Value::String(description.to_string()));
    }
    function.insert(
        "parameters".into(),
        tool.get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    );
    Ok(json!({"type": "function", "function": Value::Object(function)}))
}

/// Z.ai has no freeform tool type. Declaring a single string parameter keeps
/// the call reachable; the synthesizer unwraps it back into Muse's freeform
/// `custom_tool_call` item.
fn custom_tool(
    tool: &Map<String, Value>,
    namespace: Option<&str>,
    names: &mut ToolNames,
) -> Result<Value> {
    let original = qualified_name(tool, namespace)?;
    let encoded = names.record(&original, true)?;
    let mut function = Map::new();
    function.insert("name".into(), Value::String(encoded));
    if let Some(description) = tool.get("description").and_then(Value::as_str) {
        function.insert("description".into(), Value::String(description.to_string()));
    }
    function.insert(
        "parameters".into(),
        json!({
            "type": "object",
            "properties": {"input": {"type": "string"}},
            "required": ["input"]
        }),
    );
    Ok(json!({"type": "function", "function": Value::Object(function)}))
}

fn translate_messages(
    request: &Map<String, Value>,
    names: &mut ToolNames,
    limits: ModelLimits,
) -> Result<Vec<Value>> {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(instructions) = request
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|instructions| !instructions.is_empty())
    {
        messages.push(json!({"role": "system", "content": instructions}));
    }

    let input = match request.get("input") {
        None | Some(Value::Null) => return Ok(messages),
        Some(Value::Array(input)) => input,
        Some(_) => return Err(Error::InvalidRequest("input must be an array".into())),
    };

    for item in input {
        let item = item
            .as_object()
            .ok_or_else(|| Error::InvalidRequest("each input item must be an object".into()))?;
        match item.get("type").and_then(Value::as_str) {
            Some("message") => messages.push(translate_message(item, limits)?),
            // Reasoning items carry provider-opaque OpenAI state. Z.ai manages
            // its own chain of thought, so replaying them would be meaningless.
            Some("reasoning") => {}
            Some("function_call") => {
                let call = tool_call(item, "arguments", names, false)?;
                push_tool_call(&mut messages, call);
            }
            Some("custom_tool_call") => {
                let call = tool_call(item, "input", names, true)?;
                push_tool_call(&mut messages, call);
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                messages.push(tool_result(item)?);
            }
            other => {
                return Err(Error::InvalidRequest(format!(
                    "input item type '{}' is not supported by the Z.ai provider",
                    other.unwrap_or("(missing)")
                )));
            }
        }
    }
    Ok(messages)
}

/// Muse records parallel calls as consecutive items. Z.ai expects them in one
/// assistant message, so fold each call into the previous one when it is still
/// an assistant tool-call message.
fn push_tool_call(messages: &mut Vec<Value>, call: Value) {
    if let Some(Value::Object(last)) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("assistant")
        && let Some(Value::Array(calls)) = last.get_mut("tool_calls")
    {
        calls.push(call);
        return;
    }
    messages.push(json!({
        "role": "assistant",
        "content": Value::Null,
        "tool_calls": [call]
    }));
}

fn tool_call(
    item: &Map<String, Value>,
    arguments_field: &str,
    names: &mut ToolNames,
    freeform: bool,
) -> Result<Value> {
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::InvalidRequest("a tool call needs a call_id".to_string()))?;
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error::InvalidRequest("a tool call needs a name".to_string()))?;
    let encoded = names.record(name, freeform)?;
    let raw = item
        .get(arguments_field)
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = if freeform {
        serde_json::to_string(&json!({"input": raw}))
            .map_err(|error| Error::InvalidRequest(error.to_string()))?
    } else if raw.is_empty() {
        "{}".to_string()
    } else {
        raw.to_string()
    };
    Ok(json!({
        "id": call_id,
        "type": "function",
        "function": {"name": encoded, "arguments": arguments}
    }))
}

fn tool_result(item: &Map<String, Value>) -> Result<Value> {
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::InvalidRequest("a tool result needs a call_id".to_string()))?;
    let content = match item.get("output") {
        Some(Value::String(output)) => output.clone(),
        Some(value) => serde_json::to_string(value)
            .map_err(|error| Error::InvalidRequest(error.to_string()))?,
        None => String::new(),
    };
    Ok(json!({"role": "tool", "tool_call_id": call_id, "content": content}))
}

fn translate_message(item: &Map<String, Value>, limits: ModelLimits) -> Result<Value> {
    let role = match item.get("role").and_then(Value::as_str) {
        // Z.ai has no separate developer role; it is a system instruction.
        Some("developer" | "system") => "system",
        Some("user") => "user",
        Some("assistant") => "assistant",
        other => {
            return Err(Error::InvalidRequest(format!(
                "message role '{}' is not supported by the Z.ai provider",
                other.unwrap_or("(missing)")
            )));
        }
    };

    let content = match item.get("content") {
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(Value::Array(parts)) => translate_content_parts(parts, role, limits)?,
        None | Some(Value::Null) => Value::String(String::new()),
        Some(_) => {
            return Err(Error::InvalidRequest(
                "message content must be a string or an array".into(),
            ));
        }
    };
    Ok(json!({"role": role, "content": content}))
}

fn translate_content_parts(parts: &[Value], role: &str, limits: ModelLimits) -> Result<Value> {
    let mut text = String::new();
    let mut multimodal: Vec<Value> = Vec::new();
    let mut has_image = false;

    for part in parts {
        let part = part
            .as_object()
            .ok_or_else(|| Error::InvalidRequest("each content part must be an object".into()))?;
        match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text" | "summary_text") => {
                let value = part.get("text").and_then(Value::as_str).unwrap_or_default();
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(value);
                multimodal.push(json!({"type": "text", "text": value}));
            }
            Some("refusal") => {
                let value = part
                    .get("refusal")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(value);
                multimodal.push(json!({"type": "text", "text": value}));
            }
            Some("input_image") => {
                if !limits.accepts_images {
                    return Err(Error::InvalidRequest(
                        "the selected Z.ai model does not accept image input".into(),
                    ));
                }
                let url = part
                    .get("image_url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        Error::InvalidRequest("an image content part needs an image_url".into())
                    })?;
                has_image = true;
                multimodal.push(json!({"type": "image_url", "image_url": {"url": url}}));
            }
            other => {
                return Err(Error::InvalidRequest(format!(
                    "content part type '{}' is not supported by the Z.ai provider",
                    other.unwrap_or("(missing)")
                )));
            }
        }
    }

    // Only user turns can legitimately carry multimodal parts; keep every other
    // role on the plain string form Z.ai documents.
    if has_image && role == "user" {
        Ok(Value::Array(multimodal))
    } else {
        Ok(Value::String(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: ModelLimits = ModelLimits {
        accepts_images: true,
        max_output_tokens: 131_072,
    };
    const TEXT_ONLY: ModelLimits = ModelLimits {
        accepts_images: false,
        max_output_tokens: 131_072,
    };

    fn muse_request() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/muse-responses-request.json"
        ))
        .expect("stock Muse request fixture")
    }

    fn tool_followup() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/muse-tool-result-followup.json"
        ))
        .expect("stock Muse tool-result fixture")
    }

    #[test]
    fn translates_the_stock_muse_request() {
        let translated = translate(&muse_request(), LIMITS).expect("translation");
        let body = translated.body;

        assert_eq!(body["model"], "gpt-fixture");
        assert_eq!(body["stream"], true);
        assert_eq!(body["tool_stream"], true);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["reasoning_effort"], "medium");
        assert_eq!(body["max_tokens"], 4096);

        // Provider-only and OpenAI-only fields never reach a third party.
        for dropped in [
            "include",
            "store",
            "metadata",
            "client_metadata",
            "parallel_tool_calls",
            "service_tier",
            "user_id",
            "request_id",
            "previous_response_id",
        ] {
            assert!(body.get(dropped).is_none(), "{dropped} was forwarded");
        }

        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "Fixture instructions");
        // The developer role has no Z.ai equivalent and becomes a system turn.
        assert_eq!(messages[1]["role"], "system");
        assert_eq!(messages[1]["content"], "Keep the harness in control.");

        let user = &messages[2];
        assert_eq!(user["role"], "user");
        let parts = user["content"].as_array().expect("multimodal user content");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(
            parts[1]["image_url"]["url"],
            "data:image/png;base64,iVBORw0KGgo="
        );

        // The reasoning item carries opaque OpenAI state and is dropped, so the
        // assistant message follows the user turn directly.
        assert_eq!(messages[3]["role"], "assistant");
        assert_eq!(messages[3]["content"], "I will call the fixture tool.");

        let call = &messages[4];
        assert_eq!(call["role"], "assistant");
        assert_eq!(call["tool_calls"][0]["id"], "call_fixture");
        assert_eq!(call["tool_calls"][0]["function"]["name"], "read_fixture");

        let result = &messages[5];
        assert_eq!(result["role"], "tool");
        assert_eq!(result["tool_call_id"], "call_fixture");
        assert_eq!(result["content"], "fixture output");

        let tools = body["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        // Namespaced tools are flattened and the dot encoded, because Z.ai
        // restricts function names to `^[a-zA-Z0-9_-]+$`.
        assert_eq!(tools[0]["function"]["name"], "muse__read_fixture");
        assert_eq!(
            translated.tools.decode("muse__read_fixture"),
            "muse.read_fixture"
        );
    }

    #[test]
    fn tool_result_followup_preserves_call_pairing_and_freeform_calls() {
        let translated = translate(&tool_followup(), TEXT_ONLY).expect("translation");
        let messages = translated.body["messages"]
            .as_array()
            .expect("messages")
            .clone();

        let call = messages
            .iter()
            .find(|message| message.get("tool_calls").is_some())
            .expect("an assistant tool call");
        assert_eq!(call["tool_calls"][0]["id"], "call_fixture");
        assert_eq!(call["tool_calls"][0]["function"]["name"], "muse__read_file");

        let results: Vec<&Value> = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_call_id"], "call_fixture");
        assert_eq!(results[1]["tool_call_id"], "custom_fixture");
        // The `name` field Muse replays on outputs has no Z.ai equivalent.
        assert!(results[0].get("name").is_none());

        // A freeform call is carried through Z.ai's only tool shape.
        let freeform = messages
            .iter()
            .filter_map(|message| message.get("tool_calls"))
            .flat_map(|calls| calls.as_array().expect("tool calls").iter())
            .find(|call| call["id"] == "custom_fixture")
            .expect("the custom tool call");
        assert_eq!(
            freeform["function"]["arguments"],
            "{\"input\":\"fixture input\"}"
        );
        assert!(translated.tools.is_freeform("exec"));
    }

    #[test]
    fn rejects_image_input_for_a_text_only_model() {
        let error = translate(&muse_request(), TEXT_ONLY)
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not accept image input"));
    }

    #[test]
    fn rejects_stateful_history_references() {
        let mut request = muse_request();
        request["previous_response_id"] = Value::String("resp_prior".into());
        assert!(translate(&request, LIMITS).is_err());
    }

    #[test]
    fn maps_ultra_and_none_reasoning_efforts() {
        let mut request = muse_request();
        request["reasoning"] = json!({"effort": "ultra"});
        let body = translate(&request, LIMITS).expect("ultra").body;
        assert_eq!(body["reasoning_effort"], "xhigh");

        request["reasoning"] = json!({"effort": "none"});
        let body = translate(&request, LIMITS).expect("none").body;
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["thinking"]["type"], "disabled");

        request["reasoning"] = json!({"effort": "sideways"});
        assert!(translate(&request, LIMITS).is_err());
    }

    #[test]
    fn clamps_max_tokens_to_the_model_limit() {
        let mut request = muse_request();
        request["max_output_tokens"] = Value::from(999_999);
        let body = translate(&request, LIMITS).expect("clamped").body;
        assert_eq!(body["max_tokens"], 131_072);
    }

    #[test]
    fn rejects_unsupported_input_items_without_echoing_their_payload() {
        let mut request = muse_request();
        request["input"] = json!([
            {"type": "local_shell_call", "id": "ls_1", "action": {"command": "cat /etc/secret"}}
        ]);
        let error = translate(&request, LIMITS).unwrap_err().to_string();
        assert!(error.contains("local_shell_call"));
        assert!(!error.contains("/etc/secret"));
    }

    #[test]
    fn rejects_tool_names_that_collide_after_encoding() {
        let mut names = ToolNames::default();
        names.record("muse.read", false).expect("first name");
        // `muse__read` and `muse.read` both encode to the same wire name, so a
        // decoded call would be ambiguous.
        assert!(names.record("muse__read", false).is_err());
    }

    #[test]
    fn rejects_tool_names_z_ai_cannot_express() {
        let mut names = ToolNames::default();
        assert!(names.record("read file", false).is_err());
        assert!(names.record(&"a".repeat(65), false).is_err());
    }

    #[test]
    fn merges_consecutive_tool_calls_into_one_assistant_message() {
        let mut request = muse_request();
        request["input"] = json!([
            {"type": "function_call", "call_id": "call_a", "name": "alpha", "arguments": "{}"},
            {"type": "function_call", "call_id": "call_b", "name": "beta", "arguments": "{}"}
        ]);
        let body = translate(&request, LIMITS).expect("parallel calls").body;
        let messages = body["messages"].as_array().expect("messages");
        let assistant = messages
            .iter()
            .find(|message| message.get("tool_calls").is_some())
            .expect("assistant tool call message");
        assert_eq!(
            assistant["tool_calls"].as_array().expect("calls").len(),
            2,
            "parallel calls belong in one assistant message"
        );
    }
}
