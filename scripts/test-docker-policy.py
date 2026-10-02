#!/usr/bin/env python3
"""Check Docker exposure and context exclusions using synthetic fixtures only."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid

repo = Path(__file__).resolve().parent.parent
dummy_env = dict(os.environ)
dummy_env.update({
    "AI_ROUTER_KEYS": json.dumps([{"id": "policy-test", "sha256": "0" * 64, "scopes": ["model"]}]),
    "AI_ROUTER_PORT": "8090",
    "AI_ROUTER_TRUSTED_PROXIES": "",
})
config = json.loads(subprocess.check_output(
    ["docker", "compose", "--env-file", "/dev/null", "-f", str(repo / "compose.yaml"),
     "config", "--format", "json"], env=dummy_env, text=True))
service = config["services"]["ai-router"]
assert service["user"] == "10001:10001"
assert service["read_only"] is True
assert service.get("privileged", False) is False
assert service["cap_drop"] == ["ALL"]
assert service["security_opt"] == ["no-new-privileges:true"]
assert service["pids_limit"] == 256
assert int(service["mem_limit"]) == 2 * 1024 * 1024 * 1024
assert len(service["ports"]) == 1
assert service["ports"][0]["host_ip"] == "127.0.0.1"
assert service["ports"][0]["published"] == "8090"
assert all(mount["type"] == "volume" for mount in service["volumes"])
assert {mount["target"] for mount in service["volumes"]} == {
    "/var/lib/ai-router/auth", "/var/lib/ai-router/telemetry"}
assert service["environment"]["AI_ROUTER_NATIVE_ENABLED"] == "false"

image = "ai-router-context-policy:" + uuid.uuid4().hex
with tempfile.TemporaryDirectory(prefix="ai-router-context-policy-") as tmp:
    context = Path(tmp)
    # Copy only this safe configuration file. No real project secrets are read.
    (context / ".dockerignore").write_text((repo / ".dockerignore").read_text())
    protected = [".env.synthetic", ".env.example", "dummy.pem", "dummy.key",
                 "nested/.env.synthetic", "nested/dummy.pem", "nested/dummy.key",
                 "secrets/dummy", "credentials/dummy", "nested/secrets/dummy",
                 "nested/credentials/dummy"]
    for name in protected:
        path = context / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("synthetic dummy; never a real credential\n")
    (context / "allowed.txt").write_text("synthetic public fixture\n")
    checks = " && ".join("test ! -e /policy/" + name for name in protected)
    (context / "Dockerfile").write_text(
        "FROM ai-router-agy:local\nCOPY . /policy\nRUN test -e /policy/allowed.txt && " + checks + "\n")
    try:
        subprocess.run(["docker", "build", "-t", image, str(context)], check=True)
    finally:
        subprocess.run(["docker", "image", "rm", image], check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
print("Docker policy checks passed: loopback, nonroot, named volumes, protected context paths excluded")
