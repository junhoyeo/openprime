"""Pure identity-policy vectors plus a real owned native-reference probe."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from hermetic_ownership import OwnershipTracker, ReferenceProbe
from hermetic_processes import ProcessInventory, key, reap_owned


def row(pid, parent, birth, *, sid=1, pgid=1):
    return {"pid": pid, "ppid": parent, "start_seconds": birth, "start_microseconds": 7,
            "uid": os.getuid(), "sid": sid, "pgid": pgid, "name": "synthetic"}


def snapshot(*rows):
    return {key(item): item for item in rows}


class OwnershipTest(unittest.TestCase):
    def setUp(self):
        self.subject = row(100, 50, 10, sid=100, pgid=100)
        self.empty_probe = mock.Mock()
        self.empty_probe.inspect.return_value = {"matches": [], "errors": []}
        self.empty_probe.inventory.identity.return_value = None

    def test_observed_owned_orphan_stays_blocking_after_reparenting(self):
        child = row(101, 100, 11, sid=100, pgid=100)
        tracker = OwnershipTracker(100, self.subject)
        tracker.observe(snapshot(self.subject, child))
        orphan = {**child, "ppid": 1, "sid": 101, "pgid": 101}
        result = tracker.classify({}, snapshot(orphan), self.empty_probe)
        self.assertEqual([entry["identity"] for entry in result["observed_owned_survivors"]], [orphan])
        self.assertEqual(result["host_warnings"], [])
        self.empty_probe.inspect.assert_not_called()

    def test_unverified_subject_group_candidate_blocks_without_becoming_owned(self):
        candidate = row(102, 1, 12, sid=100, pgid=102)
        tracker = OwnershipTracker(100, self.subject)
        result = tracker.classify({}, snapshot(candidate), self.empty_probe)
        self.assertEqual(result["observed_owned_survivors"], [])
        self.assertEqual([entry["identity"] for entry in result["unverified_subject_candidates"]], [candidate])
        self.assertEqual(result["host_warnings"], [])

    def test_launchd_arrivals_warn_but_positive_private_reference_still_blocks(self):
        broker = row(103, 1, 13)
        artifact_holder = row(104, 1, 14)
        probe = mock.Mock()
        probe.inventory.identity.return_value = None
        probe.inspect.side_effect = [
            {"matches": [], "errors": ["cwd metadata unavailable"]},
            {"matches": [{"kind": "open_artifact", "path": "/private/tmp/owned/receipt"}], "errors": []},
        ]
        tracker = OwnershipTracker(100, self.subject)
        result = tracker.classify({}, snapshot(broker, artifact_holder), probe)
        self.assertEqual([entry["identity"] for entry in result["host_warnings"]], [broker])
        self.assertEqual([entry["identity"] for entry in result["unverified_subject_candidates"]], [artifact_holder])
        self.assertEqual(result["observed_owned_survivors"], [])

    def test_recycled_parent_pid_does_not_create_owned_identity_proof(self):
        tracker = OwnershipTracker(100, self.subject)
        recycled_parent = row(100, 1, 20, sid=100, pgid=100)
        child = row(105, 100, 21, sid=105, pgid=105)
        tracker.observe(snapshot(recycled_parent, child))
        result = tracker.classify({}, snapshot(child), self.empty_probe)
        self.assertEqual(result["observed_owned_survivors"], [])
        self.assertEqual([entry["identity"] for entry in result["host_warnings"]], [child])

    def test_unstable_parent_edge_is_candidate_not_owned_proof(self):
        child = row(106, 100, 11, sid=106, pgid=106)
        child.update(topology_stable=False, previous_topology={"ppid": 1, "sid": 106, "pgid": 106, "uid": os.getuid()})
        tracker = OwnershipTracker(100, self.subject)
        tracker.observe(snapshot(self.subject, child))
        result = tracker.classify({}, snapshot(child), self.empty_probe)
        self.assertEqual(result["observed_owned_survivors"], [])
        self.assertEqual([entry["identity"] for entry in result["unverified_subject_candidates"]], [child])

    def test_native_owned_cwd_and_log_inode_are_positive_candidate_evidence(self):
        root = Path(tempfile.mkdtemp(prefix="refs-", dir=os.environ["TMPDIR"])).resolve()
        inventory = ProcessInventory()
        with (root / "owned.log").open("w+b") as log:
            child = subprocess.Popen([sys.executable, "-c", "import sys; sys.stdin.buffer.read(1)"],
                                     cwd=root, stdin=subprocess.PIPE, stdout=log, stderr=log)
            try:
                identity = inventory.identity(child.pid)
                self.assertIsNotNone(identity)
                stat = os.fstat(log.fileno())
                evidence = ReferenceProbe(inventory, [root], (stat.st_dev, stat.st_ino)).inspect(identity)
                self.assertFalse(evidence.get("gone", False))
                self.assertIn({"kind": "cwd", "path": str(root)}, evidence["matches"])
                self.assertTrue(any(entry["kind"] == "subject_log_inode" for entry in evidence["matches"]), evidence)
                self.assertIsNone(child.poll(), "metadata inspection never signals the candidate")
                from hermetic_processes import UnknownObservation
                denied = UnknownObservation(child.pid, "sid_2", None, 1, "injected final SID denial", [identity])
                failed_revalidation = mock.Mock()
                failed_revalidation.lib = inventory.lib
                failed_revalidation.identity.side_effect = [identity, denied]
                retained = ReferenceProbe(failed_revalidation, [root], (stat.st_dev, stat.st_ino)).inspect(identity)
                self.assertIn({"kind": "cwd", "path": str(root)}, retained["matches"])
                self.assertIn(denied.receipt(), retained["errors"])
                self.assertFalse(retained.get("gone", False))
            finally:
                child.stdin.close()
                reap_owned(child)
