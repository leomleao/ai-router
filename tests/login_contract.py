#!/usr/bin/env python3
"""Exercise provider login helpers against a synthetic Docker executable."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[1]
HELPER = REPO / "scripts/login.sh"


class LoginContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="ai-router-login-contract-")
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.capture = self.root / "capture.json"
        docker = self.bin / "docker"
        docker.write_text("""#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
if args[:1] == ["inspect"]:
    print(os.environ.get("MOCK_GATEWAY_STATUS", "stopped"))
else:
    with open(os.environ["MOCK_CAPTURE"], "w") as f:
        json.dump({"args": args, "env": {k:v for k,v in os.environ.items() if k.startswith("AI_ROUTER_")}}, f)
""")
        docker.chmod(0o755)
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("AI_ROUTER_")}
        self.env.update({
            "PATH": str(self.bin) + os.pathsep + self.env.get("PATH", ""),
            "MOCK_CAPTURE": str(self.capture),
            "AI_ROUTER_IMAGE": "synthetic-router:test",
            "AI_ROUTER_GOOGLE_CLIENT_ID": "synthetic-client-id",
            "AI_ROUTER_GOOGLE_CLIENT_SECRET": "synthetic-client-secret",
        })
        self.env.pop("AI_ROUTER_OPENAI_CALLBACK_PORT", None)
        self.env.pop("AI_ROUTER_ANTIGRAVITY_CALLBACK_PORT", None)
        self.env.pop("AI_ROUTER_KEYS", None)
        # This dummy template is deliberately never loaded by the helper.
        (self.root / ".env").write_text("AI_ROUTER_KEYS=must-not-be-loaded\n")

    def tearDown(self):
        self.tmp.cleanup()

    def invoke(self, *args, status="stopped"):
        return subprocess.run([str(HELPER), *args], cwd=self.root, env={
            **self.env, "MOCK_GATEWAY_STATUS": status,
        }, text=True, capture_output=True)

    def captured(self):
        return json.loads(self.capture.read_text())

    def test_agy_login_uses_dedicated_volume_loopback_and_named_oauth_env(self):
        result = self.invoke("agy", "login", "--port", "1457")
        self.assertEqual(result.returncode, 0, result.stderr)
        call = self.captured()
        args = call["args"]
        self.assertEqual(args[-4:], ["antigravity", "login", "--port", "1457"])
        self.assertIn("type=volume,source=ai-router-antigravity-auth,target=/var/lib/ai-router/antigravity-auth", args)
        self.assertIn("127.0.0.1:1457:1457", args)
        self.assertIn("never", args)
        self.assertIn("--env", args)
        self.assertIn("AI_ROUTER_GOOGLE_CLIENT_ID", args)
        self.assertIn("AI_ROUTER_GOOGLE_CLIENT_SECRET", args)
        self.assertNotIn("AI_ROUTER_KEYS", call["env"])
        self.assertNotIn("must-not-be-loaded", json.dumps(call))

    def test_openai_login_and_readonly_are_isolated(self):
        result = self.invoke("openai", "login", "--new")
        self.assertEqual(result.returncode, 0, result.stderr)
        args = self.captured()["args"]
        self.assertEqual(args[-7:], ["openai", "login", "--listen", "0.0.0.0",
                                     "--port", "1455", "--new"])
        self.assertIn("type=volume,source=ai-router-openai-auth,target=/var/lib/ai-router/openai-auth", args)
        self.assertIn("127.0.0.1:1455:1455", args)
        self.assertIn("never", args)
        self.assertIn("--new", args)
        self.assertNotIn("AI_ROUTER_GOOGLE_CLIENT_SECRET", args)

        result = self.invoke("openai", "login", "--account", "acct-1")
        self.assertEqual(result.returncode, 0, result.stderr)
        args = self.captured()["args"]
        self.assertEqual(args[-8:], ["openai", "login", "--listen", "0.0.0.0",
                                     "--port", "1455", "--account", "acct-1"])

        result = self.invoke("openai", "accounts")
        self.assertEqual(result.returncode, 0, result.stderr)
        args = self.captured()["args"]
        self.assertIn("--network", args)
        self.assertIn("none", args)
        self.assertIn("type=volume,source=ai-router-openai-auth,target=/var/lib/ai-router/openai-auth", args)
        self.assertEqual(args[-2:], ["openai", "accounts"])

    def test_status_uses_exec_while_running_and_mutations_refuse(self):
        result = self.invoke("agy", "status", status="running")
        self.assertEqual(result.returncode, 0, result.stderr)
        args = self.captured()["args"]
        self.assertEqual(args[:2], ["exec", "--user"])
        self.assertEqual(args[-2:], ["antigravity", "status"])
        self.capture.unlink()
        for status in ("running", "paused", "restarting"):
            result = self.invoke("openai", "logout", status=status)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Stop ai-router", result.stderr)
            self.assertIn('docker stop "ai-router"', result.stderr)
            self.assertFalse(self.capture.exists())

    def test_bad_arguments_never_invoke_docker(self):
        for args in (("agy", "login", "--port", "0"),
                     ("openai", "login", "--port", "1455;bad"),
                     ("openai", "login", "--new", "--account", "acct-1"),
                     ("agy", "login", "--port", "1456", "--port", "1457"),
                     ("openai", "select"), ("agy", "accounts"),
                     ("openai", "status", "unexpected")):
            with self.subTest(args=args):
                result = self.invoke(*args)
                self.assertEqual(result.returncode, 2)
                self.assertFalse(self.capture.exists())

    def test_all_mutating_actions_use_binary_commands_and_private_volumes(self):
        cases = [
            (("agy", "logout"), ["antigravity", "logout"], "antigravity-auth"),
            (("agy", "resume"), ["antigravity", "resume"], "antigravity-auth"),
            (("openai", "logout"), ["openai", "logout"], "openai-auth"),
            (("openai", "resume"), ["openai", "resume"], "openai-auth"),
            (("openai", "select", "acct-9"), ["openai", "select", "acct-9"], "openai-auth"),
        ]
        for command, cli_args, volume in cases:
            with self.subTest(command=command):
                result = self.invoke(*command)
                self.assertEqual(result.returncode, 0, result.stderr)
                args = self.captured()["args"]
                self.assertEqual(args[-len(cli_args):], cli_args)
                self.assertIn(f"/var/lib/ai-router/{volume}", json.dumps(args))
                self.assertNotIn("--publish", args)
                self.assertIn("--network", args)
                network = "bridge" if command[0] == "openai" and command[1] in ("logout", "resume") else "none"
                self.assertEqual(args[args.index("--network") + 1], network)


if __name__ == "__main__":
    unittest.main()
