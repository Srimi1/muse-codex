#!/usr/bin/env python3
"""Opt-in live ChatGPT-subscription smoke test for the Muse MSP adapter.

This intentionally refuses API-key authentication. It creates one uniquely
identified Muse Codex session in the existing isolated profile, uses a private
temporary workspace, restarts the MSP host, and resumes only that session.
Protocol frames, prompts, account details, and credentials are never printed.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import pathlib
import queue
import secrets
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from typing import Any


SECRET_OR_BILLING_ENV = (
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
    "META_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_ORGANIZATION",
    "OPENAI_PROJECT",
    "TBH_AUTH_BASE_URL",
    "TBH_MINT_BASE_URL",
    "MUSE_CUSTOM_HEADERS",
)
MAX_PROTOCOL_BYTES = 10 * 1024 * 1024


class SmokeError(RuntimeError):
    pass


def uuid7() -> str:
    timestamp = int(time.time() * 1000) & ((1 << 48) - 1)
    random_bits = int.from_bytes(os.urandom(10), "big")
    value = timestamp << 80
    value |= 0x7 << 76
    value |= ((random_bits >> 68) & 0xFFF) << 64
    value |= 0b10 << 62
    value |= random_bits & ((1 << 62) - 1)
    return str(uuid.UUID(int=value))


def clean_environment(stock: str | None, gateway: str | None) -> dict[str, str]:
    environment = dict(os.environ)
    for name in SECRET_OR_BILLING_ENV:
        environment.pop(name, None)
    environment.update(
        {
            "MUSE_NO_AUTO_UPDATE": "1",
            "TBH_DISABLE_TELEMETRY": "1",
            "NO_COLOR": "1",
            "TERM": "dumb",
        }
    )
    if stock is not None:
        environment["MUSE_CODEX_MUSE_BIN"] = str(pathlib.Path(stock).resolve())
    if gateway is not None:
        environment["MUSE_CODEX_GATEWAY_BIN"] = str(pathlib.Path(gateway).resolve())
    return environment


class MspClient:
    def __init__(
        self,
        wrapper: pathlib.Path,
        environment: dict[str, str],
        workspace: pathlib.Path,
        timeout: float,
    ) -> None:
        self.timeout = timeout
        self.deadline = time.monotonic() + timeout
        self.next_id = 1
        self.records: list[dict[str, Any]] = []
        self.received: queue.Queue[dict[str, Any]] = queue.Queue()
        self.stderr_bytes = 0
        self.stdout_bytes = 0
        self.protocol_error: str | None = None
        self.process = subprocess.Popen(
            [
                str(wrapper),
                "serve",
                "--disable-write",
                "--disable-shell",
                "--sandbox-network",
                "restricted",
            ],
            cwd=workspace,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        if self.process.stdin is None or self.process.stdout is None or self.process.stderr is None:
            raise SmokeError("failed to open MSP pipes")
        self.output_thread = threading.Thread(target=self._read_output, daemon=True)
        self.error_thread = threading.Thread(target=self._read_errors, daemon=True)
        self.output_thread.start()
        self.error_thread.start()

    def _read_output(self) -> None:
        assert self.process.stdout is not None
        while True:
            line = self.process.stdout.readline(MAX_PROTOCOL_BYTES + 2)
            if not line:
                return
            self.stdout_bytes += len(line)
            if len(line) > MAX_PROTOCOL_BYTES + 1 or self.stdout_bytes > MAX_PROTOCOL_BYTES:
                self.protocol_error = "MSP output exceeded the 10 MiB smoke-test bound"
                with contextlib.suppress(ProcessLookupError):
                    self.process.terminate()
                return
            try:
                frame = json.loads(line)
            except (json.JSONDecodeError, UnicodeDecodeError):
                self.protocol_error = "MSP host emitted invalid JSON"
                with contextlib.suppress(ProcessLookupError):
                    self.process.terminate()
                return
            if not isinstance(frame, dict):
                self.protocol_error = "MSP host emitted a non-object frame"
                with contextlib.suppress(ProcessLookupError):
                    self.process.terminate()
                return
            self.received.put(frame)

    def _read_errors(self) -> None:
        assert self.process.stderr is not None
        for chunk in iter(lambda: self.process.stderr.read(8192), b""):
            self.stderr_bytes += len(chunk)

    def _send(self, frame: dict[str, Any]) -> None:
        assert self.process.stdin is not None
        encoded = (json.dumps(frame, separators=(",", ":")) + "\n").encode()
        try:
            self.process.stdin.write(encoded)
            self.process.stdin.flush()
        except (BrokenPipeError, OSError) as error:
            raise SmokeError("MSP host closed its input") from error

    def wait_for(self, predicate: Any, description: str) -> dict[str, Any]:
        for frame in self.records:
            if predicate(frame):
                return frame
        while True:
            if self.protocol_error is not None:
                raise SmokeError(self.protocol_error)
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                raise SmokeError(f"timed out waiting for {description}")
            try:
                frame = self.received.get(timeout=min(remaining, 0.5))
            except queue.Empty:
                if self.process.poll() is not None:
                    raise SmokeError(f"MSP host exited while waiting for {description}")
                continue
            self.records.append(frame)
            if frame.get("method") == "approval/requested":
                raise SmokeError("unexpected approval request; no approval was granted")
            if predicate(frame):
                return frame

    def request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        request_id = self.next_id
        self.next_id += 1
        self._send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            }
        )
        response = self.wait_for(
            lambda frame: frame.get("id") == request_id,
            f"{method} response",
        )
        if "error" in response:
            error = response.get("error")
            code = error.get("code") if isinstance(error, dict) else "unknown"
            raise SmokeError(f"{method} failed with protocol code {code}")
        result = response.get("result")
        if not isinstance(result, dict):
            raise SmokeError(f"{method} returned a malformed result")
        return result

    def initialize(self) -> None:
        result = self.request(
            "initialize",
            {"clientInfo": {"name": "muse_codex_live_smoke", "version": "0.1.0"}},
        )
        schema = result.get("schema")
        if not isinstance(schema, dict) or schema.get("version") != 1:
            raise SmokeError("MSP host did not negotiate stable schema v1")
        self._send({"jsonrpc": "2.0", "method": "initialized"})

    def run_turn(self, session_id: str, prompt: str) -> tuple[int, dict[str, Any]]:
        start_index = len(self.records)
        result = self.request(
            "turn/start",
            {
                "sessionId": session_id,
                "commandId": uuid7(),
                "input": [{"type": "text", "text": prompt}],
            },
        )
        turn_id = result.get("turnId")
        if not isinstance(turn_id, str):
            raise SmokeError("turn/start omitted turnId")
        terminal = self.wait_for(
            lambda frame: frame.get("method") == "turn/completed"
            and frame.get("params", {}).get("turnId") == turn_id,
            "turn/completed",
        )
        return start_index, terminal

    def close(self) -> None:
        if self.process.stdin is not None:
            with contextlib.suppress(BrokenPipeError, OSError, ValueError):
                self.process.stdin.close()
        try:
            self.process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)
        self.output_thread.join(timeout=3)
        self.error_thread.join(timeout=3)

    def validate_protocol(self) -> None:
        if self.protocol_error is not None:
            raise SmokeError(self.protocol_error)
        if self.output_thread.is_alive() or self.error_thread.is_alive():
            raise SmokeError("MSP output readers did not terminate")


def completed_tools(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    tools: list[dict[str, Any]] = []
    for frame in records:
        if frame.get("method") != "item/completed":
            continue
        item = frame.get("params", {}).get("item")
        if isinstance(item, dict) and item.get("kind") == "toolCall":
            tools.append(item)
    return tools


def transcript_contains(records: list[dict[str, Any]], value: str) -> bool:
    return value in json.dumps(records, separators=(",", ":"))


def require_completed(terminal: dict[str, Any], label: str) -> None:
    if terminal.get("params", {}).get("terminal") != "completed":
        raise SmokeError(f"{label} did not complete successfully")


def run_smoke(
    wrapper: pathlib.Path,
    environment: dict[str, str],
    workspace: pathlib.Path,
    timeout: float,
) -> dict[str, Any]:
    token = "muse-codex-live-" + secrets.token_hex(12)
    token_path = workspace / "smoke-token.txt"
    descriptor = os.open(token_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        output.write(token + "\n")

    session_id = uuid7()
    first = MspClient(wrapper, environment, workspace, timeout)
    try:
        first.initialize()
        started = first.request(
            "session/start",
            {
                "commandId": uuid7(),
                "sessionId": session_id,
                "workspaceRoot": str(workspace),
                "providerId": "codex",
                "approvalMode": "onRequest",
            },
        )
        session = started.get("session")
        if not isinstance(session, dict) or session.get("providerId") != "codex":
            raise SmokeError("session/start did not expose the codex provider")
        first_index, first_terminal = first.run_turn(
            session_id,
            "Use muse.read_file exactly once to read smoke-token.txt. Do not call any other "
            "tool and do not use shell, network, workflows, or writes. Then reply with the "
            "file's exact single-line value and no other text.",
        )
        require_completed(first_terminal, "first turn")
        first_records = first.records[first_index:]
        tools = completed_tools(first_records)
        if len(tools) != 1 or not str(tools[0].get("tool", "")).endswith("read_file"):
            raise SmokeError("first turn did not complete exactly one read_file call")
        if not transcript_contains(first_records, token):
            raise SmokeError("first turn did not return the private file value")
    finally:
        first.close()
    first.validate_protocol()

    resumed = MspClient(wrapper, environment, workspace, timeout)
    try:
        resumed.initialize()
        result = resumed.request(
            "session/resume",
            {
                "commandId": uuid7(),
                "sessionId": session_id,
                "history": "inline",
            },
        )
        session = result.get("session")
        if not isinstance(session, dict) or session.get("providerId") != "codex":
            raise SmokeError("session/resume did not expose the codex provider")
        if token not in json.dumps(result.get("history"), separators=(",", ":")):
            raise SmokeError("resumed history lost the first turn's tool result")
        second_index, second_terminal = resumed.run_turn(
            session_id,
            "Without calling any tool, reply with the exact value read from smoke-token.txt "
            "earlier and no other text.",
        )
        require_completed(second_terminal, "resumed turn")
        second_records = resumed.records[second_index:]
        if completed_tools(second_records):
            raise SmokeError("resumed turn unexpectedly called a tool")
        if not transcript_contains(second_records, token):
            raise SmokeError("resumed turn did not recall the private file value")
    finally:
        resumed.close()
    resumed.validate_protocol()

    return {
        "status": "ok",
        "sessionId": session_id,
        "firstTurn": {"terminal": "completed", "toolCalls": 1},
        "resumedTurn": {"terminal": "completed", "toolCalls": 0},
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wrapper", default="target/debug/muse-codex")
    parser.add_argument("--stock")
    parser.add_argument("--gateway")
    parser.add_argument("--timeout", type=float, default=240.0)
    parser.add_argument(
        "--run",
        action="store_true",
        help="required acknowledgement that this performs two live subscription turns",
    )
    args = parser.parse_args()
    if not args.run:
        parser.error("refusing live use without --run")
    if not 30 <= args.timeout <= 600:
        parser.error("--timeout must be between 30 and 600 seconds")
    wrapper = pathlib.Path(args.wrapper).resolve()
    if not os.access(wrapper, os.X_OK):
        parser.error("--wrapper must name an executable file")
    environment = clean_environment(args.stock, args.gateway)

    status = subprocess.run(
        [str(wrapper), "auth", "status"],
        env=environment,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        timeout=30,
        check=False,
    )
    if status.returncode != 0 or not status.stdout.startswith(b"Signed in with ChatGPT"):
        raise SmokeError("refusing live run: the selected profile is not using ChatGPT login")

    root = pathlib.Path(tempfile.mkdtemp(prefix="muse-codex-live-msp."))
    os.chmod(root, 0o700)
    workspace = root / "workspace"
    workspace.mkdir(mode=0o700)
    try:
        summary = run_smoke(wrapper, environment, workspace, args.timeout)
    finally:
        shutil.rmtree(root)
    print(json.dumps(summary, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SmokeError as error:
        print(f"live MSP subscription smoke failed: {error}", file=sys.stderr)
        raise SystemExit(1)
