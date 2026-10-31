"""Trash guard unit vectors: profile shape and the owned fake (no real trash, no OS sandbox)."""
import errno
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from hermetic_control import ControlDirectory
import hermetic_fake_trash as fake
import hermetic_trash as trash


class TrashProfileTest(unittest.TestCase):
    def test_rule_order_puts_the_control_allow_last_and_denies_services(self):
        rules = trash.profile_rules("/private/tmp/pa-control-x", "patrashN",
                                    {"/usr/bin/trash": "/usr/bin/trash"}, json.dumps)
        self.assertEqual(rules[-1], '(allow process-exec* (literal "/private/tmp/pa-control-x/bin/trash"))')
        self.assertIn("(deny appleevent-send)", rules)
        for name, _ in trash.DENIED_MACH_SERVICES:
            self.assertIn(f'(deny mach-lookup (global-name "{name}"))', rules)
        self.assertNotIn("lsd", "".join(rules))
        self.assertIn('(deny process-exec* (with message "patrashN") (regex #"/[Tt][Rr][Aa][Ss][Hh]$"))', rules)
        self.assertIn('(deny process-exec* (with message "patrashN") (literal "/usr/bin/trash"))', rules)
        allow = rules.index(rules[-1])
        self.assertTrue(all("allow" not in rule for rule in rules[:allow]))
        self.assertIn('(deny mach-lookup (global-name-regex #"^com\\.apple\\.[Ff]ile[Pp]rovider\\."))', rules)
        self.assertNotIn("desktopservicesd", "".join(rules))
        self.assertTrue(all(spelling in "".join(rules) for spelling in trash.owner_trash()))

    def test_regex_probes_are_concrete_names_matched_by_the_denied_regex(self):
        import re
        self.assertTrue(trash.MACH_REGEX_PROBES)
        for probe in trash.MACH_REGEX_PROBES:
            self.assertTrue(any(re.search(regex, probe) for regex, _ in trash.DENIED_MACH_REGEXES), probe)

    def test_owner_trash_ignores_the_synthetic_home(self):
        with mock.patch.dict(os.environ, {"HOME": "/private/tmp/synthetic"}):
            self.assertTrue(all(not item.startswith("/private/tmp/synthetic") for item in trash.owner_trash()))

    def test_discovery_records_path_hits_and_realpaths(self):
        with tempfile.TemporaryDirectory() as directory:
            hit = Path(directory) / "trash-put"
            hit.write_text("")
            found = trash.discover_real_trash(f"{directory}{os.pathsep}/nonexistent")
            self.assertEqual(found[str(hit)], os.path.realpath(hit))


class FakeTrashTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="fake-trash-", dir=os.environ.get("TMPDIR"))).resolve()
        self.target = self.root / "target"
        self.target.mkdir()
        self.env = trash.fake_attestation(self.root, self.target)

    def item(self, name="session.jsonl", text="x"):
        path = self.root / name
        path.write_text(text)
        return path

    def stored(self):
        return sorted(path for path in (self.root / "fake-trash").rglob("*") if path.is_file()
                      and path.name != "receipts.jsonl")

    def test_old_base_double_dash_and_v13_bare_forms_move_and_never_delete(self):
        for form in (["--"], []):
            with self.subTest(form=form):
                path = self.item(f"s{len(form)}.jsonl", "keep")
                self.assertEqual(fake.main([*form, str(path)], self.env), 0)
                self.assertFalse(os.path.lexists(path))
                self.assertIn("keep", [stored.read_text() for stored in self.stored()])

    def test_every_flag_relative_and_non_normalized_path_is_refused(self):
        path = self.item()
        for argv in (["-e"], ["-F", str(path)], ["-v", str(path)], ["--help"], ["session.jsonl"],
                     [str(self.root) + "/./session.jsonl"], [str(self.root) + "/../x"],
                     [str(self.root) + "//session.jsonl"], []):
            with self.subTest(argv=argv):
                self.assertEqual(fake.main(argv, self.env), 2)
                self.assertTrue(path.exists())

    def test_outside_roots_ancestors_and_the_store_are_refused(self):
        outside = Path(tempfile.mkdtemp(prefix="outside-")).resolve() / "x"
        outside.write_text("outside")
        (self.root / "fake-trash" / "old").mkdir(parents=True)
        inside_store = self.root / "fake-trash" / "old" / "kept"
        inside_store.write_text("kept")
        for path in (outside, self.root, self.target, self.root.parent, inside_store,
                     self.root / "fake-trash"):
            with self.subTest(path=path):
                self.assertEqual(fake.main([str(path)], self.env), 2)
                self.assertTrue(os.path.lexists(path))
        self.assertEqual(outside.read_text(), "outside")

    def test_missing_attestation_and_store_outside_roots_are_refused(self):
        path = self.item()
        self.assertEqual(fake.main([str(path)], {}), 2)
        self.assertEqual(fake.main([str(path)], {**self.env, "store": "/private/tmp/elsewhere"}), 2)
        self.assertEqual(fake.main([str(path)], {**self.env, "roots": ["relative/root"]}), 2)
        self.assertTrue(path.exists())

    def test_installed_fake_ignores_subject_environment_overrides(self):
        path = self.item()
        with mock.patch.dict(os.environ, {"PA_HERMETIC_FAKE_TRASH_ROOTS": "/", "PA_HERMETIC_FAKE_TRASH_STORE": "/"}):
            with mock.patch.object(fake, "ATTESTED", self.env):
                self.assertEqual(fake.main([str(path)]), 0)
        self.assertFalse(path.exists())
        self.assertTrue((self.root / "fake-trash").is_dir())

    def test_symlink_leaf_moves_as_itself_and_target_survives(self):
        target_file = self.item("real-target", "target")
        link = self.root / "link.jsonl"
        link.symlink_to(target_file)
        self.assertEqual(fake.main([str(link)], self.env), 0)
        self.assertFalse(os.path.lexists(link))
        self.assertEqual(target_file.read_text(), "target")

    def test_directory_operand_moves_with_its_contents(self):
        directory = self.root / "artifacts"
        directory.mkdir()
        (directory / "a").write_text("a")
        self.assertEqual(fake.main([str(directory)], self.env), 0)
        self.assertFalse(directory.exists())
        self.assertIn("a", [path.read_text() for path in self.stored()])

    def test_exclusive_rename_never_overwrites_and_reports_failure(self):
        path = self.item()

        def colliding(source, destination, flags):
            self.assertEqual(flags, fake.RENAME_EXCL)
            fake.ctypes.set_errno(errno.EEXIST)
            return -1

        self.assertEqual(fake.main([str(path)], self.env, renamex=colliding), 1)
        self.assertTrue(path.exists())

    def test_cross_device_rename_is_refused_not_copied(self):
        path = self.item()

        def cross_device(source, destination, flags):
            fake.ctypes.set_errno(errno.EXDEV)
            return -1

        self.assertEqual(fake.main([str(path)], self.env, renamex=cross_device), 1)
        self.assertEqual(path.read_text(), "x")
        self.assertEqual(self.stored(), [])

    def test_all_operands_are_validated_before_any_move(self):
        good = self.item("good")
        self.assertEqual(fake.main([str(good), str(self.root / "missing")], self.env), 2)
        self.assertTrue(good.exists())

    def test_partial_failure_reports_moved_items_and_keeps_them(self):
        first, second = self.item("first", "1"), self.item("second", "2")
        real = fake.rename_exclusive
        calls = []

        def second_fails(source, destination, renamex=None):
            calls.append(source)
            if len(calls) == 2:
                raise OSError(errno.EIO, "injected", str(source))
            real(source, destination)

        with mock.patch.object(fake, "rename_exclusive", side_effect=second_fails):
            self.assertEqual(fake.main([str(first), str(second)], self.env), 1)
        self.assertFalse(first.exists())
        self.assertTrue(second.exists())
        self.assertIn("1", [path.read_text() for path in self.stored()])

    def test_real_exclusive_rename_reports_eexist_and_keeps_both_files(self):
        source, destination = self.item("source", "new"), self.item("existing", "old")
        with self.assertRaises(OSError) as raised:
            fake.rename_exclusive(source, destination)
        self.assertEqual(raised.exception.errno, errno.EEXIST)
        self.assertEqual((source.read_text(), destination.read_text()), ("new", "old"))

    def test_reported_success_without_a_move_is_not_success(self):
        path = self.item()

        def claims_success(source, destination, flags):
            return 0

        self.assertEqual(fake.main([str(path)], self.env, renamex=claims_success), 1)
        self.assertTrue(path.exists())

    def test_receipt_failure_after_a_verified_move_keeps_exit_zero(self):
        path = self.item()
        real_open = Path.open

        def failing_open(self_path, *args, **kwargs):
            if self_path.name == "receipts.jsonl":
                raise OSError(errno.ENOSPC, "injected")
            return real_open(self_path, *args, **kwargs)

        with mock.patch.object(Path, "open", failing_open):
            self.assertEqual(fake.main([str(path)], self.env), 0)
        self.assertFalse(path.exists())

    def test_inventory_lists_names_types_and_sizes_never_contents(self):
        path = self.item("secret.jsonl", "secret contents")
        self.assertEqual(fake.main([str(path)], self.env), 0)
        inventory = trash.store_inventory(self.root)
        self.assertTrue(inventory["present"])
        self.assertNotIn("secret contents", json.dumps(inventory))
        self.assertIn("secret.jsonl", json.dumps(inventory))


class TrashInventoryTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="inventory-", dir=os.environ.get("TMPDIR"))).resolve()

    def test_symlinked_store_is_not_followed(self):
        outside = Path(tempfile.mkdtemp(prefix="outside-")).resolve()
        (outside / "private-name").write_text("x")
        (self.root / trash.FAKE_STORE).symlink_to(outside, target_is_directory=True)
        inventory = trash.store_inventory(self.root)
        self.assertEqual(inventory["present"], "not-a-directory")
        self.assertEqual(inventory["entries"], [])
        self.assertNotIn("private-name", json.dumps(inventory))

    def test_entry_symlink_to_a_directory_is_listed_not_walked(self):
        outside = Path(tempfile.mkdtemp(prefix="outside-")).resolve()
        (outside / "private-name").write_text("x")
        store = self.root / trash.FAKE_STORE
        store.mkdir()
        (store / "link").symlink_to(outside, target_is_directory=True)
        inventory = trash.store_inventory(self.root)
        self.assertEqual(inventory["entries"], [{"path": "link", "kind": "symlink",
                                                 "size": (store / "link").lstat().st_size}])
        self.assertNotIn("private-name", json.dumps(inventory))

    def test_bounds_truncate_with_a_marker(self):
        store = self.root / trash.FAKE_STORE
        deep = store
        for index in range(5):
            deep = deep / f"d{index}"
        deep.mkdir(parents=True)
        for index in range(10):
            (store / f"f{index}").write_text("")
        wide = trash.store_inventory(self.root, max_entries=3)
        self.assertTrue(wide["truncated"])
        self.assertEqual(len(wide["entries"]), 3)
        shallow = trash.store_inventory(self.root, max_depth=2)
        self.assertTrue(shallow["truncated"])
        self.assertFalse(any(entry["path"].count("/") >= 2 for entry in shallow["entries"]))

    def test_errors_during_the_walk_are_recorded_not_raised(self):
        store = self.root / trash.FAKE_STORE
        store.mkdir()
        (store / "vanishing").write_text("x")
        real_lstat = os.lstat

        def disappearing(path, *args, **kwargs):
            if path == "vanishing":
                raise FileNotFoundError(2, "vanished", path)
            return real_lstat(path, *args, **kwargs)

        with mock.patch.object(trash.os, "lstat", side_effect=disappearing):
            inventory = trash.store_inventory(self.root)
        self.assertEqual(inventory["entries"], [])
        self.assertTrue(any("vanishing" in item for item in inventory["errors"]))
        with mock.patch.object(trash.os, "listdir", side_effect=RuntimeError("injected")):
            failed = trash.store_inventory(self.root)
        self.assertTrue(any("inventory failed" in item for item in failed["errors"]))


class TrashInstallTest(unittest.TestCase):
    def setUp(self):
        self.base = Path(tempfile.mkdtemp(prefix="install-", dir=os.environ.get("TMPDIR"))).resolve()
        (self.base / "control").mkdir(mode=0o700)
        self.control = ControlDirectory(self.base / "control")
        self.addCleanup(self.control.close)

    def test_write_executable_is_exclusive_private_and_refuses_bad_names(self):
        path = self.control.write_executable("bin/tool", "#!/bin/sh\n")
        self.assertEqual(path.stat().st_mode & 0o777, 0o500)
        with self.assertRaises(FileExistsError):
            self.control.write_executable("bin/tool", "")
        for bad in ("../x", "a/b/c", ".", "bin/..", "bin/", "/abs"):
            with self.subTest(bad=bad), self.assertRaises((ValueError, OSError)):
                self.control.write_executable(bad, "")

    def test_install_fake_embeds_interpreter_and_attestation(self):
        root, target = self.base / "root", self.base / "target"
        root.mkdir()
        target.mkdir()
        installed = trash.install_fake(self.control, "/private/tmp/attested/python", root, target)
        text = installed.read_text()
        self.assertTrue(text.startswith("#!/private/tmp/attested/python\n"))
        self.assertIn(json.dumps(trash.fake_attestation(root, target)), text)
        self.assertNotIn("ATTESTED = None", text)
        with self.assertRaises(ValueError):
            trash.install_fake(self.control, "relative/python", root, target)


if __name__ == "__main__":
    unittest.main()
