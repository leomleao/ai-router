#!/usr/bin/env python3
"""Exercise monitor output and keyboard/terminal cleanup with synthetic telemetry.

Run inside the Docker test image: python3 tests/monitor_tui.py
No running router, authentication state, or configuration files are used.
"""
import datetime as dt
import fcntl
import json
import os
import pathlib
import pty
import select
import socket
import struct
import subprocess
import tempfile
import termios
import threading
import time
import pyte


def fixture():
    now = dt.datetime.now(dt.timezone.utc)
    records = []
    for i in range(30):
        failed = i % 5 == 0
        records.append({
            "request_id": f"demo-request-{i:03}", "client_id": ["workstation", "home-assistant", "batch-jobs"][i % 3],
            "model": ["gemini-3-pro", "claude-sonnet-4.5"][i % 2], "endpoint": "/v1/responses",
            "started_at": (now - dt.timedelta(minutes=(29-i)*25)).isoformat(),
            "duration_ms": 1200+i*450, "startup_ms": 80+i*4, "first_output_ms": 260+i*6,
            "status": "failed" if failed else "completed", "error_code": "provider_timeout" if failed else None,
            "usage": None if failed else {"input_tokens": 800, "output_tokens": 320, "total_tokens": 1120,
                                          "thinking_tokens": 20, "cache_read_tokens": 100},
            "usage_partial": False,
        })
    return {
        "generated_at": now.isoformat(), "active": 2, "queued": 1,
        "provider": {"version": "1.2.15", "authenticated": True, "checked_at": now.isoformat(), "error": None,
                     "models": [{"id": "gemini-3-pro", "name": "Gemini 3 Pro"}],
                     "quota": {"groups": [{"name": "Gemini", "buckets": [{"id": "pro", "remaining_fraction": .72}]}]}},
        "provider_age_seconds": 12,
        "aggregate": dict.fromkeys(("requests", "failures", "auth_failures", "complete_usage_records",
                                    "partial_usage_records", "input_tokens", "output_tokens", "thinking_tokens",
                                    "cache_read_tokens", "total_tokens"), 0)
                     | dict.fromkeys(("duration_p50_ms", "duration_p95_ms", "first_output_p50_ms",
                                      "first_output_p95_ms", "startup_p50_ms", "startup_p95_ms"))
                     | {"by_client": {}, "by_model": {}, "by_error": {}},
        "counts_since_start": {"started_at": (now-dt.timedelta(hours=12)).isoformat(), "reset_on_restart": True,
                               "total": 7, "auth": 2, "rate": 3, "scope": 0, "input": 1, "busy": 1, "other": 0},
        "records": records, "retention_days": 30, "max_events": 50000, "persistence_errors": 0,
    }


def read_terminal(master, seconds=.3):
    data = bytearray()
    until = time.monotonic()+seconds
    while time.monotonic() < until:
        ready, _, _ = select.select([master], [], [], max(0, until-time.monotonic()))
        if ready:
            try:
                chunk = os.read(master, 65536)
                if not chunk:
                    break
                data.extend(chunk)
            except OSError:
                break
    return bytes(data)


def main():
    binary = "/build/target/debug/ai-router"
    with tempfile.TemporaryDirectory(prefix="monitor-tui-") as temp:
        address = str(pathlib.Path(temp)/"monitor.sock")
        server = socket.socket(socket.AF_UNIX)
        server.bind(address)
        server.listen()
        server.settimeout(.1)
        stop = threading.Event()
        disconnected = threading.Event()
        served = []
        snapshot = fixture()

        def serve():
            while not stop.is_set():
                try:
                    conn, _ = server.accept()
                except socket.timeout:
                    continue
                with conn:
                    assert conn.recv(32) == b"snapshot\n"
                    served.append(time.monotonic())
                    if not disconnected.is_set():
                        conn.sendall(json.dumps(snapshot).encode())

        worker = threading.Thread(target=serve, daemon=True)
        worker.start()
        env = dict(os.environ, AI_ROUTER_MONITOR_SOCKET=address, TERM="xterm-256color")
        for mode in ("--json", "--text"):
            result = subprocess.run([binary, "monitor", mode], env=env, capture_output=True, timeout=5, check=True)
            assert b"\x1b" not in result.stdout
            if mode == "--json":
                assert json.loads(result.stdout)["aggregate"]["requests"] == 30
            else:
                assert b"Recent requests" in result.stdout

        for exit_key in (b"q", b"\x03"):
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 120, 0, 0))
            original = termios.tcgetattr(slave)
            process = subprocess.Popen([binary, "monitor"], env=env, stdin=slave, stdout=slave, stderr=slave)
            transcript = bytearray()
            screen = pyte.Screen(120, 36)
            stream = pyte.Stream(screen)
            def receive(seconds=.3):
                data = read_terminal(master, seconds)
                transcript.extend(data)
                stream.feed(data.decode("utf-8"))
                return "\n".join(screen.display)
            try:
                initial = read_terminal(master, .7)
                transcript.extend(initial)
                stream.feed(initial.decode("utf-8"))
                assert "AI ROUTER" in "\n".join(screen.display) and "Requests over time" in "\n".join(screen.display)
                os.write(master, b"j")
                assert "demo-request-028" in receive().split("Selected request", 1)[1]
                os.write(master, b"\r")
                expanded = receive()
                for value in ("demo-request-028", "Client:", "Model:", "Endpoint:", "Started:", "Status:",
                              "Latency:", "Usage:", "input 800", "output 320", "total 1120", "thinking 20", "cache read 100"):
                    assert value in expanded, value
                os.write(master, b"\x1b")
                assert "Recent requests" in receive()
                for key, label in [(b"2", "Token usage"), (b"3", "Quota remaining"),
                                   (b"4", "Rejections since process start"), (b"5", "Selected request"), (b"6", "Navigate")]:
                    os.write(master, key)
                    assert label in receive(), key
                os.write(master, b"1 ")
                receive()
                before = len(served)
                receive(2.2)
                assert len(served) == before, "paused monitor fetched another snapshot"
                os.write(master, b" r")
                receive()
                assert len(served) > before, "range/resume did not refresh"
                disconnected.set()
                disconnected_screen = receive(2.2)
                assert "DISCONNECTED" in disconnected_screen and "showing last snapshot" in disconnected_screen
                disconnected.clear()
                assert "DISCONNECTED" not in receive(2.2)
                os.write(master, b"t\x14")  # theme and light/dark
                receive()
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
                screen.resize(lines=24, columns=80)
                assert "q quit" in receive()
                os.write(master, b"\r")
                assert "thinking 20" in receive()
                os.write(master, exit_key)
                receive()
                assert process.wait(timeout=5) == 0
                assert termios.tcgetattr(slave) == original, "terminal input settings were not restored"
                assert b"\x1b[?1049h" in transcript and b"\x1b[?1049l" in transcript
                preview = os.environ.get("AI_ROUTER_MONITOR_PREVIEW")
                if exit_key == b"q" and preview:
                    pathlib.Path(preview).write_bytes(initial)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                os.close(master)
                os.close(slave)
        stop.set()
        worker.join(timeout=2)
        server.close()
    print("Monitor PTY checks passed: pages/scroll, pause/range, disconnect/recovery, themes, resize, q/Ctrl+C, text/JSON.")


if __name__ == "__main__":
    main()
