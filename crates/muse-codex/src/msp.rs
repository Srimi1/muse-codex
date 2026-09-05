//! Provider-name adapter for the stock Muse Session Protocol host.
//!
//! Muse continues to own the MSP implementation. This module only translates
//! the provider identifier at fields defined by the pinned stable MSP schema.
//! It deliberately does not recursively rewrite strings: prompts, tool input,
//! tool output, errors, and extension payloads remain byte-for-byte opaque.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

const PUBLIC_PROVIDER: &str = "codex";
const INTERNAL_PROVIDER: &str = "meta";
const MAX_FRAME_BYTES: usize = 10 * 1024 * 1024;
const MAX_PENDING_REQUESTS: usize = 4096;

#[derive(Debug)]
struct PendingRequest {
    method: String,
}

type PendingRequests = Arc<Mutex<HashMap<String, PendingRequest>>>;
type SharedStdout = Arc<Mutex<io::Stdout>>;

/// Relays the caller's MSP stdio connection to a piped stock Muse child.
///
/// The caller remains responsible for waiting on and signalling the child.
/// Call [`MspRelay::finish`] after the child has exited so all provider-mapped
/// output is flushed before the launcher returns.
pub struct MspRelay {
    input: JoinHandle<Result<()>>,
    output: JoinHandle<Result<()>>,
}

impl MspRelay {
    pub fn start(child: &mut Child) -> Result<Self> {
        let child_stdin = child
            .stdin
            .take()
            .context("stock Muse MSP stdin was not piped")?;
        let child_stdout = child
            .stdout
            .take()
            .context("stock Muse MSP stdout was not piped")?;
        let child_pid = child.id();
        let pending = PendingRequests::default();
        let stdout = Arc::new(Mutex::new(io::stdout()));

        let input_pending = pending.clone();
        let input_stdout = stdout.clone();
        let input = thread::spawn(move || {
            let result = relay_client_input(child_stdin, input_pending, input_stdout);
            if result.is_err() {
                terminate_child(child_pid);
            }
            result
        });

        let output = thread::spawn(move || {
            let result = relay_server_output(child_stdout, pending, stdout);
            if result.is_err() {
                terminate_child(child_pid);
            }
            result
        });

        Ok(Self { input, output })
    }

    pub fn finish(self) -> Result<()> {
        let output_result = join_relay(self.output, "stock Muse MSP output relay")?;

        // Normally the MSP client closes stdin, which lets this thread finish
        // before the host exits. A signal can terminate the host while the
        // caller's stdin remains open; never deadlock shutdown by joining a
        // thread that is still blocked in that read.
        let input_result = if self.input.is_finished() {
            Some(join_relay(self.input, "Muse Codex MSP input relay")?)
        } else {
            None
        };

        output_result?;
        if let Some(result) = input_result {
            result?;
        }
        Ok(())
    }
}

fn join_relay(handle: JoinHandle<Result<()>>, name: &'static str) -> Result<Result<()>> {
    handle.join().map_err(|_| anyhow!("{name} thread panicked"))
}

fn relay_client_input(
    child_stdin: ChildStdin,
    pending: PendingRequests,
    stdout: SharedStdout,
) -> Result<()> {
    let stdin = io::stdin();
    let mut source = stdin.lock();
    let mut destination = BufWriter::new(child_stdin);
    let mut frame = Vec::new();

    while read_frame(&mut source, &mut frame)? {
        match adapt_client_frame(&frame, &pending)? {
            ClientFrame::Forward(bytes) => {
                destination.write_all(&bytes)?;
                destination.flush()?;
            }
            ClientFrame::Reject(bytes) => write_stdout(&stdout, &bytes)?,
            ClientFrame::Drop => {}
        }
    }
    destination.flush()?;
    Ok(())
}

fn relay_server_output(
    child_stdout: ChildStdout,
    pending: PendingRequests,
    stdout: SharedStdout,
) -> Result<()> {
    let mut source = BufReader::new(child_stdout);
    let mut frame = Vec::new();
    while read_frame(&mut source, &mut frame)? {
        let bytes = adapt_server_frame(&frame, &pending)?;
        write_stdout(&stdout, &bytes)?;
    }
    Ok(())
}

fn write_stdout(stdout: &SharedStdout, bytes: &[u8]) -> Result<()> {
    let mut destination = stdout
        .lock()
        .map_err(|_| anyhow!("MSP stdout lock was poisoned"))?;
    destination.write_all(bytes)?;
    destination.flush()?;
    Ok(())
}

/// Reads one NDJSON frame without allowing an unterminated input line to grow
/// beyond the stable MSP frame limit.
fn read_frame<R: BufRead>(reader: &mut R, output: &mut Vec<u8>) -> Result<bool> {
    output.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(!output.is_empty());
        }
        let length = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let terminated = available.get(length.saturating_sub(1)) == Some(&b'\n');
        let maximum = if terminated {
            MAX_FRAME_BYTES + 1
        } else {
            MAX_FRAME_BYTES
        };
        if output.len().saturating_add(length) > maximum {
            bail!("MSP frame exceeds the {MAX_FRAME_BYTES}-byte limit");
        }
        output.extend_from_slice(&available[..length]);
        reader.consume(length);
        if output.last() == Some(&b'\n') {
            return Ok(true);
        }
    }
}

enum ClientFrame {
    Forward(Vec<u8>),
    Reject(Vec<u8>),
    Drop,
}

fn adapt_client_frame(frame: &[u8], pending: &PendingRequests) -> Result<ClientFrame> {
    let Some(mut value) = parse_frame(frame) else {
        // Invalid JSON cannot carry an executable MSP command. Preserve it so
        // the stock host remains authoritative for parse-error behavior.
        return Ok(ClientFrame::Forward(frame.to_vec()));
    };
    let Some(object) = value.as_object_mut() else {
        return Ok(ClientFrame::Forward(frame.to_vec()));
    };
    let Some(method) = object
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(ClientFrame::Forward(frame.to_vec()));
    };

    let provider = match method.as_str() {
        "session/start" => object
            .get_mut("params")
            .and_then(Value::as_object_mut)
            .and_then(|params| params.get_mut("providerId")),
        "session/setModel" => object
            .get_mut("params")
            .and_then(Value::as_object_mut)
            .and_then(|params| params.get_mut("model"))
            .and_then(Value::as_object_mut)
            .and_then(|model| model.get_mut("providerId")),
        _ => None,
    };

    let mut changed = false;
    if let Some(Value::String(provider)) = provider {
        if provider == PUBLIC_PROVIDER {
            *provider = INTERNAL_PROVIDER.to_string();
            changed = true;
        } else if !provider.is_empty() {
            let id = object.get("id").cloned();
            let command_id = object
                .get("params")
                .and_then(Value::as_object)
                .and_then(|params| params.get("commandId"))
                .and_then(Value::as_str);
            if let Some(id) = id.filter(|id| rpc_id_key(id).is_some()) {
                let error = if let Some(command_id) = command_id {
                    command_rejected(id, command_id)
                } else {
                    invalid_params(id, "provider requires a valid commandId")
                };
                return Ok(ClientFrame::Reject(encode_frame(&error)?));
            }
            // JSON-RPC notifications receive no response. Dropping prevents a
            // malformed command-shaped notification from selecting `echo`.
            if object.get("id").is_none() {
                return Ok(ClientFrame::Drop);
            }
            return Ok(ClientFrame::Reject(encode_frame(&invalid_request(
                Value::Null,
                "request id must be a string, number, or null",
            ))?));
        }
    }

    if let Some(key) = object.get("id").and_then(rpc_id_key) {
        let mut requests = pending
            .lock()
            .map_err(|_| anyhow!("MSP pending-request lock was poisoned"))?;
        if requests.contains_key(&key) {
            let id = object.get("id").cloned().expect("key came from id");
            return Ok(ClientFrame::Reject(encode_frame(&invalid_request(
                id,
                "duplicate in-flight request id",
            ))?));
        }
        if requests.len() >= MAX_PENDING_REQUESTS {
            let id = object.get("id").cloned().expect("key came from id");
            return Ok(ClientFrame::Reject(encode_frame(&overloaded(id))?));
        }
        requests.insert(key, PendingRequest { method });
    }

    if changed {
        Ok(ClientFrame::Forward(encode_frame(&value)?))
    } else {
        Ok(ClientFrame::Forward(frame.to_vec()))
    }
}

fn command_rejected(id: Value, command_id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32030,
            "message": "muse-codex only accepts provider 'codex'",
            "data": {
                "kind": "commandRejected",
                "commandId": command_id,
                "reason": "unsupported_provider",
                "retryable": false
            }
        }
    })
}

fn invalid_params(id: Value, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32602,
            "message": message,
            "data": {"kind": "invalidParams"}
        }
    })
}

fn invalid_request(id: Value, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32600,
            "message": message,
            "data": {"kind": "invalidRequest"}
        }
    })
}

fn overloaded(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32001,
            "message": "muse-codex MSP request adapter is at capacity",
            "data": {
                "kind": "overloaded",
                "capacity": MAX_PENDING_REQUESTS,
                "retryable": true
            }
        }
    })
}

fn adapt_server_frame(frame: &[u8], pending: &PendingRequests) -> Result<Vec<u8>> {
    let Some(mut value) = parse_frame(frame) else {
        bail!("stock Muse emitted malformed MSP JSON");
    };
    let Some(object) = value.as_object_mut() else {
        bail!("stock Muse emitted a non-object MSP frame");
    };

    let mut changed = false;
    if let Some(method) = object
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        changed |= adapt_server_notification(&method, object)?;
    } else if let Some(key) = object.get("id").and_then(rpc_id_key) {
        let has_result = object.contains_key("result");
        let has_error = object.contains_key("error");
        if has_result == has_error {
            bail!("stock Muse emitted an invalid MSP response envelope");
        }

        let request = pending
            .lock()
            .map_err(|_| anyhow!("MSP pending-request lock was poisoned"))?
            .remove(&key);
        if let Some(request) = request {
            if let Some(result) = object.get_mut("result") {
                changed |= adapt_server_result(&request.method, result)?;
            }
        } else if has_result {
            // Stock may legitimately emit an uncorrelated error after the
            // adapter forwards malformed client input. A successful result
            // with no tracked request is never legitimate and could bypass
            // the provider-field mapping selected by the request method.
            bail!("stock Muse emitted a result with an unknown request id");
        }
    } else if object.contains_key("id")
        || object.contains_key("result")
        || object.contains_key("error")
    {
        bail!("stock Muse emitted an invalid MSP response envelope");
    }

    if changed {
        encode_frame(&value)
    } else {
        Ok(frame.to_vec())
    }
}

fn adapt_server_result(method: &str, result: &mut Value) -> Result<bool> {
    let Some(result) = result.as_object_mut() else {
        return Ok(false);
    };
    match method {
        "session/start" | "session/resume" | "session/fork" | "session/read" => {
            let mut changed = adapt_session(result.get_mut("session"))?;
            changed |= adapt_history(result.get_mut("history"))?;
            Ok(changed)
        }
        "session/list" => {
            let mut changed = false;
            if let Some(Value::Array(sessions)) = result.get_mut("sessions") {
                for session in sessions {
                    changed |= adapt_session(Some(session))?;
                }
            }
            Ok(changed)
        }
        "model/list" => {
            let mut changed = adapt_provider(result.get_mut("providerId"))?;
            if let Some(Value::Array(models)) = result.get_mut("models") {
                for model in models {
                    changed |= adapt_provider(
                        model
                            .as_object_mut()
                            .and_then(|model| model.get_mut("providerId")),
                    )?;
                }
            }
            Ok(changed)
        }
        "view/page" => {
            let mut changed = false;
            if let Some(Value::Array(events)) = result.get_mut("events") {
                for event in events {
                    if let Some(event) = event.as_object_mut()
                        && let Some(method) = event
                            .get("method")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    {
                        changed |= adapt_server_notification(&method, event)?;
                    }
                }
            }
            Ok(changed)
        }
        _ => Ok(false),
    }
}

fn adapt_server_notification(
    method: &str,
    object: &mut serde_json::Map<String, Value>,
) -> Result<bool> {
    let Some(params) = object.get_mut("params").and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    match method {
        "session/started" | "session/resumed" | "session/forked" => {
            adapt_session(params.get_mut("session"))
        }
        "session/modelChanged" => adapt_provider(params.get_mut("providerId")),
        _ => Ok(false),
    }
}

fn adapt_session(session: Option<&mut Value>) -> Result<bool> {
    let Some(session) = session.and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    adapt_provider(session.get_mut("providerId"))
}

fn adapt_history(history: Option<&mut Value>) -> Result<bool> {
    let Some(history) = history.and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    let Some(state) = history
        .get_mut("snapshot")
        .and_then(Value::as_object_mut)
        .and_then(|snapshot| snapshot.get_mut("state"))
        .and_then(Value::as_object_mut)
    else {
        return Ok(false);
    };
    let Some(effective_model) = state
        .get_mut("effectiveModel")
        .and_then(Value::as_object_mut)
    else {
        return Ok(false);
    };
    adapt_provider(effective_model.get_mut("providerId"))
}

fn adapt_provider(provider: Option<&mut Value>) -> Result<bool> {
    let Some(provider) = provider else {
        return Ok(false);
    };
    match provider {
        Value::Null => Ok(false),
        Value::String(value) if value == INTERNAL_PROVIDER => {
            *value = PUBLIC_PROVIDER.to_string();
            Ok(true)
        }
        Value::String(value) if value == PUBLIC_PROVIDER => Ok(false),
        Value::String(_) => bail!("stock Muse exposed an unsupported MSP provider"),
        _ => bail!("stock Muse exposed a malformed MSP provider field"),
    }
}

fn parse_frame(frame: &[u8]) -> Option<Value> {
    let frame = frame.strip_suffix(b"\n").unwrap_or(frame);
    let frame = frame.strip_suffix(b"\r").unwrap_or(frame);
    serde_json::from_slice(frame).ok()
}

fn encode_frame(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn rpc_id_key(value: &Value) -> Option<String> {
    match value {
        Value::String(_) | Value::Number(_) | Value::Null => serde_json::to_string(value).ok(),
        _ => None,
    }
}

fn terminate_child(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: kill takes an integer process id and no pointers. The id was
        // obtained from the live child owned by the launcher.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> PendingRequests {
        PendingRequests::default()
    }

    fn wire(json: &str) -> Vec<u8> {
        let mut bytes = json.as_bytes().to_vec();
        bytes.push(b'\n');
        bytes
    }

    fn decode(frame: ClientFrame) -> Value {
        let ClientFrame::Forward(frame) = frame else {
            panic!("expected forwarded frame");
        };
        parse_frame(&frame).expect("valid JSON frame")
    }

    #[test]
    fn maps_only_session_start_provider_field() {
        let requests = pending();
        let frame = wire(
            r#"{"jsonrpc":"2.0","id":1,"method":"session/start","params":{"commandId":"cmd","providerId":"codex","config":{"providerId":"codex"},"modelId":"meta-in-model"}}"#,
        );
        let value = decode(adapt_client_frame(&frame, &requests).unwrap());
        assert_eq!(value["params"]["providerId"], "meta");
        assert_eq!(value["params"]["config"]["providerId"], "codex");
        assert_eq!(value["params"]["modelId"], "meta-in-model");
    }

    #[test]
    fn maps_only_set_model_provider_field() {
        let requests = pending();
        let frame = wire(
            r#"{"jsonrpc":"2.0","id":"r","method":"session/setModel","params":{"commandId":"cmd","sessionId":"s","model":{"providerId":"codex","modelId":"meta-model","displayLabel":"meta text"}}}"#,
        );
        let value = decode(adapt_client_frame(&frame, &requests).unwrap());
        assert_eq!(value["params"]["model"]["providerId"], "meta");
        assert_eq!(value["params"]["model"]["modelId"], "meta-model");
        assert_eq!(value["params"]["model"]["displayLabel"], "meta text");
    }

    #[test]
    fn rejects_internal_or_echo_provider_with_correlated_error() {
        for provider in ["meta", "echo", "other"] {
            let requests = pending();
            let frame = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"session/start\",\"params\":{{\"commandId\":\"cmd-7\",\"providerId\":\"{provider}\"}}}}\n"
            );
            let ClientFrame::Reject(frame) =
                adapt_client_frame(frame.as_bytes(), &requests).unwrap()
            else {
                panic!("expected rejection");
            };
            let value = parse_frame(&frame).unwrap();
            assert_eq!(value["id"], 7);
            assert_eq!(value["error"]["code"], -32030);
            assert_eq!(value["error"]["data"]["kind"], "commandRejected");
            assert_eq!(value["error"]["data"]["commandId"], "cmd-7");
            assert_eq!(value["error"]["data"]["reason"], "unsupported_provider");
        }
    }

    #[test]
    fn rejects_unsupported_provider_without_command_id_as_invalid_params() {
        let requests = pending();
        let frame = wire(
            r#"{"jsonrpc":"2.0","id":"missing-command","method":"session/start","params":{"providerId":"echo"}}"#,
        );
        let ClientFrame::Reject(frame) = adapt_client_frame(&frame, &requests).unwrap() else {
            panic!("expected rejection");
        };
        let value = parse_frame(&frame).unwrap();
        assert_eq!(value["id"], "missing-command");
        assert_eq!(value["error"]["code"], -32602);
        assert_eq!(value["error"]["data"]["kind"], "invalidParams");
    }

    #[test]
    fn rejects_unsupported_provider_with_invalid_request_id() {
        let requests = pending();
        let frame = wire(
            r#"{"jsonrpc":"2.0","id":{"nested":true},"method":"session/start","params":{"commandId":"cmd","providerId":"echo"}}"#,
        );
        let ClientFrame::Reject(frame) = adapt_client_frame(&frame, &requests).unwrap() else {
            panic!("expected rejection");
        };
        let value = parse_frame(&frame).unwrap();
        assert!(value["id"].is_null());
        assert_eq!(value["error"]["code"], -32600);
        assert!(requests.lock().unwrap().is_empty());
    }

    #[test]
    fn rejects_duplicate_in_flight_ids() {
        let requests = pending();
        let first = wire(
            r#"{"jsonrpc":"2.0","id":9,"method":"session/start","params":{"commandId":"cmd","providerId":"codex"}}"#,
        );
        assert!(matches!(
            adapt_client_frame(&first, &requests).unwrap(),
            ClientFrame::Forward(_)
        ));
        let duplicate = wire(r#"{"jsonrpc":"2.0","id":9,"method":"model/list","params":{}}"#);
        let ClientFrame::Reject(frame) = adapt_client_frame(&duplicate, &requests).unwrap() else {
            panic!("expected duplicate rejection");
        };
        let value = parse_frame(&frame).unwrap();
        assert_eq!(value["error"]["code"], -32600);
        assert_eq!(value["error"]["data"]["kind"], "invalidRequest");
    }

    #[test]
    fn rejects_duplicate_id_across_adapted_and_unadapted_methods() {
        let requests = pending();
        let first = wire(
            r#"{"jsonrpc":"2.0","id":"same","method":"session/start","params":{"commandId":"cmd","providerId":"codex"}}"#,
        );
        let _ = adapt_client_frame(&first, &requests).unwrap();
        let duplicate =
            wire(r#"{"jsonrpc":"2.0","id":"same","method":"unknown/extension","params":{}}"#);
        let ClientFrame::Reject(frame) = adapt_client_frame(&duplicate, &requests).unwrap() else {
            panic!("expected duplicate rejection");
        };
        let value = parse_frame(&frame).unwrap();
        assert_eq!(value["error"]["code"], -32600);
    }

    #[test]
    fn rejects_requests_when_correlation_table_is_full() {
        let requests = pending();
        {
            let mut requests = requests.lock().unwrap();
            for index in 0..MAX_PENDING_REQUESTS {
                requests.insert(
                    index.to_string(),
                    PendingRequest {
                        method: "model/list".to_string(),
                    },
                );
            }
        }
        let frame = wire(r#"{"jsonrpc":"2.0","id":"overflow","method":"model/list","params":{}}"#);
        let ClientFrame::Reject(frame) = adapt_client_frame(&frame, &requests).unwrap() else {
            panic!("expected capacity rejection");
        };
        let value = parse_frame(&frame).unwrap();
        assert_eq!(value["error"]["code"], -32001);
        assert_eq!(value["error"]["data"]["kind"], "overloaded");
        assert_eq!(value["error"]["data"]["capacity"], MAX_PENDING_REQUESTS);
    }

    #[test]
    fn preserves_a_final_frame_without_newline() {
        let bytes = br#"{"jsonrpc":"2.0","method":"initialized"}"#;
        let mut reader = BufReader::new(io::Cursor::new(bytes));
        let mut frame = Vec::new();
        assert!(read_frame(&mut reader, &mut frame).unwrap());
        assert_eq!(frame, bytes);
        assert!(!read_frame(&mut reader, &mut frame).unwrap());
    }

    #[test]
    fn leaves_prompt_and_tool_payload_strings_unchanged() {
        let requests = pending();
        let frame = wire(
            r#"{"jsonrpc":"2.0","id":4,"method":"turn/start","params":{"input":[{"type":"text","text":"providerId meta codex echo"}],"providerId":"meta"}}"#,
        );
        let ClientFrame::Forward(forwarded) = adapt_client_frame(&frame, &requests).unwrap() else {
            panic!("expected forwarding");
        };
        assert_eq!(forwarded, frame);
    }

    #[test]
    fn maps_correlated_session_and_catalog_results() {
        let requests = pending();
        let request = wire(
            r#"{"jsonrpc":"2.0","id":1,"method":"session/start","params":{"commandId":"cmd","providerId":"codex"}}"#,
        );
        let _ = adapt_client_frame(&request, &requests).unwrap();
        let response = wire(
            r#"{"jsonrpc":"2.0","id":1,"result":{"session":{"providerId":"meta","modelId":"meta-model"}}}"#,
        );
        let response = adapt_server_frame(&response, &requests).unwrap();
        let value = parse_frame(&response).unwrap();
        assert_eq!(value["result"]["session"]["providerId"], "codex");
        assert_eq!(value["result"]["session"]["modelId"], "meta-model");

        let request = wire(r#"{"jsonrpc":"2.0","id":2,"method":"model/list","params":{}}"#);
        let _ = adapt_client_frame(&request, &requests).unwrap();
        let response = wire(
            r#"{"jsonrpc":"2.0","id":2,"result":{"providerId":"meta","models":[{"providerId":"meta","modelId":"fixture"}]}}"#,
        );
        let response = adapt_server_frame(&response, &requests).unwrap();
        let value = parse_frame(&response).unwrap();
        assert_eq!(value["result"]["providerId"], "codex");
        assert_eq!(value["result"]["models"][0]["providerId"], "codex");
    }

    #[test]
    fn maps_notification_but_not_opaque_text() {
        let requests = pending();
        let frame = wire(
            r#"{"jsonrpc":"2.0","method":"session/modelChanged","params":{"providerId":"meta","modelId":"meta-model","source":"user","note":"meta"}}"#,
        );
        let frame = adapt_server_frame(&frame, &requests).unwrap();
        let value = parse_frame(&frame).unwrap();
        assert_eq!(value["params"]["providerId"], "codex");
        assert_eq!(value["params"]["modelId"], "meta-model");
        assert_eq!(value["params"]["note"], "meta");
    }

    #[test]
    fn malformed_response_does_not_consume_pending_correlation() {
        let requests = pending();
        let request = wire(r#"{"jsonrpc":"2.0","id":1,"method":"model/list","params":{}}"#);
        let _ = adapt_client_frame(&request, &requests).unwrap();

        let missing = wire(r#"{"jsonrpc":"2.0","id":1}"#);
        assert!(adapt_server_frame(&missing, &requests).is_err());
        assert!(requests.lock().unwrap().contains_key("1"));

        let doubled =
            wire(r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-32603,"message":"bad"}}"#);
        assert!(adapt_server_frame(&doubled, &requests).is_err());
        assert!(requests.lock().unwrap().contains_key("1"));
    }

    #[test]
    fn rejects_unknown_provider_in_correlated_stock_result() {
        let requests = pending();
        let request = wire(
            r#"{"jsonrpc":"2.0","id":1,"method":"session/start","params":{"commandId":"cmd","providerId":"codex"}}"#,
        );
        let _ = adapt_client_frame(&request, &requests).unwrap();
        let response =
            wire(r#"{"jsonrpc":"2.0","id":1,"result":{"session":{"providerId":"echo"}}}"#);
        assert!(adapt_server_frame(&response, &requests).is_err());
    }

    #[test]
    fn passes_uncorrelated_stock_error_for_invalid_client_input() {
        let response =
            wire(r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#);
        assert_eq!(adapt_server_frame(&response, &pending()).unwrap(), response);
    }

    #[test]
    fn rejects_oversized_final_frame_without_newline() {
        let bytes = vec![b'x'; MAX_FRAME_BYTES + 1];
        let mut reader = BufReader::new(io::Cursor::new(bytes));
        let mut frame = Vec::new();
        assert!(read_frame(&mut reader, &mut frame).is_err());
    }

    #[test]
    fn malformed_client_json_remains_stock_muse_input() {
        let requests = pending();
        let frame = b"{not json}\n";
        let ClientFrame::Forward(forwarded) = adapt_client_frame(frame, &requests).unwrap() else {
            panic!("expected forwarding");
        };
        assert_eq!(forwarded, frame);
    }

    #[test]
    fn malformed_stock_output_fails_closed() {
        assert!(adapt_server_frame(b"{not json}\n", &pending()).is_err());
    }
}
