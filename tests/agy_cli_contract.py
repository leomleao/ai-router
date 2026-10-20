"""Real pinned CLI + real gateway/relay against a loopback-only mock model.

Run in Docker with --network none. No saved login, real API key or production
volume is used. Management probe metadata is synthetic; custom-agent execution
and tool dispatch use the official executable, including schema/metadata handling.
"""
import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

AGY = Path(os.environ.get("AI_ROUTER_REAL_AGY", "/opt/agy-real"))
BINARY = Path("/build/target/debug/ai-router")
MODEL = "gemini-3.8-flash"
KEY = "synthetic-official-cli-contract-key"
case = {}
checks = 0


def check(condition, message):
    global checks
    assert condition, message
    checks += 1


class Model(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        functions = [f for t in request.get("tools", []) for f in t.get("functionDeclarations", [])]
        names = [f["name"] for f in functions]
        if not functions:
            parts = [{"text": "Synthetic conversation title"}]
        else:
            case["turns"] += 1
            name = "manage_task" if case.get("native") else f'mcp_ai-router_router_client_{case["index"]:03d}'
            function = next(f for f in functions if f["name"] == name)
            if not case.get("native"):
                schema = function["parametersJsonSchema"]
                check("call_mcp_tool" not in names, "Eager tools remove the lazy dispatcher from the model declarations")
                check(not {"ServerName", "ToolName", "Arguments"} & set(schema.get("properties", {})), "Direct function has no model-selected routing envelope")
                check("toolSummary" in schema["required"], "Official CLI declares its task-summary metadata")
            arguments = {"input":dict(case["arguments"])}
            if case.get("repair") and case["turns"] == 1:
                arguments = {"input":{"query":123}}
            arguments["toolSummary"] = "Synthetic tool selection"
            parts = [{"functionCall": {"name": name, "args": arguments}}]
        response = {"candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP", "index": 0}],
                    "usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 3, "totalTokenCount": 23}}
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        try:
            self.wfile.write(("data: " + json.dumps(response) + "\n\n").encode())
        except (BrokenPipeError, ConnectionResetError):
            pass  # The gateway stops the CLI immediately after its inert handoff.


def main():
    check(subprocess.check_output([str(AGY), "--version"], text=True).strip() == "1.4.3", "Official executable version matches the pin")
    with tempfile.TemporaryDirectory(prefix="agy-cli-synthetic-") as temporary:
        root = Path(temporary)
        model = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Model)
        threading.Thread(target=model.serve_forever, daemon=True).start()
        state, workspaces, telemetry = [root / p for p in ("auth", "workspaces", "telemetry")]
        wrapper = root / "synthetic-agy"
        # The gateway intentionally clears API billing keys. This test-only
        # wrapper injects a dummy key and loopback URL into a fresh synthetic
        # profile; it cannot access a host or production authentication volume.
        wrapper.write_text("#!/usr/bin/python3\n" + f'''
import json,os,pathlib,sys,uuid
home=pathlib.Path(os.environ["HOME"])
assert home == pathlib.Path({str(state)!r})
settings=home/".gemini/antigravity-cli/settings.json"
data=json.loads(settings.read_text()); data["modelProvider"]="gemini"
temporary=settings.with_name("synthetic-settings-"+uuid.uuid4().hex+".json")
temporary.write_text(json.dumps(data)); temporary.replace(settings)
args=sys.argv[1:]
if args == ["models"]:
 print("ID Name\\n{MODEL} Synthetic Gemini"); sys.exit(0)
if "-p" in args and args[args.index("-p")+1]=="/quota":
 print(json.dumps({{"status":"SUCCESS","command":{{"name":"usage","data":{{"groups":[{{"name":"Synthetic quota","buckets":[{{"id":"synthetic","window":"5h","remaining_fraction":0.5,"reset_time":"2026-10-21T00:00:00Z"}}]}}]}}}}}})); sys.exit(0)
if "-p" in args and args[args.index("-p")+1]=="/agents":
 # The API-key path lists no main agents on both 1.4.2 and 1.4.3. Keep
 # production's strict account-backed preflight unchanged; synthesize only
 # that management response here. Real --agent loading must still expose
 # our private MCP functions in the actual model request below.
 print(json.dumps({{"status":"SUCCESS","response":"router-model","num_turns":0,"command":{{"name":"agents","data":{{"agents":["router-model"]}}}},"usage":{{k:0 for k in ["input_tokens","output_tokens","thinking_tokens","cache_read_tokens","total_tokens"]}}}}));sys.exit(0)
os.environ["GEMINI_API_KEY"]="synthetic-not-a-real-key"
os.environ["GOOGLE_GEMINI_BASE_URL"]="http://127.0.0.1:{model.server_port}"
os.execv({str(AGY)!r},[{str(AGY)!r}]+args)
''')
        wrapper.chmod(0o700)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        base = f"http://127.0.0.1:{port}"
        environment = dict(os.environ)
        environment.update({"AI_ROUTER_AGY_BIN": str(wrapper), "AI_ROUTER_BIND": f"127.0.0.1:{port}",
                            "AI_ROUTER_STATE_DIR": str(state), "AI_ROUTER_WORKSPACE_DIR": str(workspaces),
                            "AI_ROUTER_TELEMETRY_DIR": str(telemetry), "AI_ROUTER_MONITOR_SOCKET": str(root / "monitor.sock"),
                            "AI_ROUTER_KEYS": json.dumps([{"id": "synthetic", "sha256": hashlib.sha256(KEY.encode()).hexdigest(), "scopes": ["model"]}]),
                            "AI_ROUTER_REQUEST_TIMEOUT_SECS": "20", "AI_ROUTER_PER_KEY_PER_MINUTE": "1000", "AI_ROUTER_NATIVE_ENABLED": "false"})
        with tempfile.TemporaryFile() as errors:
            gateway = subprocess.Popen([str(BINARY), "serve"], env=environment, stdout=subprocess.DEVNULL, stderr=errors)
            def request(path, payload=None):
                req = urllib.request.Request(base + path, data=json.dumps(payload).encode() if payload is not None else None,
                                             headers={"Authorization": "Bearer " + KEY, "Content-Type": "application/json"})
                try:
                    with urllib.request.urlopen(req, timeout=25) as reply:
                        return reply.status, reply.read()
                except urllib.error.HTTPError as error:
                    return error.code, error.read()
            try:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    try:
                        if request("/ready")[0] == 200:
                            break
                    except (OSError, urllib.error.URLError):
                        pass
                    time.sleep(0.05)
                check(request("/ready")[0] == 200, "Synthetic gateway reaches readiness with real CLI version discovery")
                simple = {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"], "additionalProperties": False}
                nested = {"type": "object", "properties": {"items": {"type": "array", "items": {"type": "object"}}, "arguments": {"type": "object"}, "toolSummary": {"type":"string"}}, "required": ["items", "arguments", "toolSummary"], "additionalProperties": False}
                union = {"type":"object","anyOf":[
                    {"type":"object","properties":{"action":{"const":"search"},"query":{"type":"string"}},"required":["action","query"],"additionalProperties":False},
                    {"type":"object","properties":{"action":{"const":"read"},"id":{"type":"string"}},"required":["action","id"],"additionalProperties":False}]}
                referenced = {"type":"object","$defs":{"query":{"type":"string"}},
                              "properties":{"query":{"$ref":"#/$defs/query"}},"required":["query"],"additionalProperties":False}
                for responses in (False, True):
                    for streaming in (False, True):
                        for name, schema, arguments in [("write_todos", simple, {"query": "synthetic"}),
                                                        ("research", nested, {"items": [{"name": "synthetic", "arguments": {"query": "synthetic"}}], "arguments": {"nested": {"keep": True}}, "toolSummary":"synthetic client field"}),
                                                        ("research", union, {"action":"search","query":"synthetic-private-cli-argument"}),
                                                        ("research", referenced, {"query":"synthetic reference"})]:
                            case.clear(); case.update(index=1, arguments=arguments, turns=0)
                            functions = [{"name": "unselected", "parameters": simple}, {"name": name, "parameters": schema}]
                            if responses:
                                payload = {"model": MODEL, "input": "Synthetic direct MCP selection", "tools": [dict(f, type="function") for f in functions], "stream": streaming}
                                endpoint = "/v1/responses"
                            else:
                                payload = {"model": MODEL, "messages": [{"role": "user", "content": "Synthetic direct MCP selection"}], "tools": [{"type": "function", "function": f} for f in functions], "stream": streaming}
                                endpoint = "/v1/chat/completions"
                            status, body = request(endpoint, payload)
                            check(status == 200, f"Official CLI completes {endpoint}, streaming={streaming}: {body[:500]!r}")
                            if streaming:
                                events = [json.loads(line[6:]) for line in body.decode().splitlines() if line.startswith("data: ") and line[6:] != "[DONE]"]
                                if responses:
                                    reply = next(e["response"] for e in events if e.get("type") == "response.completed")
                                else:
                                    calls = [t for e in events for choice in e.get("choices", []) for t in choice.get("delta", {}).get("tool_calls", [])]
                                    actual_name = "".join(c["function"].get("name", "") for c in calls)
                                    actual_arguments = json.loads("".join(c["function"].get("arguments", "") for c in calls))
                            else:
                                reply = json.loads(body)
                            if responses:
                                calls = [c for c in reply["output"] if c["type"] == "function_call"]
                                actual_name, actual_arguments = calls[0]["name"], json.loads(calls[0]["arguments"])
                            elif not streaming:
                                function = reply["choices"][0]["message"]["tool_calls"][0]["function"]
                                actual_name, actual_arguments = function["name"], json.loads(function["arguments"])
                            check(actual_name == name, "Official CLI selection preserves the original client name")
                            check(actual_arguments == arguments, "Official CLI preserves nested arguments and removes only transport metadata")
                case.clear(); case.update(index=0, arguments={"query": "corrected synthetic"}, turns=0, repair=True)
                status, body = request("/v1/responses", {"model": MODEL, "input": "Synthetic provider validation recovery", "tools": [{"type": "function", "name": "research", "parameters": simple}]})
                check(status == 200 and case["turns"] == 2, "Official CLI can correct a schema error before the relay and hand off one valid call")
                calls=[item for item in json.loads(body)["output"] if item["type"]=="function_call"]
                check(len(calls)==1 and json.loads(calls[0]["arguments"])==case["arguments"],"Only the corrected client arguments reach the API client")
                case.clear(); case.update(index=0, arguments={}, turns=0, native=True)
                status, body = request("/v1/responses", {"model": MODEL, "input": "Synthetic native boundary check", "tools": [{"type": "function", "name": "write_todos", "parameters": simple}]})
                check(status == 502 and json.loads(body)["error"]["code"] == "provider_native_tool_violation", "Actual native tool steps remain rejected by the model profile")
            except Exception:
                errors.seek(0)
                print("Synthetic gateway diagnostics:", errors.read().decode()[-5000:])
                try:
                    print("Synthetic capabilities:", request("/v1/capabilities")[1].decode()[-3000:])
                except Exception:
                    pass
                raise
            finally:
                gateway.send_signal(signal.SIGTERM)
                try:
                    gateway.wait(timeout=12)
                except subprocess.TimeoutExpired:
                    gateway.kill(); gateway.wait()
                model.shutdown()
                errors.seek(0)
                logs = errors.read().decode()
                check("synthetic-private-cli-argument" not in logs,"Official CLI tool argument values remain absent from gateway logs")
                if gateway.returncode:
                    print(logs[-5000:])
                check(gateway.returncode == 0, "Gateway and real CLI process groups stop cleanly")
    print(f"PASS: {checks} official AGY 1.4.3/gateway/MCP checks using a loopback mock model and synthetic state")


if __name__ == "__main__":
    main()
