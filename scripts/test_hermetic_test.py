"""Guard checks. Run through hermetic_test.py; never start a product binary here."""
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
        ht.GRACE_SECONDS = 0
        self.tmp = Path(tempfile.mkdtemp(prefix="guard-", dir=os.environ["TMPDIR"])).resolve()
        self.root = self.tmp / "run"
        self.root.mkdir()
        for name in ("h", "t", "x", "c", "cache", "pycache"):
            (self.root / name).mkdir()
        self.python = self.tmp / "python"
        self.python.write_text("")

    def test_env_is_scrubbed_and_keeps_per_test_home_resolution(self):
        environ = {"HOME": "/fake/home", "PATH": "/usr/bin", "ANTHROPIC_API_KEY": "secret",
                   "HTTPS_PROXY": "http://proxy", "PRIME_AGENT_CODING_AGENT_DIR": "/live",
                   "PRIME_AGENT_SESSION_DIR": "/live/sessions", "PRIME_AGENT_KERNEL_VENV": "/live/venv"}
        env = ht.build_env(self.root, str(self.python), environ)
        self.assertEqual(env["HOME"], str(self.root / "h"))
        self.assertEqual(env["TMPDIR"], str(self.root / "t"))
        for key in ht.KERNEL_VARS:
            self.assertEqual(env[key], str(self.python))
        for key in ("ANTHROPIC_API_KEY", "HTTPS_PROXY", "PRIME_AGENT_CODING_AGENT_DIR",
                    "PRIME_AGENT_SESSION_DIR", "PRIME_AGENT_KERNEL_VENV"):
            self.assertNotIn(key, env)

    def test_owner_home_does_not_trust_synthetic_home(self):
        with mock.patch.dict(os.environ, {"HOME": str(self.tmp)}):
            self.assertEqual(ht.real_home(), Path(ht.pwd.getpwuid(os.getuid()).pw_dir).resolve())

    def test_kernel_python_is_required_without_fallback(self):
        with self.assertRaises(SystemExit):
            ht.resolve_kernel_python(None, {})
        with self.assertRaises(SystemExit):
            ht.resolve_kernel_python(str(self.tmp / "missing"), {})
        self.assertEqual(ht.resolve_kernel_python(None, {"PA_E2E_KERNEL_PYTHON": str(self.python)}), str(self.python))

    def test_live_and_trial_interpreters_are_refused_even_with_fake_home(self):
        for name in (".prime", ".prime-rust"):
            live = self.tmp / "owner" / name / "bin"
            live.mkdir(parents=True)
            (live / "python").write_text("")
            with mock.patch.object(ht, "real_home", return_value=self.tmp / "owner"):
                with self.assertRaises(SystemExit):
                    ht.resolve_kernel_python(str(live / "python"), {"HOME": "/fake"})

    def test_no_platform_fence_means_refusal_even_with_dnt(self):
        with mock.patch.object(ht.platform, "system", return_value="Linux"):
            for opt in ([], ["--do-not-track"]):
                with self.assertRaises(SystemExit):
                    ht.main([*opt, "--kernel-python", str(self.python), "--", sys.executable, "-c", "pass"])

    def test_leftover_report_matches_executables_not_mentions(self):
        listing = ("10 1 /private/target/debug/pa-daemon worker\n"
                   "11 1 /bin/bash -c cd /private/target && cargo test\n"
                   f"12 1 {self.python} -m rlm.repl\n")
        with mock.patch.object(ht.subprocess, "run", return_value=mock.Mock(stdout=listing, returncode=0)):
            found = ht.leftovers(["/private/target", str(self.python)])
        self.assertEqual([row.split()[0] for row in found], ["10", "12"])


if __name__ == "__main__":
    unittest.main()
