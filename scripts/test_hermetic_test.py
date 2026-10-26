"""Unit checks for scripts/hermetic_test.py (no cargo, no product binary)."""
import importlib.util
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("hermetic_test", Path(__file__).with_name("hermetic_test.py"))
ht = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ht)


class HermeticTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="pa-ht", dir="/tmp"))
        self.python = self.tmp / "python"
        self.python.write_text("")

    def test_env_is_scrubbed_and_confined(self):
        root = ht.make_root()
        environ = {"HOME": "/Users/someone", "PATH": "/usr/bin", "ANTHROPIC_API_KEY": "secret",
                   "HTTPS_PROXY": "http://proxy", "PRIME_AGENT_CODING_AGENT_DIR": "/Users/someone/.prime/agent",
                   "CARGO_TARGET_DIR": "/private/target"}
        with mock.patch.object(ht, "real_home", return_value=Path("/Users/someone")):
            env = ht.build_env(root, str(self.python), environ)
        self.assertEqual(env["HOME"], str(root / "h"))
        self.assertEqual(env["TMPDIR"], str(root / "t"))
        self.assertEqual(env["PRIME_AGENT_KERNEL_VENV"], str(root / "k"))
        for key in ht.KERNEL_VARS:
            self.assertEqual(env[key], str(self.python))
        for key in ("ANTHROPIC_API_KEY", "HTTPS_PROXY", "PRIME_AGENT_CODING_AGENT_DIR"):
            self.assertNotIn(key, env)
        self.assertEqual(env["CARGO_TARGET_DIR"], "/private/target")
        self.assertEqual(env["CARGO_HOME"], "/Users/someone/.cargo")
        self.assertLess(len(str(root / "t" / "prime-agent-501" / "worker-0123456789ab-ffffffffffff.sock")), 104)

    def test_kernel_python_is_required_without_fallback(self):
        with self.assertRaises(SystemExit):
            ht.resolve_kernel_python(None, {})
        with self.assertRaises(SystemExit):
            ht.resolve_kernel_python(str(self.tmp / "missing"), {})
        self.assertEqual(ht.resolve_kernel_python(None, {"PA_E2E_KERNEL_PYTHON": str(self.python)}), str(self.python))

    def test_live_install_interpreter_is_refused(self):
        live = self.tmp / "home" / ".prime" / "agent" / "kernel-venv" / "bin"
        live.mkdir(parents=True)
        (live / "python").write_text("")
        with mock.patch.object(ht, "real_home", return_value=self.tmp / "home"):
            with self.assertRaises(SystemExit):
                ht.resolve_kernel_python(str(live / "python"), {})

    def test_command_runs_in_the_hermetic_env_and_reports_no_survivors(self):
        out = self.tmp / "out"
        code = ht.main(["--kernel-python", str(self.python), "--", sys.executable, "-c",
                        f"import os; open({str(out)!r}, 'w').write(os.environ['HOME'])"])
        self.assertEqual(code, 0)
        self.assertTrue(out.read_text().startswith(os.path.realpath("/tmp") + "/pa-h"))

    @unittest.skipUnless(sys.platform == "darwin", "macOS sandbox profile")
    def test_external_egress_is_denied_and_loopback_allowed(self):
        out = self.tmp / "net"
        code = ht.main(["--kernel-python", str(self.python), "--", sys.executable, "-c",
            "import socket,http.server,threading\n"
            "s=http.server.HTTPServer(('127.0.0.1',0),http.server.SimpleHTTPRequestHandler)\n"
            "threading.Thread(target=s.handle_request,daemon=True).start()\n"
            "socket.create_connection(('127.0.0.1',s.server_address[1]),timeout=5).close()\n"
            "try:\n socket.create_connection(('1.1.1.1',443),timeout=5); r='allowed'\n"
            "except PermissionError: r='denied'\n"
            f"open({str(out)!r},'w').write(r)"])
        self.assertEqual(code, 0)
        self.assertEqual(out.read_text(), "denied")

    def test_leftover_report_matches_executables_not_mentions(self):
        listing = ("10 1 /private/target/debug/pa-daemon worker\n"
                   "11 1 /bin/bash -c cd /private/target && cargo test\n"
                   f"12 1 {self.python} -m rlm.repl\n")
        with mock.patch.object(ht.subprocess, "run", return_value=mock.Mock(stdout=listing)):
            found = ht.leftovers(["/private/target", str(self.python)])
        self.assertEqual([row.split()[0] for row in found], ["10", "12"])

    def test_exit_code_passes_through(self):
        code = ht.main(["--kernel-python", str(self.python), "--", sys.executable, "-c", "raise SystemExit(7)"])
        self.assertEqual(code, 7)


if __name__ == "__main__":
    unittest.main()
