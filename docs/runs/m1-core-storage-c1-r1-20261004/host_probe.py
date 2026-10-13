#!/usr/bin/env python3
"""Task-local public Host probes for the two r3 capability gaps.

This script only starts the frozen Host binary with an owned socket/profile
root, talks to its public Unix-socket protocol, and terminates the exact PID it
spawned. A caller-provided work root must not already exist; the harness creates
it and removes only that owned root after a successful run. It does not modify
product source or inspect private runtime state.
"""

from __future__ import annotations

import argparse
import hashlib
import http.server
import json
import os
import signal
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any


SOCKET_TIMEOUT_SECONDS = 15.0


def emit(kind: str, **fields: Any) -> None:
    print(json.dumps({"kind": kind, **fields}, sort_keys=True))
    sys.stdout.flush()


def file_receipt(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {"exists": False, "path": str(path)}
    data = path.read_bytes()
    return {
        "exists": True,
        "path": str(path),
        "sha256": hashlib.sha256(data).hexdigest(),
        "text": data.decode("utf-8", errors="replace"),
    }


class HostClient:
    def __init__(self, stream: socket.socket):
        self.stream = stream
        self.buffer = b""
        self.next_id = 1

    def read(self) -> dict[str, Any]:
        while b"\n" not in self.buffer:
            chunk = self.stream.recv(65536)
            if not chunk:
                raise RuntimeError("Host socket closed before a complete response line")
            self.buffer += chunk
        line, self.buffer = self.buffer.split(b"\n", 1)
        return json.loads(line.decode("utf-8"))

    def raw(self, request: dict[str, Any]) -> dict[str, Any]:
        emit("request", request=request)
        self.stream.sendall(json.dumps(request, separators=(",", ":")).encode("utf-8") + b"\n")
        response = self.read()
        emit("response", response=response)
        return response

    @staticmethod
    def operation(status: dict[str, Any], operation_id: str | None = None) -> dict[str, Any]:
        sequence = status["next_sequence"]
        return {
            "operation_id": operation_id or f"probe-{sequence}",
            "session_id": status["session_id"],
            "attachment_id": status["attachment_id"],
            "sequence": sequence,
            "control_epoch": status["control"]["epoch"],
            "viewport_revision": status["viewport_revision"],
            "document_revision": status["document_revision"],
        }

    @staticmethod
    def frame(status: dict[str, Any], frame_sequence: int = 1) -> dict[str, Any]:
        return {
            "session_id": status["session_id"],
            "tab_id": status["active_tab_id"],
            "presentation_revision": status["presentation_revision"],
            "frame_sequence": frame_sequence,
            "document_revision": status["document_revision"],
            "viewport_revision": status["viewport_revision"],
        }

    def status(self) -> dict[str, Any]:
        response = self.raw({"id": self.next_id, "command": {"type": "status"}, "operation": None})
        self.next_id += 1
        if response.get("type") != "result":
            raise RuntimeError(f"status failed: {response}")
        return response["value"]

    def request(self, command: dict[str, Any], operation: dict[str, Any] | None) -> dict[str, Any]:
        request_id = self.next_id
        self.next_id += 1
        return self.raw({"id": request_id, "command": command, "operation": operation})

    def mutation(self, command: dict[str, Any], operation: dict[str, Any] | None = None) -> dict[str, Any]:
        if operation is None:
            operation = self.operation(self.status())
        return self.request(command, operation)

    def attach_agent(self) -> dict[str, Any]:
        response = self.request({"type": "attach", "mode": "agent", "viewport": None}, None)
        if response.get("type") != "result":
            raise RuntimeError(f"attach failed: {response}")
        return response["value"]


def start_host(
    binary: Path, root: Path, profile_root: Path, label: str
) -> tuple[subprocess.Popen[str], Path, Path]:
    socket_parent = Path(tempfile.mkdtemp(prefix="m1r3sock-", dir="/tmp"))
    socket_dir = socket_parent / "socket"
    stdout_path = root / f"{label}.stdout"
    stderr_path = root / f"{label}.stderr"
    stdout = stdout_path.open("w", encoding="utf-8")
    stderr = stderr_path.open("w", encoding="utf-8")
    process = subprocess.Popen(
        [
            str(binary),
            "--socket-dir",
            str(socket_dir),
            "--profile-root",
            str(profile_root),
            "--allow-private-network",
        ],
        stdout=stdout,
        stderr=stderr,
        text=True,
    )
    stdout.close()
    stderr.close()
    emit(
        "host_started",
        label=label,
        pid=process.pid,
        socket_dir=str(socket_dir),
        profile_root=str(profile_root),
        binary=str(binary),
    )
    emit("socket_root", label=label, path=str(socket_parent))
    deadline = time.monotonic() + 60.0
    socket_path = socket_dir / "host.sock"
    while time.monotonic() < deadline:
        if process.poll() is not None:
            stderr_text = stderr_path.read_text(encoding="utf-8", errors="replace")
            stop_host(process, root, label, socket_parent)
            raise RuntimeError(
                f"Host exited before listener readiness: pid={process.pid} "
                f"exit={process.returncode} stderr={stderr_text!r}"
            )
        try:
            probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            probe.settimeout(0.2)
            probe.connect(str(socket_path))
            probe.close()
            return process, socket_path, socket_parent
        except OSError:
            time.sleep(0.02)
    try:
        stop_host(process, root, label, socket_parent)
    except subprocess.TimeoutExpired as exc:
        emit(
            "host_cleanup_failed",
            label=label,
            pid=process.pid,
            socket_root=str(socket_parent),
            error=str(exc),
            retained=True,
        )
        raise
    emit(
        "host_cleanup_succeeded",
        label=label,
        pid=process.pid,
        socket_root=str(socket_parent),
    )
    raise RuntimeError(f"Host listener timed out: pid={process.pid} socket={socket_path}")


def stop_host(process: subprocess.Popen[str], root: Path, label: str, socket_parent: Path) -> None:
    if process.poll() is None:
        os.kill(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=30)
    except subprocess.TimeoutExpired:
        emit("host_stop_timeout", label=label, pid=process.pid)
        raise
    emit("host_stopped", label=label, pid=process.pid, exit=process.returncode)
    for name in (f"{label}.stdout", f"{label}.stderr"):
        path = root / name
        if path.exists():
            emit("host_log", name=name, text=path.read_text(encoding="utf-8", errors="replace"))
    shutil.rmtree(socket_parent, ignore_errors=True)
    emit("socket_root_removed", label=label, path=str(socket_parent), exists=socket_parent.exists())


def connect(socket_path: Path) -> HostClient:
    stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    stream.settimeout(SOCKET_TIMEOUT_SECONDS)
    stream.connect(str(socket_path))
    client = HostClient(stream)
    ready = client.read()
    emit("ready", response=ready)
    if ready.get("type") != "ready":
        raise RuntimeError(f"unexpected ready response: {ready}")
    return client


def close_client(client: HostClient) -> None:
    client.stream.close()


def evaluate_value(response: dict[str, Any]) -> Any:
    if response.get("type") != "result":
        raise RuntimeError(f"evaluate failed: {response}")
    return response["value"]["result"]["value"]


class PageHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        if self.path == "/storage.html":
            body = b"<input id=f value=seed style='position:absolute;left:10px;top:10px;width:300px;height:30px'>"
        elif self.path == "/compose.html":
            body = (
                b"<input id=f value=seed style='position:absolute;left:10px;top:10px;width:300px;height:30px'>"
                b"<script>window.events=[];var e=document.getElementById('f');"
                b"['beforeinput','input','compositionstart','compositionupdate','compositionend']"
                b".forEach(function(t){e.addEventListener(t,function(ev){window.events.push({type:ev.type,data:ev.data||''});});});</script>"
            )
        else:
            self.send_response(404)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: Any) -> None:
        return


def start_page_server() -> tuple[http.server.ThreadingHTTPServer, threading.Thread, str]:
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), PageHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    port = server.server_address[1]
    origin = f"http://127.0.0.1:{port}"
    emit("page_server_started", origin=origin, port=port)
    return server, thread, origin


def stop_page_server(server: http.server.ThreadingHTTPServer, thread: threading.Thread) -> None:
    server.shutdown()
    server.server_close()
    thread.join(timeout=5)
    emit("page_server_stopped")


def run_storage_clear_restart(binary: Path, work: Path) -> int:
    server, server_thread, origin = start_page_server()
    page_url = f"{origin}/storage.html"
    origin_key = origin.encode("utf-8").hex()
    profile_root = work / "profile-root"
    storage_file = profile_root / "profiles" / "default" / "localStorage" / f"{origin_key}.json"
    try:
        phase1 = work / "storage-phase1"
        phase1.mkdir()
        process, socket_path, socket_parent = start_host(binary, phase1, profile_root, "storage-phase1")
        try:
            client = connect(socket_path)
            client.attach_agent()
            client.mutation({"type": "navigate", "url": page_url})
            set_result = client.mutation(
                {
                    "type": "evaluate",
                    "expression": "localStorage.setItem('k','v'); localStorage.getItem('k')",
                }
            )
            flush_set = client.request({"type": "flush_profiles"}, None)
            after_set = file_receipt(storage_file)
            emit(
                "f1_after_set_flush",
                page_url=page_url,
                origin=origin,
                origin_key=origin_key,
                set_result=set_result,
                flush=flush_set,
                storage_file=after_set,
            )

            clear_result = client.mutation(
                {
                    "type": "evaluate",
                    "expression": "localStorage.clear(); localStorage.getItem('k')",
                }
            )
            flush_clear = client.request({"type": "flush_profiles"}, None)
            after_clear = file_receipt(storage_file)
            emit(
                "f1_after_clear_flush",
                clear_result=clear_result,
                flush=flush_clear,
                storage_file=after_clear,
            )
            close_client(client)
        finally:
            stop_host(process, phase1, "storage-phase1", socket_parent)
        after_stop = file_receipt(storage_file)
        emit("f1_after_phase1_stop", storage_file=after_stop)

        phase2 = work / "storage-phase2"
        phase2.mkdir()
        process, socket_path, socket_parent = start_host(binary, phase2, profile_root, "storage-phase2")
        try:
            client = connect(socket_path)
            client.attach_agent()
            navigate = client.mutation({"type": "navigate", "url": page_url})
            cold_get = client.mutation(
                {"type": "evaluate", "expression": "localStorage.getItem('k')"}
            )
            status = client.status()
            value = evaluate_value(cold_get)
            emit(
                "f1_after_cold_restart",
                navigate=navigate,
                get=cold_get,
                value=value,
                status=status,
            )
            close_client(client)
        finally:
            stop_host(process, phase2, "storage-phase2", socket_parent)

        clear_value = evaluate_value(clear_result)
        resurrected = value == "v"
        emit(
            "f1_summary",
            same_origin=origin,
            profile_root=str(profile_root),
            storage_file=str(storage_file),
            set_value=evaluate_value(set_result),
            clear_value=clear_value,
            stale_file_after_clear=after_clear,
            cold_get=value,
            resurrection_observed=resurrected,
            result="RED" if resurrected else "UNREPRODUCED",
        )
        return 1 if resurrected else 0
    finally:
        stop_page_server(server, server_thread)


def require_editor_context(status: dict[str, Any]) -> dict[str, Any]:
    context = status.get("editor_context")
    if not isinstance(context, dict):
        raise RuntimeError(f"expected editor_context, got {context!r}")
    return context


def editor_binding(context: dict[str, Any], edit_session_id: str) -> dict[str, Any]:
    return {
        "type": "editor",
        "edit_session_id": edit_session_id,
        "target_id": context["target_id"],
        "focus_revision": context["focus_revision"],
        "editor_revision": context["editor_revision"],
    }


def input_command(
    client: HostClient,
    status: dict[str, Any],
    operation: dict[str, Any],
    event_sequence: int,
    binding: dict[str, Any],
    event: dict[str, Any],
) -> dict[str, Any]:
    return client.request(
        {
            "type": "input",
            "envelope": {
                "event_sequence": event_sequence,
                "frame": client.frame(status, event_sequence),
                "binding": binding,
                "event": event,
            },
        },
        operation,
    )


def run_composition_cancel(binary: Path, work: Path) -> int:
    server, server_thread, origin = start_page_server()
    page_url = f"{origin}/compose.html"
    try:
        phase = work / "composition-session"
        phase.mkdir()
        process, socket_path, socket_parent = start_host(
            binary, phase, work / "composition-profiles", "composition-session"
        )
        try:
            client = connect(socket_path)
            client.attach_agent()
            client.mutation({"type": "navigate", "url": page_url})
            click = client.mutation({"type": "click", "x": 20.0, "y": 20.0})
            status = client.status()
            context = require_editor_context(status)
            emit("f2_editor_ready", page_url=page_url, click=click, status=status)

            operation = client.operation(status, "composition-cancel")
            begin = input_command(
                client,
                status,
                operation,
                0,
                editor_binding(context, "cancel-1"),
                {"type": "composition_begin", "data": "中"},
            )
            status_begin = client.status()
            update = input_command(
                client,
                status,
                operation,
                1,
                editor_binding(context, "cancel-1"),
                {"type": "composition_update", "data": "中文"},
            )
            status_update = client.status()
            cancel = input_command(
                client,
                status,
                operation,
                2,
                editor_binding(context, "cancel-1"),
                {"type": "composition_cancel"},
            )
            status_cancel = client.status()
            cancel_context = require_editor_context(status_cancel)
            cancel_state = cancel.get("value", {}).get("editing", {}).get("state")
            cancel_lease = status_cancel.get("input_lease")
            cancel_composition = cancel_context.get("composition")
            cancel_value = (cancel_context.get("surrounding") or {}).get("text")

            next_operation = client.operation(status_cancel, "composition-next")
            next_begin = input_command(
                client,
                status_cancel,
                next_operation,
                0,
                editor_binding(cancel_context, "cancel-2"),
                {"type": "composition_begin", "data": "文"},
            )
            status_next = client.status()
            next_state = next_begin.get("value", {}).get("editing", {}).get("state")
            next_lease = status_next.get("input_lease")
            next_cancel = input_command(
                client,
                status_cancel,
                next_operation,
                1,
                editor_binding(cancel_context, "cancel-2"),
                {"type": "composition_cancel"},
            )
            status_final = client.status()
            final_state = next_cancel.get("value", {}).get("editing", {}).get("state")
            final_lease = status_final.get("input_lease")

            passed = (
                begin.get("value", {}).get("editing", {}).get("state") == "pending"
                and update.get("value", {}).get("editing", {}).get("state") == "pending"
                and cancel_state == "cancelled"
                and cancel_lease is None
                and cancel_composition is None
                and cancel_value == "seed"
                and next_state == "pending"
                and next_lease is not None
                and next_lease.get("kind") == "composition"
                and final_state == "cancelled"
                and final_lease is None
            )
            emit(
                "f2_summary",
                begin=begin,
                begin_status=status_begin,
                update=update,
                update_status=status_update,
                cancel=cancel,
                cancel_status=status_cancel,
                next_begin=next_begin,
                next_status=status_next,
                next_cancel=next_cancel,
                final_status=status_final,
                checks={
                    "cancel_state": cancel_state,
                    "lease_after_cancel": cancel_lease,
                    "composition_after_cancel": cancel_composition,
                    "value_after_cancel": cancel_value,
                    "next_begin_state": next_state,
                    "next_lease": next_lease,
                    "final_state": final_state,
                    "final_lease": final_lease,
                },
                result="PASS" if passed else "RED",
            )
            close_client(client)
            return 0 if passed else 1
        finally:
            stop_host(process, phase, "composition-session", socket_parent)
    finally:
        stop_page_server(server, server_thread)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host-binary", required=True, type=Path)
    parser.add_argument(
        "--scenario",
        required=True,
        choices=["storage-clear-restart", "composition-cancel"],
    )
    parser.add_argument("--work-root", type=Path)
    parser.add_argument("--keep-work-root", action="store_true")
    args = parser.parse_args()

    provided_root = args.work_root is not None
    if provided_root:
        work = args.work_root
        if work.exists() or work.is_symlink():
            emit(
                "work_root_rejected",
                path=str(work),
                reason="caller-provided work root must not already exist",
            )
            return 2
        try:
            work.mkdir()
        except OSError as exc:
            emit(
                "work_root_rejected",
                path=str(work),
                reason=f"cannot create fresh work root: {exc}",
            )
            return 2
    else:
        work = Path(tempfile.mkdtemp(prefix="m1r3-", dir="/tmp"))
    emit(
        "work_root",
        path=str(work),
        provided=provided_root,
        host_binary=str(args.host_binary),
        host_binary_sha256=hashlib.sha256(args.host_binary.read_bytes()).hexdigest(),
    )
    exit_code = 2
    try:
        if args.scenario == "storage-clear-restart":
            exit_code = run_storage_clear_restart(args.host_binary, work)
        elif args.scenario == "composition-cancel":
            exit_code = run_composition_cancel(args.host_binary, work)
        return exit_code
    finally:
        keep = args.keep_work_root or exit_code != 0
        if not keep:
            shutil.rmtree(work, ignore_errors=True)
        emit("work_root_final", path=str(work), exists=work.exists(), retained=keep)


if __name__ == "__main__":
    raise SystemExit(main())
