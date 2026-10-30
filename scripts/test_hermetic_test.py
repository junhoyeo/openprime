"""Guard checks. Run through hermetic_test.py; never start a product binary here."""
import importlib.util
import contextlib
import io
import os
import json
import subprocess
import select
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

from test_hermetic_ownership import OwnershipTest
from test_hermetic_unknown import UnknownObservationTest

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

    def run_subject(self, argv, env, root, target, timeout, inventory=None):
        # Unit fixture only: the outer real wrapper has its controller outside
        # its writable root. This local fd lets us inject parent-side failures.
        with ht.ControlDirectory(root) as control:
            return ht.run_subject(argv, env, root, target, timeout, control=control, inventory=inventory)

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
        real_write = ht.write_receipt

        def launch(*args, **kwargs):
            child = real_popen(*args, **kwargs)
            children.append(child)
            return child

        def write(control, name, value):
            if name == "command.json":
                raise OSError("injected receipt failure")
            return real_write(control, name, value)

        with mock.patch.object(ht.subprocess, "Popen", side_effect=launch), \
             mock.patch.object(ht, "write_receipt", new=write):
            with self.assertRaisesRegex(OSError, "injected receipt failure"):
                self.run_subject(["/bin/sleep", "30"], ht.build_env(self.root, sys.executable, {}),
                               self.root, self.root, 10)
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].returncode, "owned external child was not reaped")
        outcome = json.loads((self.root / "outcome.json").read_text())
        self.assertEqual(outcome["subject_exit_code"], children[0].returncode)
        self.assertEqual(outcome["wrapper_decision"], "error")
        self.assertEqual(outcome["failure_type"], "OSError")
        self.assertTrue(outcome["direct_child_reaped"])

    def test_identity_failure_and_interrupt_reap_the_actual_child(self):
        for failure in (RuntimeError("injected identity failure"), KeyboardInterrupt()):
            with self.subTest(failure=type(failure).__name__):
                case_root = self.root / type(failure).__name__
                case_root.mkdir()
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
                        self.run_subject(["/bin/sleep", "30"], ht.build_env(case_root, sys.executable, {}),
                                       case_root, case_root, 10, inventory=probe)
                self.assertEqual(len(children), 1)
                self.assertIsNotNone(children[0].returncode)
                outcome = json.loads((case_root / "outcome.json").read_text())
                self.assertEqual(outcome["subject_exit_code"], children[0].returncode)
                self.assertEqual(outcome["failure_type"], type(failure).__name__)
                self.assertTrue(outcome["direct_child_reaped"])

    def test_unattributed_host_arrival_warns_without_masking_subject_exit(self):
        # Capture before the external process exists, just as a real launch does.
        inventory = ht.ProcessInventory()
        before = inventory.snapshot()
        child = subprocess.Popen(["/bin/sleep", "30"], cwd=self.tmp)
        try:
            identity = inventory.identity(child.pid)
            self.assertIsNotNone(identity)
            after = inventory.snapshot()
            from hermetic_processes import key
            self.assertIn(key(identity), after)
            identity = after[key(identity)]
            self.assertIn(identity, ht.new_processes(before, after))
            # An unattributed process is not an ownership proof. Preserve actual
            # subject status while retaining the warning and never signalling it.
            for subject_exit in (0, 3, 7, -15):
                case_root = self.root / f"exit-{subject_exit}"
                case_root.mkdir()
                probe = mock.Mock()
                probe.snapshot.side_effect = [before, after, after]
                probe.identity.side_effect = inventory.identity
                probe.lib = inventory.lib
                command = (["/bin/sh", "-c", "kill -TERM $$"] if subject_exit == -15 else
                           [sys.executable, "-c", f"raise SystemExit({subject_exit})"])
                code = self.run_subject(command,
                                      ht.build_env(case_root, sys.executable, {}),
                                      case_root, case_root, 10, inventory=probe)
                self.assertEqual(code, subject_exit)
                outcome = json.loads((case_root / "outcome.json").read_text())
                self.assertEqual(outcome["subject_exit_code"], subject_exit)
                self.assertEqual(outcome["wrapper_exit_code"], subject_exit & 255)
                self.assertEqual(outcome["wrapper_decision"], "subject_exit_with_host_warnings")
                self.assertIn(identity, [row["identity"] for row in outcome["ownership"]["host_warnings"]])
                self.assertEqual(outcome["ownership"]["observed_owned_survivors"], [])
                self.assertEqual(outcome["ownership"]["unverified_subject_candidates"], [])
                self.assertIn(identity, outcome["survivors"])
                self.assertFalse(outcome["timed_out"])
                self.assertIsNone(child.poll(), "discovered PID must not be signalled")
        finally:
            ht.reap_owned(child)

    def test_observed_orphan_blocks_and_exits_only_via_its_owned_control_pipe(self):
        ack_path = self.root / "parent-ack.fifo"
        stop_path = self.root / "child-stop.fifo"
        pid_path = self.root / "fixture-child.pid"
        os.mkfifo(ack_path)
        os.mkfifo(stop_path)
        ack_fd = os.open(ack_path, os.O_RDWR | os.O_NONBLOCK)
        stop_fd = os.open(stop_path, os.O_RDWR | os.O_NONBLOCK)
        real = ht.ProcessInventory()

        class CooperativeInventory:
            def __init__(self):
                self.lib = real.lib
                self.parent_pid = None
                self.child_identity = None
                self.watcher = None

            def identity(self, pid):
                identity = real.identity(pid)
                if self.parent_pid is None:
                    self.parent_pid = pid
                return identity

            def snapshot(self):
                observed = real.snapshot()
                if self.parent_pid is not None and self.child_identity is None:
                    for identity in observed.values():
                        if identity["ppid"] == self.parent_pid and identity.get("topology_stable", True):
                            self.child_identity = identity
                            self.watcher = select.kqueue()
                            event = select.kevent(identity["pid"], filter=select.KQ_FILTER_PROC,
                                                 flags=select.KQ_EV_ADD | select.KQ_EV_ONESHOT,
                                                 fflags=select.KQ_NOTE_EXIT)
                            self.watcher.control([event], 0, 0)
                            # Release the fixture parent only after native parent/
                            # child identities are actually observable together.
                            os.write(ack_fd, b"release")
                            break
                return observed

        probe = CooperativeInventory()
        script = (
            "import os\n"
            f"ack = os.open({str(ack_path)!r}, os.O_RDONLY)\n"
            f"stop = os.open({str(stop_path)!r}, os.O_RDONLY)\n"
            "child = os.fork()\n"
            "if child == 0:\n"
            "    os.close(ack)\n"
            "    os.setsid()\n"
            "    os.read(stop, 1)\n"
            "    os._exit(0)\n"
            "os.close(stop)\n"
            f"open({str(pid_path)!r}, 'w').write(str(child))\n"
            "os.read(ack, 1)\n"
            "os._exit(0)\n"
        )
        try:
            code = self.run_subject([sys.executable, "-c", script],
                                    ht.build_env(self.root, sys.executable, {}),
                                    self.root, self.root, 10, inventory=probe)
            self.assertEqual(code, 3)
            self.assertIsNotNone(probe.child_identity)
            outcome = json.loads((self.root / "outcome.json").read_text())
            self.assertEqual(outcome["subject_exit_code"], 0)
            self.assertEqual(outcome["wrapper_decision"], "owned_survivors")
            from hermetic_processes import key
            self.assertIn(key(probe.child_identity), {
                key(entry["identity"]) for entry in outcome["ownership"]["observed_owned_survivors"]})
            live = real.identity(probe.child_identity["pid"])
            self.assertIsNotNone(live, "the classifier must not signal an observed orphan")
            self.assertEqual(key(live), key(probe.child_identity))
            self.assertNotEqual(live["ppid"], probe.parent_pid, "the fixture parent actually exited")
        finally:
            # Never signal a census PID. The test owns this pipe from before
            # launch; its byte is the cooperative child's only exit instruction.
            os.write(stop_fd, b"stop")
            try:
                if probe.watcher is not None:
                    events = probe.watcher.control(None, 1, 5)
                    self.assertTrue(any(event.fflags & select.KQ_NOTE_EXIT for event in events),
                                    "owned fixture did not exit after its control pipe instruction")
                elif pid_path.exists():
                    # Setup failed before installing the normal watcher. Keep the
                    # failed test non-green; the outer guard retains any survivor.
                    identity = real.identity(int(pid_path.read_text()))
                    self.assertTrue(identity is None or identity.get("status") == 5,
                                    "fixture cleanup could not prove exit; retain for owned review")
            finally:
                if probe.watcher is not None:
                    probe.watcher.close()
                os.close(ack_fd)
                os.close(stop_fd)

    def test_typed_unknown_direct_live_child_fails_and_is_reaped(self):
        from hermetic_processes import UnknownObservation
        inventory = ht.ProcessInventory()
        injected = mock.Mock()
        injected.lib = inventory.lib
        injected.snapshot.side_effect = inventory.snapshot
        injected.identity.side_effect = lambda pid: UnknownObservation(pid, "bsd_1", 0, 1, "injected EPERM")
        with self.assertRaisesRegex(RuntimeError, "direct child identity is Unknown"):
            self.run_subject(["/bin/sleep", "30"], ht.build_env(self.root, sys.executable, {}),
                             self.root, self.root, 10, inventory=injected)
        outcome = json.loads((self.root / "outcome.json").read_text())
        self.assertEqual(outcome["wrapper_decision"], "ownership_tracking_failure")
        self.assertTrue(outcome["direct_child_reaped"])
        self.assertEqual(outcome["launch_observation"]["errno"], 1)
        self.assertEqual(outcome["launch_observation"]["kind"], "unknown")
        self.assertLess(outcome["subject_exit_code"], 0)

    def test_host_only_typed_unknown_preserves_child_exit_and_receipt_failures(self):
        from hermetic_processes import ProcessSnapshot, UnknownObservation
        inventory = ht.ProcessInventory()
        native_inspect = ht.ReferenceProbe.inspect
        unavailable = UnknownObservation(2_147_483_000, "bsd_1", 0, 1, "synthetic EPERM")
        def inspect(probe, observation):
            if isinstance(observation, UnknownObservation):
                return {"matches": [], "errors": [observation.receipt()]}
            return native_inspect(probe, observation)
        for exit_code in (0, 3, 7, -15):
            case_root = self.root / f"typed-{exit_code}"
            case_root.mkdir()
            def scan():
                result = inventory.snapshot()
                result.unknown[unavailable.pid] = unavailable
                return result
            injected = mock.Mock()
            injected.lib = inventory.lib
            injected.identity.side_effect = inventory.identity
            injected.snapshot.side_effect = scan
            command = (["/bin/sh", "-c", "kill -TERM $$"] if exit_code == -15 else
                       [sys.executable, "-c", f"raise SystemExit({exit_code})"])
            with mock.patch.object(ht.ReferenceProbe, "inspect", new=inspect):
                code = self.run_subject(command, ht.build_env(case_root, sys.executable, {}),
                                        case_root, case_root, 10, inventory=injected)
            self.assertEqual(code, exit_code)
            outcome = json.loads((case_root / "outcome.json").read_text())
            self.assertEqual(outcome["subject_exit_code"], exit_code)
            self.assertEqual(outcome["wrapper_decision"], "subject_exit_with_host_warnings")
            self.assertIn(unavailable.receipt(), outcome["last_observation"]["unknown"])
            self.assertIn(unavailable.pid, [entry["unknown"]["pid"] for entry in outcome["ownership"]["host_warnings"] if "unknown" in entry])
        failure_root = self.root / "typed-receipt-error"
        failure_root.mkdir()
        original_write = ht.write_receipt
        def fail_command(control, name, value):
            if name == "command.json":
                raise OSError("typed receipt failure")
            return original_write(control, name, value)
        with mock.patch.object(ht, "write_receipt", new=fail_command), \
             mock.patch.object(ht.ReferenceProbe, "inspect", new=inspect), \
             self.assertRaisesRegex(OSError, "typed receipt failure"):
            self.run_subject(["/bin/sleep", "30"], ht.build_env(failure_root, sys.executable, {}),
                             failure_root, failure_root, 10, inventory=injected)
        outcome = json.loads((failure_root / "outcome.json").read_text())
        self.assertTrue(outcome["direct_child_reaped"])
        self.assertEqual(outcome["failure_type"], "OSError")
        self.assertIn(unavailable.receipt(), outcome["last_observation"]["unknown"])
        cleanup_root = self.root / "typed-cleanup-error"
        cleanup_root.mkdir()
        original_reap = ht.reap_owned
        def fail_after_owned_reap(child):
            original_reap(child)
            raise OSError("typed cleanup reporting failure")
        with mock.patch.object(ht, "reap_owned", new=fail_after_owned_reap), \
             mock.patch.object(ht.ReferenceProbe, "inspect", new=inspect), \
             self.assertRaisesRegex(OSError, "typed cleanup reporting failure"):
            self.run_subject(["/usr/bin/true"], ht.build_env(cleanup_root, sys.executable, {}),
                             cleanup_root, cleanup_root, 10, inventory=injected)
        outcome = json.loads((cleanup_root / "outcome.json").read_text())
        self.assertEqual(outcome["cleanup_failure_type"], "OSError")
        self.assertTrue(outcome["direct_child_reaped"])
        self.assertNotEqual(outcome["wrapper_exit_code"], 0)
        self.assertIn(unavailable.receipt(), outcome["last_observation"]["unknown"])

    def test_command_timeout_records_actual_reaped_exit(self):
        code = self.run_subject(["/bin/sleep", "30"], ht.build_env(self.root, sys.executable, {}),
                              self.root, self.root, 0.01)
        self.assertEqual(code, 124)
        outcome = json.loads((self.root / "outcome.json").read_text())
        self.assertTrue(outcome["timed_out"])
        self.assertTrue(outcome["direct_child_reaped"])
        self.assertLess(outcome["subject_exit_code"], 0)
        self.assertEqual(outcome["wrapper_decision"], "timeout")
        self.assertIsNone(outcome["failure_type"])

    def test_log_tail_does_not_move_an_inherited_writer_offset(self):
        with ht.ControlDirectory(self.root) as control, control.create_file("shared.log") as output:
            output.write(b"before")
            output.flush()
            os.lseek(output.fileno(), 3, os.SEEK_SET)
            writer = os.dup(output.fileno())
            try:
                self.assertEqual(ht.read_log_tail(output), b"before")
                self.assertEqual(os.lseek(writer, 0, os.SEEK_CUR), 3)
                os.write(writer, b"X")
                self.assertEqual(ht.read_log_tail(output), b"befXre")
            finally:
                os.close(writer)

    def test_large_log_tail_is_bounded_and_full_log_is_retained(self):
        payload = b"01234567" * (200 * 1024 // 8)
        with ht.ControlDirectory(self.root) as control, control.create_file("large.log") as output:
            output.write(payload)
            output.flush()
            os.lseek(output.fileno(), 17, os.SEEK_SET)
            self.assertEqual(ht.read_log_tail(output), payload[-128 * 1024:])
            self.assertEqual(os.lseek(output.fileno(), 0, os.SEEK_CUR), 17)
            self.assertEqual(os.fstat(output.fileno()).st_size, len(payload))
            self.assertEqual(os.pread(output.fileno(), len(payload), 0), payload)

    def test_parent_receipt_writer_refuses_an_owned_symlink_canary(self):
        forbidden = self.tmp / "forbidden-receipt-canary"
        forbidden.write_text("preserve")
        link = self.root / "command.json"
        link.symlink_to(forbidden)
        with ht.ControlDirectory(self.root) as control, self.assertRaises(FileExistsError):
            ht.write_receipt(control, "command.json", {"unsafe": "must not overwrite the canary"})
        self.assertEqual(forbidden.read_text(), "preserve")
        self.assertTrue(link.is_symlink(), "existing evidence is retained")

    def test_parent_receipt_stays_on_held_directory_after_ancestor_replacement(self):
        original = self.tmp / "controller"
        original.mkdir()
        moved = self.tmp / "moved-controller"
        forbidden = self.tmp / "forbidden-controller-canary"
        forbidden.mkdir()
        (forbidden / "outcome.json").write_text("preserve")
        with ht.ControlDirectory(original) as control:
            original.rename(moved)
            original.symlink_to(forbidden, target_is_directory=True)
            ht.write_receipt(control, "outcome.json", {"held_inode": True})
        self.assertEqual((forbidden / "outcome.json").read_text(), "preserve")
        self.assertEqual(json.loads((moved / "outcome.json").read_text()), {"held_inode": True})
        self.assertTrue(original.is_symlink(), "retain synthetic redirection evidence")

    def test_outcome_receipt_cannot_silently_overwrite_existing_evidence(self):
        (self.root / "outcome.json").write_text("preserve")
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), self.assertRaises(FileExistsError):
            self.run_subject(["/usr/bin/true"], ht.build_env(self.root, sys.executable, {}),
                           self.root, self.root, 10)
        self.assertEqual((self.root / "outcome.json").read_text(), "preserve")
        fallback = json.loads(stderr.getvalue().split("fallback ", 1)[1])
        self.assertEqual(fallback["subject_exit_code"], 0)
        self.assertEqual(fallback["wrapper_exit_code"], 1)
        self.assertEqual(fallback["wrapper_decision"], "outcome_receipt_error")
        self.assertEqual(fallback["outcome_receipt_failure_type"], "FileExistsError")

    def test_unknown_inventory_is_not_reported_as_no_survivors(self):
        inventory = mock.Mock()
        inventory.snapshot.side_effect = RuntimeError("injected inventory failure")
        with mock.patch.object(ht.subprocess, "Popen") as launch:
            with self.assertRaisesRegex(RuntimeError, "injected inventory failure"):
                self.run_subject(["/usr/bin/true"], {}, self.root, self.root, 10, inventory=inventory)
            launch.assert_not_called()
        outcome = json.loads((self.root / "outcome.json").read_text())
        self.assertEqual(outcome["wrapper_decision"], "ownership_tracking_failure")
        self.assertEqual(outcome["tracking_failure_type"], "RuntimeError")
        self.assertIsNone(outcome["subject_exit_code"])


if __name__ == "__main__":
    unittest.main()
