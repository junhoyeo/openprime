"""Cutover preflight gates. Only synthetic bytes/processes; no product execution."""
import importlib.util
import io
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location("cutover_preflight", Path(__file__).with_name("cutover_preflight.py"))
cutover = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cutover)


class CutoverPreflightTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="pa-cut-test-", dir=os.environ.get("TMPDIR", "/tmp"))).resolve()
        self.stage = self.root / "stage"
        self.stage.mkdir()
        for name in (*cutover.REQUIRED, "skills/example/SKILL.md"):
            path = self.stage / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("synthetic fixture\n")
        (self.stage / "package.json").write_text(json.dumps({"version": "0.0.0-test"}))
        for name in ("models.bundled.json", "mcp-services.bundled.json"):
            (self.stage / name).write_text("{}")
        self.archive = self.root / "fixture.tar.gz"
        self.pack()
        self.manifest = {
            "repository": cutover.FORK,
            "artifact_url": cutover.FORK + "/releases/download/fixture/fixture.tar.gz",
            "commit": "1" * 40, "version": "0.0.0-test",
            "legacy_commit": cutover.LEGACY_COMMIT,
            "archive_sha256": cutover.digest(self.archive),
            "process_inventory": [{"role": "old-daemon", "pid": 123, "start_id": "unknown"}],
        }

    def pack(self):
        with tarfile.open(self.archive, "w:gz") as archive:
            for path in sorted(self.stage.rglob("*")):
                archive.add(path, arcname=str(path.relative_to(self.stage)), recursive=False)

    def check(self):
        return cutover.validate_manifest(self.manifest, self.archive, self.stage)

    def test_default_preflight_verifies_bytes_but_never_claims_activation_or_ownership(self):
        before = {str(path): path.read_bytes() for path in self.root.rglob("*") if path.is_file()}
        result = self.check()
        after = {str(path): path.read_bytes() for path in self.root.rglob("*") if path.is_file()}
        self.assertEqual(before, after)
        self.assertEqual(result["mode"], "dry-run")
        self.assertEqual(result["activation"], "pending")
        self.assertFalse(result["build_commit_verified"])
        self.assertFalse(result["served_url_verified"])
        self.assertEqual(result["process_inventory"], [{
            "role": "old-daemon", "pid": 123, "start_id": "unknown",
            "ownership": "unverified; retain; no signal authorized",
        }])
        self.assertEqual(result["skill_manifests"], ["skills/example/SKILL.md"])
        self.assertIn("unverified", result["runtime_abi"])

    def test_upstream_and_spoofed_artifact_urls_are_refused(self):
        for url in (
            "https://github.com/PrimeIntellect-ai/prime-agent/releases/download/x/a",
            "https://github.com.evil.test/junhoyeo/openprime/releases/download/x/a",
            "https://github.com/junhoyeo/openprime/releases/download/x/a?redirect=bad",
            "http://github.com/junhoyeo/openprime/releases/download/x/a",
            cutover.FORK + "/releases/download/../../../../PrimeIntellect-ai/prime-agent/releases/download/x/a",
            cutover.FORK + "/releases/download/%2e%2e/%2e%2e/upstream",
            cutover.FORK + "/releases/download/tag/%252e%252e",
        ):
            with self.subTest(url=url):
                self.manifest["artifact_url"] = url
                with self.assertRaisesRegex(ValueError, "fork GitHub"):
                    self.check()

    def test_archive_or_staging_tampering_is_refused(self):
        self.manifest["archive_sha256"] = "0" * 64
        with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
            self.check()
        self.manifest["archive_sha256"] = cutover.digest(self.archive)
        (self.stage / "prime-agent").write_text("replaced")
        with self.assertRaisesRegex(ValueError, "staged bytes differ"):
            self.check()

    def test_unsafe_archive_paths_links_and_duplicates_are_refused(self):
        for kind in ("traversal", "symlink", "duplicate"):
            with self.subTest(kind=kind):
                with tarfile.open(self.archive, "w:gz") as archive:
                    member = tarfile.TarInfo("../escape" if kind == "traversal" else "prime-agent")
                    if kind == "symlink":
                        member.type = tarfile.SYMTYPE
                        member.linkname = "/tmp/not-owned"
                        archive.addfile(member)
                    else:
                        payload = (self.stage / "prime-agent").read_bytes()
                        member.size = len(payload)
                        archive.addfile(member, io.BytesIO(payload))
                        if kind == "duplicate":
                            archive.addfile(member, io.BytesIO(payload))
                self.manifest["archive_sha256"] = cutover.digest(self.archive)
                with self.assertRaisesRegex(ValueError, "unsafe|unsupported"):
                    self.check()
        self.assertFalse((self.root / "escape").exists())

    def test_missing_layout_and_extra_staged_files_are_refused(self):
        (self.stage / "prime-agent-runtime/src/rlm/repl.py").unlink()
        self.pack()
        self.manifest["archive_sha256"] = cutover.digest(self.archive)
        with self.assertRaisesRegex(ValueError, "missing artifact layout"):
            self.check()
        (self.stage / "unreviewed.py").write_text("extra")
        with self.assertRaisesRegex(ValueError, "absent from archive"):
            self.check()

    def test_rollback_pin_is_mandatory(self):
        self.manifest["legacy_commit"] = "2" * 40
        with self.assertRaisesRegex(ValueError, "legacy develop"):
            self.check()

    def test_synthetic_rehearsal_starts_stops_and_reaps_old_new_rollback(self):
        result = cutover.simulate()
        self.assertEqual(result["activation"], "pending")
        self.assertFalse(result["live_processes_touched"])
        self.assertEqual([step["role"] for step in result["steps"] if step["action"] == "ready"],
                         ["old-ts-fixture", "new-rust-fixture", "rollback-ts-fixture"])
        self.assertEqual([step["exit_code"] for step in result["steps"] if step["action"] == "reaped"],
                         [0, 0, 0])
        self.assertEqual(result["final_selection"]["sha256"], result["old_artifact_sha256"])
        self.assertEqual(result["steps"][0]["artifact_sha256"], result["steps"][4]["artifact_sha256"])
        self.assertNotEqual(result["steps"][0]["artifact_sha256"], result["steps"][2]["artifact_sha256"])
        self.assertEqual(json.loads((Path(result["root"]) / "receipt.json").read_text()), result)

    def test_simulation_refuses_external_inputs(self):
        with self.assertRaises(SystemExit) as raised:
            cutover.main(["--simulate", "--manifest", str(self.root / "not-read.json")])
        self.assertEqual(raised.exception.code, 2)

    def test_readiness_failure_reaps_only_the_created_child(self):
        children = []
        launch = cutover.subprocess.Popen

        def capture(*args, **kwargs):
            child = launch(*args, **kwargs)
            children.append(child)
            return child

        with mock.patch.object(cutover.subprocess, "Popen", side_effect=capture), \
             mock.patch.object(cutover.selectors, "DefaultSelector", side_effect=OSError("injected readiness failure")):
            with self.assertRaisesRegex(OSError, "injected readiness failure"):
                cutover.simulate()
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].returncode, "owned child must be reaped on readiness failure")


if __name__ == "__main__":
    unittest.main()
