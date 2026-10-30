"""Guard checks. Run through hermetic_test.py; never start a product binary here."""
import importlib.util
import os
import subprocess
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

    def test_target_is_explicit_canonical_private_and_not_a_broad_ancestor(self):
        self.assertEqual(ht.private_target(os.environ), Path(os.environ["CARGO_TARGET_DIR"]))
        for value in (None, "/private/tmp", "/private/tmp/pa-test", str(self.root / "target"),
                      str(Path(os.environ["CARGO_TARGET_DIR"]) / ".." / "target")):
            with self.subTest(value=value), self.assertRaises(SystemExit):
                ht.private_target({} if value is None else {"CARGO_TARGET_DIR": value})

    def test_nonfinite_or_nonpositive_deadline_is_refused(self):
        for value in ("nan", "inf", "-inf", "0", "-1"):
            with self.subTest(value=value), self.assertRaises(SystemExit):
                ht.main(["--timeout=" + value, "--kernel-python", str(self.python), "--", "true"])

    def test_receipt_write_failure_reaps_the_actual_child(self):
        children = []
        real_popen = subprocess.Popen
        real_write = Path.write_text

        def launch(*args, **kwargs):
            child = real_popen(*args, **kwargs)
            children.append(child)
            return child

        def write(path, *args, **kwargs):
            if path.name == "command.json":
                raise OSError("injected receipt failure")
            return real_write(path, *args, **kwargs)

        with mock.patch.object(ht.subprocess, "Popen", side_effect=launch), \
             mock.patch.object(Path, "write_text", new=write):
            with self.assertRaisesRegex(OSError, "injected receipt failure"):
                ht.run_subject(["/bin/sleep", "30"], ht.build_env(self.root, sys.executable, {}),
                               self.root, self.root, 10)
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].returncode, "owned external child was not reaped")

    def test_identity_failure_and_interrupt_reap_the_actual_child(self):
        for failure in (RuntimeError("injected identity failure"), KeyboardInterrupt()):
            with self.subTest(failure=type(failure).__name__):
                children = []
                real_popen = subprocess.Popen
                inventory = ht.ProcessInventory()
                probe = mock.Mock()
                probe.snapshot.side_effect = inventory.snapshot
                probe.identity.side_effect = failure

                def launch(*args, **kwargs):
                    child = real_popen(*args, **kwargs)
                    children.append(child)
                    return child

                with mock.patch.object(ht.subprocess, "Popen", side_effect=launch):
                    with self.assertRaises(type(failure)):
                        ht.run_subject(["/bin/sleep", "30"], ht.build_env(self.root, sys.executable, {}),
                                       self.root, self.root, 10, inventory=probe)
                self.assertEqual(len(children), 1)
                self.assertIsNotNone(children[0].returncode)

    def test_external_executable_survivor_is_unknown_and_never_killed(self):
        # Capture before the external process exists, just as a real launch does.
        inventory = ht.ProcessInventory()
        before = inventory.snapshot()
        child = subprocess.Popen(["/bin/sleep", "30"])
        try:
            identity = inventory.identity(child.pid)
            self.assertIsNotNone(identity)
            after = inventory.snapshot()
            self.assertIn(identity, ht.new_processes(before, after))
            # Feed the real census into the subject-result branch. Unlike argv[0]
            # marker matching, this reports the external executable as UNKNOWN.
            probe = mock.Mock()
            probe.snapshot.side_effect = [before, after]
            probe.identity.side_effect = inventory.identity
            code = ht.run_subject(["/usr/bin/true"], ht.build_env(self.root, sys.executable, {}),
                                  self.root, self.root, 10, inventory=probe)
            self.assertEqual(code, 3)
            self.assertIsNone(child.poll(), "discovered PID must not be signalled")
        finally:
            ht.reap_owned(child)

    def test_unknown_inventory_is_not_reported_as_no_survivors(self):
        inventory = mock.Mock()
        inventory.snapshot.side_effect = RuntimeError("injected inventory failure")
        with mock.patch.object(ht.subprocess, "Popen") as launch:
            with self.assertRaisesRegex(RuntimeError, "injected inventory failure"):
                ht.run_subject(["/usr/bin/true"], {}, self.root, self.root, 10, inventory=inventory)
            launch.assert_not_called()


if __name__ == "__main__":
    unittest.main()
