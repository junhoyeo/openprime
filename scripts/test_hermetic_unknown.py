"""Injected per-row Unknown regimes; no protected host process fixtures."""
import ctypes
import errno
import unittest
from unittest import mock

from hermetic_processes import BsdInfo, ProcessInventory, ProcessSnapshot, UnknownObservation, key
from hermetic_ownership import OwnershipTracker
from test_hermetic_ownership import row, snapshot


class NativeFixture:
    def __init__(self, pids, results=None):
        self.pids = pids
        self.results = results or {}
        self.calls = []
        self.list_calls = 0

    def proc_listpids(self, _kind, _uid, buffer, size):
        self.list_calls += 1
        if buffer is not None:
            for index, pid in enumerate(self.pids):
                buffer[index] = pid
        return len(self.pids) * ctypes.sizeof(ctypes.c_int)

    def proc_pidinfo(self, pid, _kind, _arg, buffer, _size):
        self.calls.append(pid)
        planned = self.results.get(pid)
        result = planned.pop(0) if planned else {}
        if isinstance(result, BaseException):
            raise result
        ctypes.set_errno(result.get("errno", 0))
        size = result.get("size", ctypes.sizeof(BsdInfo))
        if size == ctypes.sizeof(BsdInfo):
            info = ctypes.cast(buffer, ctypes.POINTER(BsdInfo)).contents
            info.pid = result.get("pid", pid)
            info.ppid = result.get("ppid", 1)
            info.pgid = result.get("pgid", pid)
            info.uid = 501
            info.start_seconds = result.get("birth", 10)
            info.start_microseconds = 5
            info.status = 2
        return size


def inventory_with(native):
    inventory = object.__new__(ProcessInventory)
    inventory.lib = native
    inventory.sequence = 0
    return inventory


def unknown(pid, *, partial=None, stage="bsd_1"):
    return UnknownObservation(pid, stage, 0, errno.EPERM, "injected denial", partial or [])


def typed(*known, missing=(), gone=()):
    result = ProcessSnapshot(listed=[entry["pid"] for entry in known] + [entry.pid for entry in missing])
    result.update(snapshot(*known))
    result.unknown = {entry.pid: entry for entry in missing}
    result.gone = set(gone)
    result.complete = True
    return result


def probe(returned=None):
    result = mock.Mock()
    result.inventory.identity.return_value = returned
    result.inspect.return_value = {"matches": [], "errors": []}
    return result


class UnknownObservationTest(unittest.TestCase):
    def setUp(self):
        self.subject = row(100, 50, 10, sid=100, pgid=100)

    def test_eperm_preserves_earlier_known_rows_and_inspects_later_rows(self):
        native = NativeFixture([10, 11, 12], {11: [{"size": 0, "errno": errno.EPERM}]})
        with mock.patch("hermetic_processes.os.getsid", return_value=1):
            observed = inventory_with(native).snapshot()
        self.assertEqual([entry["pid"] for entry in observed.values()], [10, 12])
        self.assertEqual(native.calls, [10, 10, 11, 12, 12])
        self.assertEqual(observed.unknown[11].receipt(), {
            "kind": "unknown", "pid": 11, "stage": "bsd_1", "size": 0,
            "errno": errno.EPERM, "reason": "BSD identity unavailable", "partial": []})
        self.assertNotIn(11, observed.gone)
        self.assertTrue(observed.complete)
        result = OwnershipTracker(100, self.subject).classify({}, observed, probe())
        self.assertIn(11, [entry["unknown"]["pid"] for entry in result["host_warnings"] if "unknown" in entry])

    def test_prior_owned_and_related_unknown_preserve_proof_and_block(self):
        for relationship in ("owned", "related"):
            with self.subTest(relationship=relationship):
                child = row(101, 100 if relationship == "owned" else 1, 11, sid=100, pgid=100)
                tracker = OwnershipTracker(100, self.subject)
                tracker.observe(typed(self.subject, child))
                result = tracker.classify({}, typed(missing=[unknown(101)]), probe())
                candidates = [entry for entry in result["unverified_subject_candidates"] if "unknown" in entry]
                self.assertEqual([entry["unknown"]["pid"] for entry in candidates], [101])
                self.assertTrue(candidates[0]["historical_proof"])
                self.assertIn(key(child), tracker.owned if relationship == "owned" else tracker.related)

    def test_failed_second_bsd_and_sid_keep_positive_partial_topology(self):
        for failure_stage in ("bsd_2", "sid_1", "sid_2"):
            with self.subTest(stage=failure_stage):
                plan = [{"ppid": 100, "pgid": 100}]
                plan.append({"size": 0, "errno": errno.EPERM} if failure_stage == "bsd_2" else {"ppid": 100, "pgid": 100})
                native = NativeFixture([101], {101: plan})
                sid = ([PermissionError(errno.EPERM, "injected")] if failure_stage == "sid_1" else
                       [100, PermissionError(errno.EPERM, "injected")] if failure_stage == "sid_2" else [100])
                with mock.patch("hermetic_processes.os.getsid", side_effect=sid):
                    result = inventory_with(native).identity(101)
                self.assertIsInstance(result, UnknownObservation)
                self.assertEqual(result.stage, failure_stage)
                self.assertEqual(result.partial[0]["ppid"], 100)
                classified = OwnershipTracker(100, self.subject).classify({}, typed(missing=[result]), probe())
                self.assertEqual(len(classified["unverified_subject_candidates"]), 1)
                self.assertEqual(classified["host_warnings"], [])

    def test_child_of_unknown_historical_parent_remains_candidate(self):
        parent = row(101, 100, 11, sid=100, pgid=100)
        child = row(102, 101, 12, sid=102, pgid=102)
        tracker = OwnershipTracker(100, self.subject)
        tracker.observe(typed(self.subject, parent))
        result = tracker.classify({}, typed(child, missing=[unknown(101)]), probe())
        self.assertIn(child, [entry["identity"] for entry in result["unverified_subject_candidates"] if "identity" in entry])
        self.assertNotIn(key(child), tracker.owned)
        partial_parent = unknown(103, partial=[{"ppid": 100}])
        indirect_child = row(104, 103, 14)
        second = OwnershipTracker(100, self.subject).classify({}, typed(indirect_child, missing=[partial_parent]), probe())
        self.assertIn(indirect_child, [entry["identity"] for entry in second["unverified_subject_candidates"] if "identity" in entry])

    def test_final_unknown_does_not_disappear_through_known_set_subtraction(self):
        child = row(101, 100, 11, sid=100, pgid=100)
        tracker = OwnershipTracker(100, self.subject)
        tracker.observe(typed(self.subject, child))
        tracker.observe(typed(missing=[unknown(101)]))
        result = tracker.classify({}, typed(missing=[unknown(101)]), probe())
        self.assertTrue(result["unverified_subject_candidates"])
        self.assertTrue(result["observation_audit"])
        self.assertIn(key(child), tracker.owned)

    def test_baseline_unknown_known_unknown_and_explicit_absence_provenance(self):
        baseline = typed(missing=[unknown(105)])
        tracker = OwnershipTracker(100, self.subject)
        tracker.observe(baseline)
        known = row(105, 1, 15)
        observed = tracker.classify(baseline, typed(known), probe())
        self.assertIn(known, [entry["identity"] for entry in observed["host_warnings"]])
        self.assertTrue(any(entry.get("resolution") == "known_birth" for entry in tracker.audit))
        tracker.observe(typed(missing=[unknown(105)]))
        tracker.observe(typed(gone=[105]))
        self.assertNotIn(105, tracker.gaps)
        self.assertTrue(any(entry.get("resolution") == "explicit_absence" for entry in tracker.audit))
        self.assertEqual(list(baseline), [], "no synthetic birth key for Unknown")
        native = NativeFixture([105], {105: [{"size": 0, "errno": errno.ESRCH}]})
        absent = inventory_with(native).snapshot()
        self.assertEqual(absent.gone, {105})
        self.assertEqual(absent.unknown, {})
        self.assertEqual(absent.gone_observations[105]["stage"], "bsd_1")
        self.assertEqual(absent.gone_observations[105]["errno"], errno.ESRCH)
        self.assertEqual(absent.receipt()["schema"], "hermetic.process-observation.v1")

    def test_missing_prior_owned_and_related_each_reconciled_once(self):
        for relationship in ("owned", "related"):
            for observation_kind in ("same", "different", "gone", "unknown"):
                with self.subTest(relationship=relationship, observation=observation_kind):
                    child = row(101, 100 if relationship == "owned" else 1, 11, sid=100, pgid=100)
                    tracker = OwnershipTracker(100, self.subject)
                    tracker.observe(typed(self.subject, child))
                    current = {"same": child, "different": row(101, 1, 21), "gone": None,
                               "unknown": unknown(101)}[observation_kind]
                    inspector = probe()
                    inspector.inventory.identity.side_effect = lambda pid: current if pid == 101 else None
                    result = tracker.classify({}, typed(), inspector)
                    self.assertEqual(sum(call.args == (101,) for call in inspector.inventory.identity.call_args_list), 1)
                    self.assertIn(key(child), tracker.owned if relationship == "owned" else tracker.related)
                    if observation_kind == "unknown":
                        self.assertFalse(any(isinstance(call.args[0], UnknownObservation)
                                             for call in inspector.inspect.call_args_list),
                                         "required Unknown must not trigger reference identity rechecks")
                    blocked = bool(result["observed_owned_survivors"] or result["unverified_subject_candidates"])
                    self.assertEqual(blocked, observation_kind in ("same", "unknown"))
                    if observation_kind == "different":
                        self.assertNotIn(key(current), tracker.owned)

    def test_private_reference_with_errors_and_related_unavailable_reference_block(self):
        for related in (False, True):
            with self.subTest(related=related):
                entry = unknown(101, partial=[{"sid": 100}] if related else [])
                inspector = probe()
                inspector.inspect.return_value = {
                    "matches": [] if related else [{"kind": "subject_log_inode", "fd": 1}],
                    "errors": [{"stage": "birth_revalidation", "errno": errno.EPERM}],
                }
                result = OwnershipTracker(100, self.subject).classify({}, typed(missing=[entry]), inspector)
                self.assertEqual(len(result["unverified_subject_candidates"]), 1)
                self.assertEqual(result["host_warnings"], [])
        inspector = probe()
        inspector.inspect.return_value = {"matches": [], "errors": [unknown(101, partial=[{"ppid": 100}], stage="bsd_2").receipt()]}
        tracker = OwnershipTracker(100, self.subject)
        result = tracker.classify({}, typed(missing=[unknown(101)]), inspector)
        self.assertEqual(len(result["unverified_subject_candidates"]), 1)
        self.assertEqual(result["host_warnings"], [])
        self.assertEqual(result["observed_owned_survivors"], [])
        self.assertEqual(tracker.resolved[key(self.subject)]["resolution"], "explicit_absence")
        self.assertEqual(tracker.active_history(100), [])

    def test_partial_positive_unknown_latches_detached_known_birth(self):
        tracker = OwnershipTracker(100, self.subject)
        gap = unknown(101, partial=[{"pid": 101, "ppid": 100, "start_seconds": 11,
                                    "start_microseconds": 7}], stage="sid_1")
        tracker.observe(typed(missing=[gap]))
        detached = row(101, 1, 11, sid=101, pgid=101)
        tracker.observe(typed(detached))
        result = tracker.classify({}, typed(detached), probe())
        self.assertEqual(result["observed_owned_survivors"], [])
        self.assertEqual([entry["identity"] for entry in result["unverified_subject_candidates"]], [detached])
        self.assertEqual(result["host_warnings"], [])
        self.assertEqual(tracker.related[key(detached)]["previous_unknown"], gap.receipt())
        # A later explicit different complete birth does not inherit the latch.
        reused = row(101, 1, 21, sid=101, pgid=101)
        final = tracker.classify({}, typed(reused), probe())
        self.assertEqual(final["unverified_subject_candidates"], [])
        self.assertIn(key(detached), tracker.related, "retain historical proof")
        self.assertNotIn(key(reused), tracker.related)
        # The same latch must apply when the complete row arrives through the
        # one explicit final reconciliation rather than a normal census.
        omitted = OwnershipTracker(100, self.subject)
        omitted.observe(typed(missing=[gap]))
        inspector = probe()
        inspector.inventory.identity.side_effect = lambda pid: detached if pid == 101 else None
        result = omitted.classify({}, typed(), inspector)
        self.assertEqual([entry["identity"] for entry in result["unverified_subject_candidates"]], [detached])
        self.assertEqual(sum(call.args == (101,) for call in inspector.inventory.identity.call_args_list), 1)
        no_birth = OwnershipTracker(100, self.subject)
        no_birth.observe(typed(missing=[unknown(101, partial=[{"pid": 101, "ppid": 100}])]))
        later = row(101, 1, 99, sid=101, pgid=101)
        result = no_birth.classify({}, typed(later), probe())
        self.assertEqual([entry["identity"] for entry in result["unverified_subject_candidates"]], [later],
                         "missing prior birth cannot prove PID reuse")

    def test_known_row_failed_revalidation_partial_relation_still_blocks(self):
        detached = row(101, 1, 11, sid=101, pgid=101)
        inspector = probe()
        partial = unknown(101, partial=[{"pid": 101, "ppid": 100}], stage="sid_2")
        inspector.inspect.return_value = {"matches": [], "errors": [partial.receipt()]}
        tracker = OwnershipTracker(100, self.subject)
        result = tracker.classify({}, typed(detached), inspector)
        self.assertEqual(result["observed_owned_survivors"], [])
        self.assertEqual([entry["identity"] for entry in result["unverified_subject_candidates"]], [detached])
        self.assertEqual(result["host_warnings"], [])
        self.assertEqual(tracker.resolved[key(self.subject)]["resolution"], "explicit_absence")
        self.assertEqual(tracker.active_history(100), [])
        self.assertNotIn(key(detached), tracker.owned)

    def test_required_unknown_cannot_be_cleared_by_an_unrequested_esrch_tail(self):
        child = row(101, 100, 11, sid=100, pgid=100)
        tracker = OwnershipTracker(100, self.subject)
        tracker.observe(typed(self.subject, child))
        inspector = probe()
        inspector.inspect.return_value = {"gone": True, "matches": [], "errors": []}
        result = tracker.classify({}, typed(missing=[unknown(101)]), inspector)
        self.assertEqual(len(result["unverified_subject_candidates"]), 1)
        inspector.inspect.assert_not_called()
        self.assertIn(key(child), tracker.owned)

    def test_complete_parent_reuse_resolution_survives_later_snapshot_omission(self):
        parent = row(101, 100, 11, sid=100, pgid=100)
        tracker = OwnershipTracker(100, self.subject)
        tracker.observe(typed(self.subject, parent))
        replacement = row(101, 1, 21, sid=101, pgid=101)
        child = row(102, 101, 22, sid=102, pgid=102)
        tracker.observe(typed(replacement, child))
        self.assertIn(key(parent), tracker.resolved)
        result = tracker.classify({}, typed(child), probe())
        self.assertEqual(result["observed_owned_survivors"], [])
        self.assertEqual(result["unverified_subject_candidates"], [])
        self.assertIn(child, [entry["identity"] for entry in result["host_warnings"]])
        self.assertIn(key(parent), tracker.owned, "historical proof must remain retained")
        self.assertTrue(result["historical_resolutions"])

    def test_new_positive_gap_after_reuse_is_not_discharged_by_stale_history(self):
        for final_only in (False, True):
            for with_birth in (False, True):
                with self.subTest(final_only=final_only, with_birth=with_birth):
                    tracker = OwnershipTracker(100, self.subject)
                    old = row(101, 100, 11, sid=100, pgid=100)
                    tracker.observe(typed(self.subject, old))
                    replacement = row(101, 1, 21, sid=101, pgid=101)
                    tracker.observe(typed(replacement))
                    self.assertIn(key(old), tracker.resolved)
                    partial = {"pid": 101, "ppid": 100}
                    if with_birth:
                        partial.update(start_seconds=21, start_microseconds=7)
                    new_gap = unknown(101, partial=[partial], stage="sid_2")
                    tracker.observe(typed(missing=[new_gap]))
                    inspector = probe()
                    inspector.inventory.identity.side_effect = lambda pid: replacement if pid == 101 else self.subject if pid == 100 else None
                    result = tracker.classify({}, typed() if final_only else typed(replacement), inspector)
                    self.assertEqual([entry["identity"] for entry in result["unverified_subject_candidates"]], [replacement])
                    self.assertEqual(result["host_warnings"], [])
                    self.assertEqual(tracker.related[key(replacement)]["previous_unknown"], new_gap.receipt())
                    self.assertIn(key(old), tracker.owned, "old proof retained, not used to erase new evidence")
                    # Repeat at a third birth: the previous candidate also becomes
                    # historical, but another positive gap belongs to this birth.
                    third = row(101, 1, 31, sid=101, pgid=101)
                    tracker.observe(typed(third))
                    third_gap = unknown(101, partial=[{"pid": 101, "ppid": 100}])
                    tracker.observe(typed(missing=[third_gap]))
                    inspector.inventory.identity.side_effect = lambda pid: third if pid == 101 else self.subject if pid == 100 else None
                    final = tracker.classify({}, typed() if final_only else typed(third), inspector)
                    self.assertEqual([entry["identity"] for entry in final["unverified_subject_candidates"]], [third])
                    self.assertIn(key(replacement), tracker.resolved)

    def test_reference_parent_reuse_does_not_revive_old_topology_evidence(self):
        for known_row in (False, True):
            with self.subTest(known_row=known_row):
                tracker = OwnershipTracker(100, self.subject)
                replacement = row(100, 1, 20, sid=100, pgid=100)
                tracker.observe(typed(replacement))
                detached = row(101, 1, 21, sid=101, pgid=101)
                inspector = probe()
                inspector.inspect.return_value = {"matches": [], "errors": [unknown(101, partial=[{"ppid": 100}], stage="sid_2").receipt()]}
                result = tracker.classify({}, typed(detached) if known_row else typed(missing=[unknown(101)]), inspector)
                self.assertEqual(result["observed_owned_survivors"], [])
                self.assertEqual(result["unverified_subject_candidates"], [])
                self.assertEqual(len(result["host_warnings"]), 1)
                self.assertEqual(tracker.resolved[key(self.subject)]["resolution"], "different_complete_birth")
                self.assertEqual(tracker.topology_history(100), [])
                self.assertIn(key(self.subject), tracker.owned, "historical proof remains retained")

    def test_global_native_and_implementation_failures_are_not_unknown_rows(self):
        for native in (NativeFixture([10], {10: [{"size": 7}]}),
                       NativeFixture([10], {10: [{"pid": 11}]}),
                       NativeFixture([10], {10: [ValueError("observer bug")]}),
                       NativeFixture([-10]), NativeFixture([10, 10])):
            with self.subTest(native=native), mock.patch("hermetic_processes.os.getsid", return_value=1):
                with self.assertRaises((RuntimeError, ValueError)):
                    inventory_with(native).snapshot()
        for size in (-1, 3, 10000000):
            native = NativeFixture([10])
            native.proc_listpids = mock.Mock(side_effect=[4, size])
            with self.subTest(size=size), self.assertRaises(RuntimeError):
                inventory_with(native).snapshot()
        with mock.patch("hermetic_processes.ctypes.sizeof", return_value=7), self.assertRaisesRegex(RuntimeError, "ABI"):
            ProcessInventory()

    def test_unknown_bounds_fail_visibly_without_silent_evidence_drop(self):
        native = NativeFixture([10, 11], {10: [{"size": 0, "errno": errno.EPERM}],
                                        11: [{"size": 0, "errno": errno.EPERM}]})
        with mock.patch("hermetic_processes.MAX_UNKNOWN_ROWS", 1), self.assertRaisesRegex(RuntimeError, "bound"):
            inventory_with(native).snapshot()
        tracker = OwnershipTracker(100, self.subject)
        tracker.MAX_GAPS = 1
        with self.assertRaisesRegex(RuntimeError, "bound"):
            tracker.observe(typed(missing=[unknown(10), unknown(11)]))
        tracker = OwnershipTracker(100, self.subject)
        tracker.MAX_AUDIT = 0
        with self.assertRaisesRegex(RuntimeError, "bound"):
            tracker.observe(typed(missing=[unknown(10)]))

    def test_incident_shape_keeps_owned_orphan_across_unrelated_eperm(self):
        child = row(101, 100, 11, sid=100, pgid=100)
        tracker = OwnershipTracker(100, self.subject)
        for _ in range(4):
            tracker.observe(typed(self.subject, child))
        tracker.observe(typed(self.subject, child, missing=[unknown(999)]))
        orphan = {**child, "ppid": 1}
        inspector = probe()
        result = tracker.classify({}, typed(orphan, missing=[unknown(999)]), inspector)
        self.assertEqual([entry["identity"] for entry in result["observed_owned_survivors"]], [orphan])
        self.assertEqual([entry["unknown"]["pid"] for entry in result["host_warnings"]], [999])
        self.assertEqual(tracker.owned[key(child)]["parent"], self.subject)
