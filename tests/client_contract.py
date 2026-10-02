"""Real OpenAI SDK over TCP against the explicit fake AGY, entirely in Docker.

These are n8n/Hermes-shaped protocol probes, not tests of installed applications.
No authentication files or host CLI are discovered or read.
"""
import hashlib
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import signal
import socket
import stat
import subprocess
import threading
import time
import urllib.error
import urllib.request

import openai
from openai import OpenAI

BINARY = Path("/build/target/release/ai-router")
FAKE = Path("/build/tests/fixtures/fake-agy")
STATE = Path("/var/lib/ai-router/auth")
TELEMETRY = Path("/var/lib/ai-router/telemetry")
WORKSPACES = Path("/run/ai-router/workspaces")
MONITOR = Path("/run/ai-router/monitor.sock")
BASE = "http://127.0.0.1:8080"
MODEL = "gemini-3-pro"
MODEL_KEY = "synthetic-model-test-key"
NATIVE_KEY = "synthetic-native-test-key"
OTHER_KEY = "synthetic-other-owner-test-key"
TOOL_SCHEMA = {"type": "object", "properties": {"city": {"type": "string"}},
               "required": ["city"], "additionalProperties": False}
CHAT_TOOL = {"type": "function", "function": {"name": "weather", "description": "Weather",
             "parameters": TOOL_SCHEMA, "strict": True}}
RESPONSE_TOOL = {"type": "function", "name": "weather", "description": "Weather",
                 "parameters": TOOL_SCHEMA, "strict": True}
SCHEMA = {"type": "object", "properties": {"count": {"type": "integer"}},
          "required": ["count"], "additionalProperties": False}
checks = 0


def check(condition, message):
    global checks
    assert condition, message
    checks += 1


def http(path, body=None, key=MODEL_KEY, method=None, headers=None):
    request_headers = {"Authorization": "Bearer " + key}
    if body is not None:
        request_headers["Content-Type"] = "application/json"
    request_headers.update(headers or {})
    payload = body if isinstance(body, bytes) else (json.dumps(body).encode() if body is not None else None)
    request = urllib.request.Request(BASE + path, data=payload, method=method, headers=request_headers)
    try:
        with urllib.request.urlopen(request, timeout=8) as response:
            return response.status, response.headers, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.headers, error.read()


def snapshot():
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(5)
        client.connect(str(MONITOR))
        client.sendall(b"snapshot\n")
        chunks = []
        while True:
            chunk = client.recv(65536)
            if not chunk:
                break
            chunks.append(chunk)
        return json.loads(b"".join(chunks))


def wait_idle():
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        current = snapshot()
        if current["active"] == 0 and current["queued"] == 0:
            return current
        time.sleep(0.02)
    raise AssertionError("Fixture requests did not release runtime admission")


def start(native=False, preauth=2000, request_timeout=3):
    keys = [{"id": label, "sha256": hashlib.sha256(value.encode()).hexdigest(), "scopes": scopes}
            for label, value, scopes in [("model-client", MODEL_KEY, ["model"]),
                ("native-client", NATIVE_KEY, ["native"]), ("other-client", OTHER_KEY, ["native"])]]
    environment = dict(os.environ)
    environment.update({
        "HOME": str(STATE), "AGY_CLI_DISABLE_AUTO_UPDATE": "true",
        "AI_ROUTER_AGY_BIN": str(FAKE), "AI_ROUTER_BIND": "127.0.0.1:8080",
        "AI_ROUTER_STATE_DIR": str(STATE), "AI_ROUTER_WORKSPACE_DIR": str(WORKSPACES),
        "AI_ROUTER_TELEMETRY_DIR": str(TELEMETRY), "AI_ROUTER_MONITOR_SOCKET": str(MONITOR),
        "AI_ROUTER_KEYS": json.dumps(keys), "AI_ROUTER_NATIVE_ENABLED": str(native).lower(),
        "AI_ROUTER_MAX_CONCURRENT": "2", "AI_ROUTER_MAX_QUEUE": "0",
        "AI_ROUTER_REQUEST_TIMEOUT_SECS": str(request_timeout), "AI_ROUTER_TRUSTED_PROXIES": "",
        "AI_ROUTER_PREAUTH_PER_MINUTE": str(preauth), "AI_ROUTER_GLOBAL_PREAUTH_PER_MINUTE": "2000",
        "AI_ROUTER_PER_KEY_PER_MINUTE": "1000", "AI_ROUTER_MAX_BODY_BYTES": "1048576",
    })
    server = subprocess.Popen([str(BINARY), "serve"], env=environment,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if server.poll() is not None:
            raise AssertionError("Fixture server startup failed: " + server.stderr.read().decode())
        try:
            if http("/ready")[0] == 200 and MONITOR.exists():
                return server
        except (urllib.error.URLError, ConnectionError):
            pass
        time.sleep(0.05)
    stop(server)
    raise AssertionError("Fixture readiness deadline exceeded")


def stop(server, timeout=8):
    if server.poll() is None:
        server.send_signal(signal.SIGTERM)
    try:
        server.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        server.kill()
        server.wait()
        raise AssertionError("Gateway did not shut down within the local acceptance bound")
    check(server.returncode == 0, "Gateway exited unsuccessfully")


def wait_descendant(request_id):
    path = WORKSPACES / request_id / "descendant.pid"
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if path.is_file():
            # This is an explicit synthetic fixture PID, never AGY state.
            return int(path.read_text())
        time.sleep(0.02)
    raise AssertionError("Synthetic provider descendant did not start")


def descendant_dead(pid):
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
            # A zombie cannot execute; Docker's init will reap it.
            if Path(f"/proc/{pid}/stat").read_text().split()[2] == "Z":
                return True
        except (ProcessLookupError, FileNotFoundError):
            return True
        time.sleep(0.02)
    return False


def active_shutdown_contract():
    server = start(native=True, request_timeout=30)
    stream = None
    collector = None
    collected, errors = [], []
    try:
        body = {"model": MODEL, "messages": [{"role": "user", "content": "__fake_timeout__"}], "stream": True}
        stream = urllib.request.urlopen(urllib.request.Request(BASE + "/v1/chat/completions", data=json.dumps(body).encode(),
            headers={"Authorization": "Bearer " + MODEL_KEY, "Content-Type": "application/json"}), timeout=10)
        model_id = stream.headers["X-Request-Id"]
        model_pid = wait_descendant(model_id)
        status, _, response = http("/v1/agy/runs", {"model": MODEL, "prompt": "__fake_timeout__"}, key=NATIVE_KEY)
        check(status == 202, "Native cancellation fixture accepted before SIGTERM")
        native_id = json.loads(response)["id"]
        native_pid = wait_descendant(native_id)
        check(snapshot()["active"] == 2, "Model and native children both active before SIGTERM")

        def consume():
            try:
                collected.append(stream.read())
            except Exception as error:
                errors.append(error)

        collector = threading.Thread(target=consume, daemon=True)
        collector.start()
        stop(server)
        collector.join(timeout=3)
        check(not collector.is_alive() and not errors, "SIGTERM closes the consumed model stream cleanly")
        check(collected and b"request_cancelled" in collected[0] and b"[DONE]" in collected[0],
              "Active model SIGTERM returns terminal cancellation and DONE")
        persisted = json.loads((TELEMETRY / "telemetry.json").read_text())
        final = {record["request_id"]: record for record in persisted if record["request_id"] in (model_id, native_id)}
        check(set(final) == {model_id, native_id}, "SIGTERM flush persists both active cleanup records")
        check(all(record["status"] == "cancelled" and record["error_code"] == "request_cancelled" for record in final.values()),
              "Active model and native shutdown metadata retain cancellation status")
        check(descendant_dead(model_pid) and descendant_dead(native_pid), "SIGTERM kills both provider process groups")
        check(not (WORKSPACES / model_id).exists() and not (WORKSPACES / native_id).exists(),
              "SIGTERM removes both owned active workspaces before exit")
    finally:
        if stream is not None:
            stream.close()
        if collector is not None:
            collector.join(timeout=1)
        if server.poll() is None:
            stop(server, timeout=15)


def stalled_input_shutdown_contract():
    server = start()
    connection = socket.create_connection(("127.0.0.1", 8080), timeout=3)
    try:
        before = snapshot()
        connection.sendall(("POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n"
            f"Authorization: Bearer {MODEL_KEY}\r\nContent-Type: application/json\r\n"
            "Content-Length: 100000\r\n\r\n{").encode())
        check(http("/v1/models")[0] == 200, "Server remains responsive while a request body stalls")
        check(snapshot()["active"] == 0, "Incomplete request body never admits provider work")
        # The HTTP graceful-close bound precedes middleware's body deadline;
        # this case holds no generation admission and may exit successfully.
        stop(server, timeout=15)
        persisted = json.loads((TELEMETRY / "telemetry.json").read_text())
        previous_ids = {record["request_id"] for record in before["records"]}
        new = [record for record in persisted if record["request_id"] not in previous_ids]
        check(all(record["startup_ms"] is None for record in new), "Stalled-body shutdown persists no fabricated provider readiness")
        check(not any(WORKSPACES.iterdir()), "Stalled-body shutdown leaves no provider workspace")
    finally:
        connection.close()
        if server.poll() is None:
            stop(server, timeout=15)


def sdk_contracts():
    client = OpenAI(api_key=MODEL_KEY, base_url=BASE + "/v1", max_retries=0, timeout=8)
    check(MODEL in [model.id for model in client.models.list().data], "SDK model discovery")
    completion = client.chat.completions.create(model=MODEL, messages=[{"role": "user", "content": "Hello"}])
    check(completion.choices[0].message.content == "Hello from fixture", "SDK Chat nonstream text")
    check(completion.usage.total_tokens == 23, "SDK observed Chat usage")
    chunks = list(client.chat.completions.create(model=MODEL, messages=[{"role": "user", "content": "Hello"}],
                                               stream=True, stream_options={"include_usage": True}))
    check(len({chunk.id for chunk in chunks}) == 1, "Stable Chat stream completion ID")
    check("".join(chunk.choices[0].delta.content or "" for chunk in chunks if chunk.choices) == "Hello from fixture",
          "SDK Chat text deltas")
    check(any(chunk.choices and chunk.choices[0].finish_reason == "stop" for chunk in chunks), "Chat finish reason")
    check(chunks[-1].usage.total_tokens == 23, "Chat stream terminal usage")

    # Hermes-compatible Chat tool call/result history with a client-owned tool.
    messages = [{"role": "system", "content": "Use supplied client tools when useful."},
                {"role": "user", "content": "Weather in London"}]
    selected = client.chat.completions.create(model=MODEL, messages=messages, tools=[CHAT_TOOL],
                                             tool_choice="auto", parallel_tool_calls=False)
    call = selected.choices[0].message.tool_calls[0]
    check(selected.choices[0].finish_reason == "tool_calls", "Hermes-shaped tool finish reason")
    check(call.function.name == "weather" and json.loads(call.function.arguments) == {"city": "London"},
          "Structured Chat tool name/arguments")
    check(selected.model_extra.get("agy_usage_partial") is True, "Partial tool-handoff usage labelled")
    messages.extend([{"role": "assistant", "content": selected.choices[0].message.content,
                      "tool_calls": [{"id": call.id, "type": "function", "function": {
                          "name": call.function.name, "arguments": call.function.arguments}}]},
                     {"role": "tool", "tool_call_id": call.id, "content": "15 C"}])
    final = client.chat.completions.create(model=MODEL, messages=messages, tools=[CHAT_TOOL], parallel_tool_calls=False)
    check(final.choices[0].message.content == "Fixture final answer", "Chat tool result/final answer loop")
    streamed = list(client.chat.completions.create(model=MODEL, messages=[{"role": "user", "content": "__fake_tool__"}],
                                                  tools=[CHAT_TOOL], parallel_tool_calls=False, stream=True))
    stream_calls = [tool for chunk in streamed if chunk.choices for tool in (chunk.choices[0].delta.tool_calls or [])]
    check(stream_calls and stream_calls[0].index == 0 and stream_calls[0].id.startswith("call_"), "Stable streamed Chat call ID/index")
    check("".join(tool.function.arguments or "" for tool in stream_calls) == '{"city":"London"}', "Chat streamed tool arguments")

    response = client.responses.create(model=MODEL, input="Hello", store=False)
    check(response.output_text == "Hello from fixture", "SDK Responses nonstream text")
    events = list(client.responses.create(model=MODEL, input="Hello", stream=True, store=False))
    check(events[0].type == "response.created" and events[-1].type == "response.completed", "Responses lifecycle events")
    check([event.sequence_number for event in events] == list(range(len(events))), "Responses event sequence")
    check("".join(event.delta for event in events if event.type == "response.output_text.delta") == "Hello from fixture",
          "SDK Responses text deltas")
    # The SDK's own accumulator checks output-item/content-part bookkeeping.
    with client.responses.stream(model=MODEL, input="Hello", store=False) as stream:
        list(stream)
        check(stream.get_final_response().output_text == "Hello from fixture", "Responses SDK stream accumulator")

    # n8n-compatible Responses tool call/result replay: call_id is distinct from item id.
    history = [{"role": "user", "content": [{"type": "input_text", "text": "Weather in London"}]}]
    selected = client.responses.create(model=MODEL, input=history, tools=[RESPONSE_TOOL], store=False, parallel_tool_calls=False)
    call = next(item for item in selected.output if item.type == "function_call")
    check(call.id != call.call_id and call.call_id.startswith("call_"), "Responses item ID and call ID remain distinct")
    history.extend([call.model_dump(exclude_none=True), {"type": "function_call_output", "call_id": call.call_id, "output": "15 C"}])
    final = client.responses.create(model=MODEL, input=history, tools=[RESPONSE_TOOL], store=False, parallel_tool_calls=False)
    check(final.output_text == "Fixture final answer", "Responses tool result/final answer loop")
    with client.responses.stream(model=MODEL, input="__fake_tool__", tools=[RESPONSE_TOOL], store=False,
                                 parallel_tool_calls=False) as stream:
        tool_events = list(stream)
        accumulated = stream.get_final_response()
        item = next(item for item in accumulated.output if item.type == "function_call")
        check(item.name == "weather" and json.loads(item.arguments) == {"city": "London"}, "Responses SDK tool accumulator")
        delta = next(event for event in tool_events if event.type == "response.function_call_arguments.delta")
        done = next(event for event in tool_events if event.type == "response.function_call_arguments.done")
        check(delta.item_id == done.item_id == item.id, "Stable Responses tool item IDs")

    chat_schema = client.chat.completions.create(model=MODEL, messages=[{"role": "user", "content": "schema"}],
        response_format={"type": "json_schema", "json_schema": {"name": "answer", "schema": SCHEMA, "strict": True}})
    check(json.loads(chat_schema.choices[0].message.content) == {"count": 1}, "Chat JSON schema output")
    response_schema = client.responses.create(model=MODEL, input="schema", store=False,
        text={"format": {"type": "json_schema", "name": "answer", "schema": SCHEMA, "strict": True}})
    check(json.loads(response_schema.output_text) == {"count": 1}, "Responses JSON schema output")
    try:
        client.chat.completions.create(model=MODEL, messages=[{"role": "user", "content": "Hello"}], temperature=0.5)
        raise AssertionError("Unsupported sampling was accepted")
    except openai.BadRequestError as error:
        check(error.code == "unsupported_parameter", "SDK receives explicit unsupported sampling error")
    try:
        client.embeddings.create(model=MODEL, input="No invented embedding")
        raise AssertionError("Unsupported embeddings were accepted")
    except openai.NotFoundError as error:
        check(error.code == "unsupported_endpoint", "No fabricated embeddings")
    client.close()


def policy_contracts():
    before = wait_idle()
    status, _, _ = http("/v1/chat/completions", b"{malformed", key="wrong-key")
    check(status == 401, "Authentication happens before malformed JSON parsing")
    check(snapshot()["active"] == 0, "Rejected key launches no provider work")
    # Send the oversized declaration without flooding a closed socket. An
    # implementation that reads the body before rejecting would stall here.
    connection = HTTPConnection("127.0.0.1", 8080, timeout=3)
    connection.putrequest("POST", "/v1/chat/completions")
    connection.putheader("Authorization", "Bearer " + MODEL_KEY)
    connection.putheader("Content-Type", "application/json")
    connection.putheader("Content-Length", "1048577")
    connection.endheaders()
    response = connection.getresponse()
    check(response.status == 413, "Oversized declared body rejected before body transfer")
    response.read(); connection.close()
    status, _, _ = http("/v1/agy/runs", {"model": MODEL, "prompt": "Hello"})
    check(status == 403, "Model-only key cannot access native API")
    status, _, _ = http("/v1/agy/runs", {"model": MODEL, "prompt": "Hello"}, key=NATIVE_KEY)
    check(status == 503, "Native profile remains gated without verification")
    check(stat.S_IMODE(MONITOR.stat().st_mode) == 0o600, "Private monitor socket mode0600")
    payload = {"model": MODEL, "messages": [{"role": "user", "content": "__fake_timeout__"}], "stream": True}
    request = lambda: urllib.request.urlopen(urllib.request.Request(BASE + "/v1/chat/completions", data=json.dumps(payload).encode(),
        headers={"Authorization": "Bearer " + MODEL_KEY, "Content-Type": "application/json"}), timeout=8)
    first, second = request(), request()
    status, headers, _ = http("/v1/chat/completions", {"model": MODEL, "messages": [{"role": "user", "content": "Hello"}]})
    check(status == 429 and "Retry-After" in headers, "Saturation rejects without unbounded queue")
    disconnected_at = time.monotonic()
    first.close(); second.close()
    after = wait_idle()
    check(time.monotonic() - disconnected_at < 1.5, "Client disconnect releases work before the provider deadline")
    check(after["aggregate"]["auth_failures"] >= before["aggregate"]["auth_failures"] + 1, "Auth failures observable")
    check(after["aggregate"]["partial_usage_records"] > 0, "Tool usage is visibly partial")
    check(any(record["startup_ms"] is not None for record in after["records"]), "AGY readiness latency observable")
    encoded = json.dumps(after)
    check("__fake_tool__" not in encoded and "Hello from fixture" not in encoded and "15 C" not in encoded and "London" not in encoded,
          "Monitor contains no prompt/generated/tool-result payloads")
    check(MODEL_KEY not in encoded and NATIVE_KEY not in encoded, "Monitor contains no bearer keys")


def native_transport_contracts():
    status, _, body = http("/v1/agy/runs", {"model": MODEL, "prompt": "synthetic native transport"}, key=NATIVE_KEY)
    check(status == 202, "Synthetic native job accepted")
    run_id = json.loads(body)["id"]
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        status, _, body = http("/v1/agy/runs/" + run_id, key=NATIVE_KEY)
        view = json.loads(body)
        if view["status"] not in ("running", "queued"):
            break
        time.sleep(0.02)
    check(view["status"] == "completed", "Synthetic native run completed")
    check(http("/v1/agy/runs/" + run_id, key=OTHER_KEY)[0] == 404, "Native job ownership enforced")
    status, _, body = http("/v1/agy/runs/" + run_id + "/artifacts", key=NATIVE_KEY)
    check(status == 200 and b"answer.txt" in body, "Native artifact listing")
    status, _, body = http("/v1/agy/runs/" + run_id + "/artifacts/answer.txt", key=NATIVE_KEY)
    check(status == 200 and body == b"Native fixture artifact\n", "Native artifact download")
    check(http("/v1/agy/runs/" + run_id + "/artifacts/answer.txt", key=OTHER_KEY)[0] == 404, "Artifact owner scope")
    check(http("/v1/agy/runs/" + run_id + "/artifacts/%2eenv.synthetic", key=NATIVE_KEY)[0] in (403, 404),
          "Protected artifact path rejected")
    check(http("/v1/agy/runs", {"model": MODEL, "prompt": "input", "files": [{"path": ".env.synthetic", "data_base64": "ZHVtbXk="}]},
               key=NATIVE_KEY)[0] == 403, "Protected upload rejected before workspace creation")


def brute_force_contract():
    initial = snapshot()
    check(initial["counts_since_start"]["total"] == 0, "Rejection counters reset on process restart despite retained history")
    statuses = [http("/v1/models", key="wrong-key", headers={"X-Real-IP": f"203.0.113.{i}"})[0] for i in range(1, 5)]
    check(statuses == [401, 401, 401, 429], "Untrusted forwarded IPs cannot bypass failed-auth limit")
    after = snapshot()
    counts = after["counts_since_start"]
    check(counts["reset_on_restart"] is True and counts["total"] == 4 and counts["auth"] == 3 and counts["rate"] == 1,
          "Every failed authentication and pre-auth rate rejection counted in fixed buckets")
    check(all(http("/v1/models", key="wrong-key")[0] == 429 for _ in range(100)), "Repeated attack traffic remains rate limited")
    after_flood = snapshot()
    counts = after_flood["counts_since_start"]
    check(counts["total"] == 104 and counts["auth"] == 3 and counts["rate"] == 101,
          "Unsampled rejection counters preserve complete flood totals")
    check(len(after_flood["records"]) - len(initial["records"]) <= 30,
          "Rejection flood contributes at most 30 recent-history records per minute")


def main():
    check(os.getuid() == 10001, "Network tests run as service UID10001")
    check(BINARY.is_file() and FAKE.is_file(), "Explicit Docker fixture paths exist")
    check(openai.__version__ == "3.24.0", "Pinned official OpenAI SDK version")
    server = start()
    try:
        sdk_contracts()
        policy_contracts()
    finally:
        stop(server)
    server = start(native=True)
    try:
        native_transport_contracts()
        wait_idle()
    finally:
        stop(server)
    active_shutdown_contract()
    stalled_input_shutdown_contract()
    server = start(preauth=3)
    try:
        brute_force_contract()
        expected_record_ids = {record["request_id"] for record in snapshot()["records"]}
    finally:
        stop(server)
    # Router-owned telemetry file only; never inspect AGY authentication state.
    persisted = json.loads((TELEMETRY / "telemetry.json").read_text())
    check(bool(persisted), "Graceful shutdown persists monitoring metadata")
    check({record["request_id"] for record in persisted} == expected_record_ids,
          "Graceful shutdown flushes the latest rejected-request metadata")
    check(stat.S_IMODE((TELEMETRY / "telemetry.json").stat().st_mode) == 0o600, "Persisted metadata mode0600")
    print(f"PASS: {checks} TCP/SDK/policy checks using OpenAI {openai.__version__} and synthetic AGY")
    print("n8n/Hermes request contracts exercised; installed applications and real AGY/sandbox remain separate gates")


if __name__ == "__main__":
    main()
