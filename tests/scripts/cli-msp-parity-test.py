#!/usr/bin/env python3
"""Deterministic stock-Muse versus muse-codex CLI/MSP compatibility gate.

The same file doubles as the private fixture gateway executable. It binds only
to loopback, uses fixed dummy credentials, and confines every profile, session,
workspace, and shell action to a mode-0700 temporary directory.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import http.server
import json
import os
import pathlib
import queue
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Iterator
from typing import Any

MODEL_ID = "fixture-model"
REPLY_TEXT = "fixture response"
TOOL_TEXT = "fixture tool complete"
TOKEN = base64.urlsafe_b64encode(bytes(range(32))).decode().rstrip("=")
ESCAPE = b"\x1b"
FAILURE_CODES = (
    "invalid_request",
    "context_length_exceeded",
    "rate_limit_exceeded",
    "invalid_api_key",
)


def model_info() -> dict[str, Any]:
    return {
        "id": MODEL_ID,
        "display_name": "Fixture Model",
        "description": "Deterministic local parity fixture",
        "context_window": 100_000,
        "max_output_tokens": 4_096,
        "supported_reasoning_efforts": ["low", "high", "xhigh", "max", "ultra"],
        "default_reasoning_effort": "low",
        "is_visible": True,
        "is_default": True,
    }


def raw_catalog() -> bytes:
    value = {
        "object": "list",
        "data": [
            {
                "id": MODEL_ID,
                "object": "model",
                "metadata": {
                    "muse-code": {
                        "release_date": "2026-01-01",
                        "is_hidden": False,
                        "limit": {"context": 100_000, "output": 4_096},
                        "attachment": True,
                        "reasoning": True,
                        "temperature": False,
                        "tool_call": True,
                    }
                },
            }
        ],
    }
    return json.dumps(value, separators=(",", ":")).encode()


def sse(event: dict[str, Any]) -> bytes:
    return ("data: " + json.dumps(event, separators=(",", ":")) + "\n\n").encode()


def response_frame(response_id: str, status: str, **extra: Any) -> dict[str, Any]:
    value: dict[str, Any] = {
        "id": response_id,
        "object": "response",
        "model": MODEL_ID,
        "status": status,
        "output": [],
    }
    value.update(extra)
    return value


def completion(text: str, response_id: str = "resp_fixture_text") -> bytes:
    return b"".join(
        [
            sse(
                {
                    "type": "response.created",
                    "sequence_number": 1,
                    "response": response_frame(response_id, "in_progress"),
                }
            ),
            sse(
                {
                    "type": "response.output_text.delta",
                    "sequence_number": 2,
                    "output_index": 0,
                    "item_id": "msg_fixture_text",
                    "content_index": 0,
                    "delta": text,
                }
            ),
            sse(
                {
                    "type": "response.completed",
                    "sequence_number": 3,
                    "response": response_frame(
                        response_id,
                        "completed",
                        usage={
                            "input_tokens": 3,
                            "output_tokens": 2,
                            "total_tokens": 5,
                        },
                    ),
                }
            ),
        ]
    )


def tool_call() -> bytes:
    return b"".join(
        [
            sse(
                {
                    "type": "response.created",
                    "sequence_number": 1,
                    "response": response_frame("resp_fixture_tool", "in_progress"),
                }
            ),
            sse(
                {
                    "type": "response.output_item.done",
                    "sequence_number": 2,
                    "output_index": 0,
                    "item": {
                        "type": "function_call",
                        "id": "fc_fixture_call",
                        "namespace": "muse",
                        "name": "bash",
                        "call_id": "call_fixture_1",
                        "arguments": json.dumps(
                            {
                                "command": "printf fixture-tool",
                                "description": "Print deterministic fixture text",
                            },
                            separators=(",", ":"),
                        ),
                    },
                }
            ),
            sse(
                {
                    "type": "response.completed",
                    "sequence_number": 3,
                    "response": response_frame(
                        "resp_fixture_tool",
                        "completed",
                        usage={
                            "input_tokens": 3,
                            "output_tokens": 2,
                            "total_tokens": 5,
                        },
                    ),
                }
            ),
        ]
    )


def failed_response(code: str) -> bytes:
    return sse(
        {
            "type": "response.failed",
            "sequence_number": 1,
            "response": {
                **response_frame("resp_fixture_failed", "failed"),
                "created_at": 0,
                "completed_at": None,
                "usage": None,
                "error": {
                    "code": code,
                    "message": f"fixture {code}",
                    "param": "input[0].content",
                },
                "previous_response_id": None,
                "metadata": {},
                "incomplete_details": None,
            },
        }
    )


class FixtureState:
    def __init__(self, catalog_status: int, request_log: pathlib.Path | None = None) -> None:
        self.catalog_status = catalog_status
        self.request_log = request_log
        self.requests: list[str] = []
        self.lock = threading.Lock()

    def record(self, body: str) -> int:
        with self.lock:
            self.requests.append(body)
            sequence = len(self.requests)
            if self.request_log is not None:
                request = json.loads(body)
                reasoning = request.get("reasoning")
                effort = reasoning.get("effort") if isinstance(reasoning, dict) else None
                descriptor = os.open(
                    self.request_log,
                    os.O_WRONLY | os.O_CREAT | os.O_APPEND,
                    0o600,
                )
                with os.fdopen(descriptor, "ab") as output:
                    output.write(
                        (
                            json.dumps(
                                {
                                    "sequence": sequence,
                                    "monotonic_ns": time.monotonic_ns(),
                                    "reasoning_effort": effort,
                                },
                                separators=(",", ":"),
                            )
                            + "\n"
                        ).encode()
                    )
            return sequence


def requested_failure_code(request: str) -> str:
    for code in FAILURE_CODES:
        if code in request:
            return code
    return "fixture_failure"


def handler_for(state: FixtureState) -> type[http.server.BaseHTTPRequestHandler]:
    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
            if self.path.split("?", 1)[0].endswith("/muse-code/models"):
                if state.catalog_status == 304:
                    self.send_response(304)
                    self.send_header("content-length", "0")
                    self.end_headers()
                else:
                    body = raw_catalog()
                    self.send_response(200)
                    self.send_header("content-type", "application/json")
                    self.send_header("content-length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                return
            self.send_error(404)

        def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
            length = int(self.headers.get("content-length", "0"))
            request = self.rfile.read(length).decode("utf-8", errors="replace")
            state.record(request)
            if "PARITY_HTTP_400" in request:
                body = json.dumps(
                    {
                        "error": {
                            "code": "invalid_request",
                            "message": "fixture HTTP 400",
                            "param": "input[0].content",
                            "type": "invalid_request_error",
                        }
                    },
                    separators=(",", ":"),
                ).encode()
                self.send_response(400)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            if "PARITY_FAILURE" in request:
                body = failed_response(requested_failure_code(request))
            elif "PARITY_TOOL" in request and "function_call_output" not in request:
                body = tool_call()
            elif "PARITY_TOOL" in request:
                body = completion(TOOL_TEXT, "resp_fixture_after_tool")
            else:
                body = completion(REPLY_TEXT)
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("cache-control", "no-cache")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    return Handler


@contextlib.contextmanager
def fixture_server(catalog_status: int = 200) -> Iterator[tuple[str, FixtureState]]:
    state = FixtureState(catalog_status)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler_for(state))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    host, port = server.server_address
    try:
        yield f"http://{host}:{port}", state
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def atomic_ready(path: pathlib.Path, base_url: str) -> None:
    ready = {
        "schema_version": 2,
        "base_url": base_url,
        "token": TOKEN,
        "default_model": MODEL_ID,
        "models": [model_info()],
    }
    temporary = path.with_name(path.name + ".fixture-tmp")
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        json.dump(ready, output, separators=(",", ":"))
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, path)
    os.chmod(path, 0o600)


def fixture_gateway(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("command")
    parser.add_argument("--bind", required=True)
    parser.add_argument("--ready-file", required=True)
    parser.add_argument("--parent-pid", required=True, type=int)
    parser.add_argument("--upstream-base-url")
    parser.add_argument("--api-key-stdin", action="store_true")
    args = parser.parse_args(argv)
    if args.command != "serve":
        return 2
    start_log = os.environ.get("MUSE_CODEX_FIXTURE_START_LOG")
    if start_log:
        descriptor = os.open(start_log, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
        with os.fdopen(descriptor, "ab") as output:
            output.write(
                (json.dumps({"monotonic_ns": time.monotonic_ns()}) + "\n").encode()
            )
    if args.api_key_stdin:
        # Consume the dummy invocation key without retaining or logging it.
        sys.stdin.buffer.readline(16 * 1024)

    host, port_text = args.bind.rsplit(":", 1)
    request_log = os.environ.get("MUSE_CODEX_FIXTURE_LOG")
    state = FixtureState(304, pathlib.Path(request_log) if request_log else None)
    server = http.server.ThreadingHTTPServer((host, int(port_text)), handler_for(state))
    actual_host, actual_port = server.server_address
    ready_path = pathlib.Path(args.ready_file)
    atomic_ready(ready_path, f"http://{actual_host}:{actual_port}")

    stopping = threading.Event()

    def stop(_signal: int, _frame: object) -> None:
        stopping.set()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    worker = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.05})
    worker.start()
    try:
        while not stopping.wait(0.05):
            try:
                os.kill(args.parent_pid, 0)
            except ProcessLookupError:
                break
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=5)
        with contextlib.suppress(FileNotFoundError):
            ready_path.unlink()
    return 0


def private_directory(path: pathlib.Path) -> None:
    path.mkdir(parents=True, mode=0o700, exist_ok=False)
    os.chmod(path, 0o700)


def base_environment(home: pathlib.Path) -> dict[str, str]:
    return {
        "HOME": str(home),
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "TMPDIR": str(home.parent / "tmp"),
        "MUSE_NO_AUTO_UPDATE": "1",
        "TBH_DISABLE_TELEMETRY": "1",
        "NO_COLOR": "1",
        "TERM": "dumb",
        "LANG": "C",
        "LC_ALL": "C",
    }


def prepare_profile(root: pathlib.Path, name: str) -> tuple[pathlib.Path, dict[str, str]]:
    profile = root / name
    private_directory(profile)
    for child in ("home", "tmp", "config", "data", "work"):
        private_directory(profile / child)
    environment = base_environment(profile / "home")
    environment["XDG_CONFIG_HOME"] = str(profile / "config")
    environment["XDG_DATA_HOME"] = str(profile / "data")
    return profile, environment


def run_checked(
    command: list[str],
    *,
    cwd: pathlib.Path,
    env: dict[str, str],
    input_bytes: bytes | None = None,
    timeout: int = 20,
) -> subprocess.CompletedProcess[bytes]:
    result = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        input=input_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
        check=False,
    )
    if result.returncode != 0:
        raise AssertionError(
            f"command failed ({result.returncode}): {command!r}\n"
            f"stdout={result.stdout.decode(errors='replace')}\n"
            f"stderr={result.stderr.decode(errors='replace')}"
        )
    return result


def run_msp_checked(
    command: list[str], *, cwd: pathlib.Path, env: dict[str, str], frames: bytes
) -> subprocess.CompletedProcess[bytes]:
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    assert process.stdin is not None and process.stdout is not None and process.stderr is not None
    stdout_chunks: list[bytes] = []
    stderr_chunks: list[bytes] = []
    received: queue.Queue[dict[str, Any]] = queue.Queue()
    observed: list[dict[str, Any]] = []

    def drain_stdout() -> None:
        for line in iter(process.stdout.readline, b""):
            stdout_chunks.append(line)
            try:
                value = json.loads(line)
            except (json.JSONDecodeError, UnicodeDecodeError):
                continue
            if isinstance(value, dict):
                received.put(value)

    def drain_stderr() -> None:
        for chunk in iter(lambda: process.stderr.read(8192), b""):
            stderr_chunks.append(chunk)

    def wait_for(predicate: Any, description: str, timeout: float = 20.0) -> dict[str, Any]:
        for value in observed:
            if predicate(value):
                return value
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"timed out waiting for MSP {description}")
            try:
                value = received.get(timeout=remaining)
            except queue.Empty as error:
                raise TimeoutError(f"timed out waiting for MSP {description}") from error
            observed.append(value)
            if predicate(value):
                return value

    stdout_thread = threading.Thread(target=drain_stdout)
    stderr_thread = threading.Thread(target=drain_stderr)
    stdout_thread.start()
    stderr_thread.start()
    failure: BaseException | None = None
    try:
        for line in frames.splitlines(keepends=True):
            value = json.loads(line)
            process.stdin.write(line)
            process.stdin.flush()
            method = value.get("method") if isinstance(value, dict) else None
            request_id = value.get("id") if isinstance(value, dict) else None
            if method is None or "id" not in value:
                continue
            response = wait_for(
                lambda frame, expected=request_id: frame.get("id") == expected,
                f"{method} response id={request_id!r}",
            )
            if method == "session/start" and "error" not in response:
                wait_for(
                    lambda frame: frame.get("method") == "session/started",
                    "session/started notification",
                )
            if method == "turn/start" and "error" not in response:
                wait_for(
                    lambda frame: frame.get("method") == "turn/completed",
                    "turn/completed notification",
                )
        process.stdin.close()
        process.wait(timeout=20)
    except BaseException as error:  # cleanup and report protocol/process evidence below
        failure = error
    finally:
        with contextlib.suppress(BrokenPipeError, OSError, ValueError):
            process.stdin.close()
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)
        stdout_thread.join(timeout=5)
        stderr_thread.join(timeout=5)

    stdout = b"".join(stdout_chunks)
    stderr = b"".join(stderr_chunks)
    if stdout_thread.is_alive() or stderr_thread.is_alive():
        raise AssertionError("MSP output drain did not terminate")
    if failure is not None:
        raise AssertionError(
            f"MSP exchange failed: {failure}\ncommand={command!r}\n"
            f"stdout={stdout.decode(errors='replace')}\n"
            f"stderr={stderr.decode(errors='replace')}"
        ) from failure
    result = subprocess.CompletedProcess(command, process.returncode, stdout, stderr)
    if result.returncode != 0:
        raise AssertionError(
            f"MSP command failed ({result.returncode}): {command!r}\n"
            f"stdout={stdout.decode(errors='replace')}\n"
            f"stderr={stderr.decode(errors='replace')}"
        )
    return result


def json_lines(data: bytes) -> list[dict[str, Any]]:
    if ESCAPE in data:
        raise AssertionError("protocol stdout contains a terminal escape byte")
    return [json.loads(line) for line in data.splitlines() if line]


def payload_types(records: list[dict[str, Any]]) -> list[str]:
    return [str(record.get("payload_type")) for record in records]


def seed_stock_endpoint(profile: pathlib.Path, base_url: str) -> None:
    config = profile / "config" / "muse"
    config.mkdir(mode=0o700)
    settings = {
        "schema_version": 1,
        "endpoint_transport": {"base_url": base_url, "auth": "bearer"},
        "provider_retry": {"max_retries": 0},
        "telemetry": {"enabled": False},
    }
    (config / "settings.json").write_text(json.dumps(settings) + "\n", encoding="utf-8")
    os.chmod(config / "settings.json", 0o600)


def wrapped_environment(
    profile: pathlib.Path, environment: dict[str, str], stock: pathlib.Path, script: pathlib.Path
) -> dict[str, str]:
    result = dict(environment)
    app_parent = profile / "app"
    private_directory(app_parent)
    result.update(
        {
            "MUSE_CODEX_HOME": str(app_parent / "muse-codex"),
            "MUSE_CODEX_MUSE_BIN": str(stock),
            "MUSE_CODEX_GATEWAY_BIN": str(script),
            "MUSE_CODEX_GATEWAY_READY_TIMEOUT_MS": "5000",
            "MUSE_CODEX_FIXTURE_LOG": str(profile / "gateway-requests.jsonl"),
            "MUSE_CODEX_FIXTURE_START_LOG": str(profile / "gateway-starts.jsonl"),
        }
    )
    result.pop("META_API_KEY", None)
    return result


def assert_exec_json(
    stock: pathlib.Path,
    wrapper: pathlib.Path,
    script: pathlib.Path,
    root: pathlib.Path,
) -> None:
    with fixture_server() as (base_url, _state):
        stock_profile, stock_env = prepare_profile(root, "stock-exec")
        stock_env["META_API_KEY"] = "fixture-not-a-secret"
        stock_result = run_checked(
            [
                str(stock),
                "exec",
                "--provider",
                "meta",
                "--base-url",
                base_url,
                "--model",
                MODEL_ID,
                "--json",
                "--no-session-log",
                "--disable-web-tools",
                "--approval-mode",
                "never",
                "--max-model-steps",
                "4",
                "--",
                "PARITY_TEXT --help",
            ],
            cwd=stock_profile / "work",
            env=stock_env,
        )
    wrapped_profile, wrapped_env_base = prepare_profile(root, "wrapped-exec")
    wrapped_env = wrapped_environment(wrapped_profile, wrapped_env_base, stock, script)
    wrapped_result = run_checked(
        [
            str(wrapper),
            "--provider",
            "codex",
            "--model",
            MODEL_ID,
            "exec",
            "--json",
            "--no-session-log",
            "--disable-web-tools",
            "--approval-mode",
            "never",
            "--max-model-steps",
            "4",
            "--",
            "PARITY_TEXT --help",
        ],
        cwd=wrapped_profile / "work",
        env=wrapped_env,
    )
    stock_records = json_lines(stock_result.stdout)
    wrapped_records = json_lines(wrapped_result.stdout)
    if payload_types(stock_records) != payload_types(wrapped_records):
        raise AssertionError("stock/wrapped exec JSON record sequence differs")
    for records in (stock_records, wrapped_records):
        prompts = [r.get("payload", {}).get("prompt") for r in records]
        if "PARITY_TEXT --help" not in prompts:
            raise AssertionError("literal -- prompt was not preserved")
        deltas = [r.get("payload", {}).get("text") for r in records]
        if REPLY_TEXT not in deltas:
            raise AssertionError("fixture response was not streamed")


def gateway_request_records(profile: pathlib.Path) -> list[dict[str, Any]]:
    path = profile / "gateway-requests.jsonl"
    if not path.exists():
        return []
    records = [json.loads(line) for line in path.read_bytes().splitlines() if line]
    if [record.get("sequence") for record in records] != list(range(1, len(records) + 1)):
        raise AssertionError("fixture gateway request log is not sequential")
    return records


def gateway_request_count(profile: pathlib.Path) -> int:
    return len(gateway_request_records(profile))


def assert_ultra_effort(
    stock: pathlib.Path,
    wrapper: pathlib.Path,
    script: pathlib.Path,
    root: pathlib.Path,
) -> None:
    def run_effort(effort: str) -> tuple[subprocess.CompletedProcess[bytes], dict[str, Any]]:
        profile, environment_base = prepare_profile(root, f"wrapped-effort-{effort}")
        environment = wrapped_environment(profile, environment_base, stock, script)
        # A caller-controlled false value must not close the compatibility gate.
        environment["MUSE_EXPERIMENTAL_ULTRA_REASONING_EFFORT"] = "0"
        result = run_checked(
            [
                str(wrapper),
                "--provider",
                "codex",
                "--model",
                MODEL_ID,
                "--reasoning-effort",
                effort,
                "exec",
                "--json",
                "--no-session-log",
                "--disable-web-tools",
                "--approval-mode",
                "never",
                "--max-model-steps",
                "1",
                "PARITY_TEXT EFFORT",
            ],
            cwd=profile / "work",
            env=environment,
        )
        requests = gateway_request_records(profile)
        if len(requests) != 1:
            raise AssertionError(f"{effort} produced {len(requests)} fixture requests")
        return result, requests[0]

    ultra_result, ultra_request = run_effort("ultra")
    xhigh_result, xhigh_request = run_effort("xhigh")
    if b"gate ultra_reasoning_effort is closed" in ultra_result.stderr:
        raise AssertionError("Muse Codex left the stock Ultra compatibility gate closed")
    if ultra_request.get("reasoning_effort") != "xhigh":
        raise AssertionError("Ultra did not use the pinned model's xhigh wire effort")
    if xhigh_request.get("reasoning_effort") != "xhigh":
        raise AssertionError("xhigh did not preserve its wire effort")
    if b"gate ultra_reasoning_effort is closed" in xhigh_result.stderr:
        raise AssertionError("xhigh unexpectedly emitted an Ultra gate warning")


def gateway_start_count(profile: pathlib.Path) -> int:
    path = profile / "gateway-starts.jsonl"
    if not path.exists():
        return 0
    return sum(1 for line in path.read_bytes().splitlines() if line)


def exec_command(
    binary: pathlib.Path,
    prompt: str,
    *,
    provider: str,
    base_url: str | None = None,
) -> list[str]:
    arguments = [str(binary)]
    if provider == "codex":
        # Global flags before the subcommand exercise the launcher's canonical
        # exec dispatch without changing the public option shape.
        arguments.extend(["--provider", provider, "--model", MODEL_ID, "exec"])
    else:
        arguments.extend(["exec", "--provider", provider])
        if base_url is not None:
            arguments.extend(["--base-url", base_url])
        arguments.extend(["--model", MODEL_ID])
    arguments.extend(
        [
            "--json",
            "--no-session-log",
            "--disable-web-tools",
            "--approval-mode",
            "never",
            "--max-model-steps",
            "4",
            "--",
            prompt,
        ]
    )
    return arguments


def assert_exec_tool_loop(
    stock: pathlib.Path,
    wrapper: pathlib.Path,
    script: pathlib.Path,
    root: pathlib.Path,
) -> None:
    with fixture_server() as (base_url, state):
        stock_profile, stock_env = prepare_profile(root, "stock-exec-tool")
        stock_env["META_API_KEY"] = "fixture-not-a-secret"
        seed_stock_endpoint(stock_profile, base_url)
        stock_result = run_checked(
            exec_command(stock, "PARITY_TOOL", provider="meta", base_url=base_url),
            cwd=stock_profile / "work",
            env=stock_env,
        )
        stock_request_count = len(state.requests)
        if stock_request_count != 2 or "function_call_output" not in state.requests[1]:
            raise AssertionError("stock Muse did not complete exactly one fixture tool loop")

    wrapped_profile, wrapped_env_base = prepare_profile(root, "wrapped-exec-tool")
    wrapped_env = wrapped_environment(wrapped_profile, wrapped_env_base, stock, script)
    wrapped_result = run_checked(
        exec_command(wrapper, "PARITY_TOOL", provider="codex"),
        cwd=wrapped_profile / "work",
        env=wrapped_env,
    )
    if gateway_request_count(wrapped_profile) != 2:
        raise AssertionError("wrapped Muse did not complete exactly one fixture tool loop")

    stock_records = json_lines(stock_result.stdout)
    wrapped_records = json_lines(wrapped_result.stdout)
    if payload_types(stock_records) != payload_types(wrapped_records):
        raise AssertionError("stock/wrapped tool-loop JSON record sequence differs")
    for records in (stock_records, wrapped_records):
        deltas = [record.get("payload", {}).get("text") for record in records]
        if TOOL_TEXT not in deltas:
            raise AssertionError("fixture tool result was not followed by the final response")
        serialized = json.dumps(records, separators=(",", ":"))
        if "fixture-tool" not in serialized:
            raise AssertionError("fixture shell output is absent from the event stream")


def run_failure(
    command: list[str], *, cwd: pathlib.Path, env: dict[str, str]
) -> tuple[subprocess.CompletedProcess[bytes], float]:
    started = time.monotonic()
    result = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=12,
        check=False,
    )
    return result, time.monotonic() - started


def assert_terminal_failure(result: subprocess.CompletedProcess[bytes], label: str) -> list[str]:
    records = json_lines(result.stdout)
    types = payload_types(records)
    if "run.terminal.failed" not in types:
        raise AssertionError(
            f"{label} did not leave a terminal failed event (rc={result.returncode}): "
            f"{types!r}; stderr={result.stderr.decode(errors='replace')!r}"
        )
    return types


def assert_failure_retry_bounds(
    stock: pathlib.Path,
    wrapper: pathlib.Path,
    script: pathlib.Path,
    root: pathlib.Path,
) -> None:
    cases = [(f"PARITY_FAILURE {code}", code) for code in FAILURE_CODES]
    cases.append(("PARITY_HTTP_400", "http_400"))
    for prompt, label in cases:
        with fixture_server() as (base_url, state):
            stock_profile, stock_env = prepare_profile(root, f"stock-failure-{label}")
            stock_env["META_API_KEY"] = "fixture-not-a-secret"
            seed_stock_endpoint(stock_profile, base_url)
            stock_result, stock_duration = run_failure(
                exec_command(stock, prompt, provider="meta", base_url=base_url),
                cwd=stock_profile / "work",
                env=stock_env,
            )
            stock_count = len(state.requests)

        wrapped_profile, wrapped_env_base = prepare_profile(root, f"wrapped-failure-{label}")
        wrapped_env = wrapped_environment(wrapped_profile, wrapped_env_base, stock, script)
        wrapped_result, wrapped_duration = run_failure(
            exec_command(wrapper, prompt, provider="codex"),
            cwd=wrapped_profile / "work",
            env=wrapped_env,
        )
        wrapped_count = gateway_request_count(wrapped_profile)
        stock_types = assert_terminal_failure(stock_result, f"stock {label}")
        wrapped_types = assert_terminal_failure(wrapped_result, f"wrapped {label}")
        if stock_types != wrapped_types:
            raise AssertionError(f"stock/wrapped failure event sequence differs for {label}")
        if stock_count != 1 or wrapped_count != 1:
            raise AssertionError(
                f"{label} retried outside the gateway: stock={stock_count}, wrapped={wrapped_count}"
            )
        if stock_duration >= 8 or wrapped_duration >= 8:
            raise AssertionError(
                f"{label} exceeded local failure bound: "
                f"stock={stock_duration:.3f}s, wrapped={wrapped_duration:.3f}s"
            )


def msp_frames(workspace: pathlib.Path, public_provider: str) -> bytes:
    session_id = "0198f0aa-1111-7000-8000-0000000000aa"
    frames = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "muse_codex_parity", "version": "0.0.0"}},
        },
        {"jsonrpc": "2.0", "method": "initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "model/list", "params": {}},
        {
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/start",
            "params": {
                "commandId": "0198f0ab-9999-7000-8000-0000000000c1",
                "sessionId": session_id,
                "workspaceRoot": str(workspace),
                "providerId": public_provider,
                "modelId": MODEL_ID,
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 4,
            "method": "turn/start",
            "params": {
                "sessionId": session_id,
                "commandId": "0198f0ab-9999-7000-8000-0000000000c2",
                "input": [{"type": "text", "text": "PARITY_TEXT MSP"}],
            },
        },
    ]
    return b"".join(
        (json.dumps(frame, separators=(",", ":")) + "\n").encode() for frame in frames
    )


def msp_resume_fork_frames(session_id: str) -> bytes:
    frames = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "muse_codex_resume", "version": "0.0.0"}},
        },
        {"jsonrpc": "2.0", "method": "initialized"},
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/resume",
            "params": {
                "commandId": "0198f0ab-9999-7000-8000-0000000000e1",
                "sessionId": session_id,
                "history": "inline",
            },
        },
        {
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/fork",
            "params": {
                "commandId": "0198f0ab-9999-7000-8000-0000000000e2",
                "sessionId": session_id,
                "excludeItems": False,
            },
        },
    ]
    return b"".join(
        (json.dumps(frame, separators=(",", ":")) + "\n").encode() for frame in frames
    )


def assert_resume_fork_result(
    result: subprocess.CompletedProcess[bytes], expected_provider: str, session_id: str
) -> None:
    frames = json_lines(result.stdout)
    resume = next(frame for frame in frames if frame.get("id") == 2).get("result")
    fork = next(frame for frame in frames if frame.get("id") == 3).get("result")
    if not isinstance(resume, dict) or not isinstance(fork, dict):
        raise AssertionError("MSP resume/fork did not return successful result envelopes")
    if resume["session"]["sessionId"] != session_id:
        raise AssertionError("MSP resume returned the wrong session")
    if resume["session"]["providerId"] != expected_provider:
        raise AssertionError("MSP resume returned the wrong provider")
    if fork["session"]["providerId"] != expected_provider:
        raise AssertionError("MSP fork returned the wrong provider")
    provenance = fork["session"].get("forkedFrom")
    if not isinstance(provenance, dict) or provenance.get("sessionId") != session_id:
        raise AssertionError("MSP fork did not preserve source-session provenance")
    for label, envelope in (("resume", resume), ("fork", fork)):
        history = envelope.get("history")
        if not isinstance(history, dict) or history.get("mode") != "inline":
            raise AssertionError(f"MSP {label} did not return inline durable history")
        serialized = json.dumps(history, separators=(",", ":"))
        if "PARITY_TEXT MSP" not in serialized or REPLY_TEXT not in serialized:
            raise AssertionError(f"MSP {label} lost completed turn history")


def assert_msp(
    stock: pathlib.Path,
    wrapper: pathlib.Path,
    script: pathlib.Path,
    root: pathlib.Path,
) -> None:
    session_id = "0198f0aa-1111-7000-8000-0000000000aa"
    with fixture_server() as (base_url, _state):
        stock_profile, stock_env = prepare_profile(root, "stock-msp")
        stock_env["META_API_KEY"] = "fixture-not-a-secret"
        seed_stock_endpoint(stock_profile, base_url)
        stock_result = run_msp_checked(
            [str(stock), "serve"],
            cwd=stock_profile / "work",
            env=stock_env,
            frames=msp_frames(stock_profile / "work", "meta"),
        )
        stock_resume_fork = run_msp_checked(
            [str(stock), "serve"],
            cwd=stock_profile / "work",
            env=stock_env,
            frames=msp_resume_fork_frames(session_id),
        )

    wrapped_profile, wrapped_env_base = prepare_profile(root, "wrapped-msp")
    wrapped_env = wrapped_environment(wrapped_profile, wrapped_env_base, stock, script)
    wrapped_result = run_msp_checked(
        [str(wrapper), "serve"],
        cwd=wrapped_profile / "work",
        env=wrapped_env,
        frames=msp_frames(wrapped_profile / "work", "codex"),
    )
    wrapped_resume_fork = run_msp_checked(
        [str(wrapper), "serve"],
        cwd=wrapped_profile / "work",
        env=wrapped_env,
        frames=msp_resume_fork_frames(session_id),
    )
    stock_frames = json_lines(stock_result.stdout)
    wrapped_frames = json_lines(wrapped_result.stdout)
    assert_resume_fork_result(stock_resume_fork, "meta", session_id)
    assert_resume_fork_result(wrapped_resume_fork, "codex", session_id)

    def methods(frames: list[dict[str, Any]]) -> list[str]:
        return [str(frame["method"]) for frame in frames if "method" in frame]

    if methods(stock_frames) != methods(wrapped_frames):
        raise AssertionError("stock/wrapped MSP notification sequence differs")
    stock_init = next(frame for frame in stock_frames if frame.get("id") == 1)["result"]
    wrapped_init = next(frame for frame in wrapped_frames if frame.get("id") == 1)["result"]
    for key in ("serverInfo", "platformFamily", "platformOs", "schema", "sessionDurability"):
        if stock_init[key] != wrapped_init[key]:
            raise AssertionError(f"MSP initialize field differs: {key}")
    wrapped_models = next(frame for frame in wrapped_frames if frame.get("id") == 2)["result"]
    if wrapped_models["providerId"] != "codex":
        raise AssertionError("model/list leaked the internal provider")
    if any(model["providerId"] != "codex" for model in wrapped_models["models"]):
        raise AssertionError("model/list row leaked the internal provider")
    wrapped_session = next(frame for frame in wrapped_frames if frame.get("id") == 3)["result"][
        "session"
    ]
    if wrapped_session["providerId"] != "codex":
        raise AssertionError("session/start leaked the internal provider")
    if not any(
        frame.get("method") == "item/delta"
        and frame.get("params", {}).get("delta") == REPLY_TEXT
        for frame in wrapped_frames
    ):
        summary = [
            {
                "id": frame.get("id"),
                "method": frame.get("method"),
                "result": frame.get("result"),
                "error": frame.get("error"),
            }
            for frame in wrapped_frames
        ]
        raise AssertionError(
            f"MSP turn did not stream fixture text: {summary!r}; "
            f"stderr={wrapped_result.stderr.decode(errors='replace')!r}"
        )

    reject_profile, reject_env_base = prepare_profile(root, "wrapped-msp-reject")
    reject_env = wrapped_environment(reject_profile, reject_env_base, stock, script)
    reject_frames = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "muse_codex_reject", "version": "0.0.0"}},
        },
        {"jsonrpc": "2.0", "method": "initialized"},
    ]
    for index, provider in enumerate(("echo", "meta"), start=2):
        reject_frames.append(
            {
                "jsonrpc": "2.0",
                "id": index,
                "method": "session/start",
                "params": {
                    "commandId": f"0198f0ab-9999-7000-8000-0000000000d{index}",
                    "workspaceRoot": str(reject_profile / "work"),
                    "providerId": provider,
                },
            }
        )
    reject_result = run_msp_checked(
        [str(wrapper), "serve"],
        cwd=reject_profile / "work",
        env=reject_env,
        frames=b"".join(
            (json.dumps(frame, separators=(",", ":")) + "\n").encode()
            for frame in reject_frames
        ),
    )
    responses = json_lines(reject_result.stdout)
    for request_id in (2, 3):
        error = next(frame for frame in responses if frame.get("id") == request_id)["error"]
        if error["code"] != -32030 or error["data"]["reason"] != "unsupported_provider":
            raise AssertionError("unsupported MSP provider was not rejected")


def assert_malformed_cli(stock: pathlib.Path, wrapper: pathlib.Path, script: pathlib.Path, root: pathlib.Path) -> None:
    profile, environment_base = prepare_profile(root, "wrapped-malformed")
    environment = wrapped_environment(profile, environment_base, stock, script)
    # `--help` makes this a credential-free parser probe. Stock must remain the
    # authority for non-provider argument diagnostics.
    cases = (["exec", "--bad", "--help"], ["exec", "--model", "--help"], ["--bad", "--help"])
    for arguments in cases:
        result = subprocess.run(
            [str(wrapper), *arguments],
            cwd=profile / "work",
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
            check=False,
        )
        if b"Codex model catalog" in result.stderr or b"login" in result.stderr.lower():
            raise AssertionError(f"parser probe unexpectedly required authentication: {arguments!r}")

    starts_before_invalid_serve = gateway_start_count(profile)
    serve = subprocess.run(
        [str(wrapper), "serve", "--no-session-log"],
        cwd=profile / "work",
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=10,
        check=False,
    )
    if serve.returncode == 0 or b"cannot deliver MSP turn events" not in serve.stderr:
        raise AssertionError("serve --no-session-log was not rejected with a protocol diagnostic")
    if gateway_start_count(profile) != starts_before_invalid_serve:
        raise AssertionError("invalid MSP serve options started the provider gateway")


def assert_information_compatibility(
    stock: pathlib.Path,
    wrapper: pathlib.Path,
    script: pathlib.Path,
    root: pathlib.Path,
) -> None:
    profile, environment_base = prepare_profile(root, "wrapped-information")
    environment = wrapped_environment(profile, environment_base, stock, script)
    root_help = run_checked(
        [str(wrapper), "--help"],
        cwd=profile / "work",
        env=environment,
    ).stdout.decode(errors="replace")
    if "Startup provider: codex (default: codex)" not in root_help:
        raise AssertionError("root help does not advertise the Codex provider")
    for provider_text in ("echo or meta", "Meta provider", "echo provider only"):
        if provider_text in root_help:
            raise AssertionError(f"root help leaked stock provider wording: {provider_text!r}")

    serve_help = run_checked(
        [str(wrapper), "serve", "--help"],
        cwd=profile / "work",
        env=environment,
    ).stdout.decode(errors="replace")
    if "Unavailable for MSP turns with the pinned Muse 1.0.3 host" not in serve_help:
        raise AssertionError("serve help does not explain the session-log compatibility limit")
    if "Use memory-only sessions" in serve_help:
        raise AssertionError("serve help still advertises the broken memory-only MSP mode")
    if gateway_start_count(profile) != 0:
        raise AssertionError("auth-free help unexpectedly started the provider gateway")

    stock_profile, stock_env = prepare_profile(root, "stock-schema")
    wrapped_profile, wrapped_env_base = prepare_profile(root, "wrapped-schema")
    wrapped_env = wrapped_environment(wrapped_profile, wrapped_env_base, stock, script)
    stock_out = stock_profile / "work" / "schema"
    wrapped_out = wrapped_profile / "work" / "schema"
    run_checked(
        [str(stock), "schema", "generate-json-schema", "--out", str(stock_out)],
        cwd=stock_profile / "work",
        env=stock_env,
    )
    run_checked(
        [str(wrapper), "schema", "generate-json-schema", "--out", str(wrapped_out)],
        cwd=wrapped_profile / "work",
        env=wrapped_env,
    )
    if (stock_out / "msp.schema.json").read_bytes() != (
        wrapped_out / "msp.schema.json"
    ).read_bytes():
        raise AssertionError("wrapper changed the stable MSP schema bytes")
    if gateway_start_count(wrapped_profile) != 0:
        raise AssertionError("auth-free schema export unexpectedly started the provider gateway")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--stock",
        default=os.environ.get(
            "MUSE_CODEX_MUSE_BIN", str(pathlib.Path.home() / ".local/bin/muse-bin-1.0.3-R2198.1")
        ),
    )
    parser.add_argument(
        "--wrapper",
        default=os.environ.get("MUSE_CODEX_BIN", "target/debug/muse-codex"),
    )
    args = parser.parse_args()
    stock = pathlib.Path(args.stock).resolve()
    wrapper = pathlib.Path(args.wrapper).resolve()
    script = pathlib.Path(__file__).resolve()
    if not os.access(stock, os.X_OK) or not os.access(wrapper, os.X_OK):
        parser.error("--stock and --wrapper must name executable files")
    version = subprocess.run(
        [str(stock), "--version"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False
    )
    if version.stdout.decode().strip() != "Muse Code 1.0.3 (1.0.3-R2198.1)":
        parser.error("--stock is not Muse 1.0.3-R2198.1")

    root = pathlib.Path(tempfile.mkdtemp(prefix="muse-codex-cli-msp-parity."))
    os.chmod(root, 0o700)
    try:
        assert_information_compatibility(stock, wrapper, script, root)
        assert_exec_json(stock, wrapper, script, root)
        assert_ultra_effort(stock, wrapper, script, root)
        assert_exec_tool_loop(stock, wrapper, script, root)
        assert_failure_retry_bounds(stock, wrapper, script, root)
        assert_msp(stock, wrapper, script, root)
        assert_malformed_cli(stock, wrapper, script, root)
    finally:
        shutil.rmtree(root)
    print("Muse Codex deterministic CLI/MSP parity: ok")
    return 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "serve":
        raise SystemExit(fixture_gateway(sys.argv[1:]))
    raise SystemExit(main())
