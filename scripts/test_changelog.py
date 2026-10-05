"""Regression tests for release blocking and changelog preparation."""

import json
from pathlib import Path
import tempfile
import unittest

import changelog


class ChangelogTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "docs").mkdir()
        self.path = self.root / "docs/CHANGELOG.md"
        self.set_version("1.2.3")
        self.write_version_copies("1.2.3")
        self.path.write_text(
            "# Pyroshine changelog\n\n## [Unreleased]\n\n"
            "### Fixed\n\n- Next release fix.\n\n"
            "## [v1.2.3] - 2026-09-30\n\n### Added\n\n- Current feature.\n\n"
            "## [v1.2.2] - 2026-09-29\n\n### Fixed\n\n- Older fix.\n"
        )

    def set_version(self, version):
        (self.root / "Cargo.toml").write_text(f'[workspace.package]\nversion = "{version}"\n')

    def write_version_copies(self, version):
        """The files that repeat the workspace version, shaped like the real ones."""

        def lock(names):
            # An unrelated dependency with the same version must never change.
            entries = [("serde", version)] + [(name, version) for name in names]
            return 'version = 4\n\n' + "\n".join(
                f'[[package]]\nname = "{name}"\nversion = "{value}"\ndependencies = []\n' for name, value in entries
            )

        (self.root / "Cargo.lock").write_text(
            lock(["moonshine", "moonshine-core", "moonshine-management", "moonshine-tools"])
        )
        ui = self.root / "pyroshine-ui"
        (ui / "src-tauri").mkdir(parents=True, exist_ok=True)
        (ui / "src-tauri/Cargo.lock").write_text(lock(["moonshine-management", "moonshine-ui"]))
        (ui / "src-tauri/Cargo.toml").write_text(
            f'[package]\nname = "moonshine-ui"\nversion = "{version}"\nedition = "2024"\n\n'
            f'[dependencies]\nserde = "{version}"\n'
        )
        (ui / "package.json").write_text(
            json.dumps({"name": "pyroshine-ui", "version": version, "dependencies": {"x": version}}, indent=2) + "\n"
        )
        (ui / "package-lock.json").write_text(
            json.dumps(
                {
                    "name": "pyroshine-ui",
                    "version": version,
                    "lockfileVersion": 3,
                    "packages": {"": {"name": "pyroshine-ui", "version": version}, "node_modules/x": {"version": version}},
                },
                indent=2,
            )
            + "\n"
        )

    def test_notes_only_include_selected_release(self):
        self.assertEqual(
            changelog.release_notes(self.root, "v1.2.3"),
            "### Added\n\n- Current feature.\n",
        )

    def test_tag_must_match_workspace_version(self):
        with self.assertRaisesRegex(ValueError, "does not match"):
            changelog.release_notes(self.root, "v1.2.4")

    def test_version_bump_without_entry_blocks_release(self):
        self.set_version("1.2.4")
        changelog.sync_versions(self.root)
        with self.assertRaisesRegex(ValueError, "latest dated"):
            changelog.release_notes(self.root)

    def test_empty_or_placeholder_notes_block_release(self):
        for notes in ["", "- TODO", "- TBD"]:
            with self.subTest(notes=notes):
                text = "## [Unreleased]\n\n## [v1.2.3] - 2026-09-30\n\n" + notes
                self.path.write_text(text)
                with self.assertRaisesRegex(ValueError, "non-placeholder"):
                    changelog.release_notes(self.root)

    def test_malformed_or_duplicate_entries_block_release(self):
        original = self.path.read_text()
        for text in [
            original.replace("2026-09-30", "2026-02-30"),
            original.replace("2026-09-30", "30-09-2026"),
            original.replace("v1.2.2", "v1.2.3"),
        ]:
            with self.subTest(text=text):
                self.path.write_text(text)
                with self.assertRaises(ValueError):
                    changelog.release_notes(self.root)

    def test_stale_version_copies_block_release(self):
        self.set_version("1.2.4")
        with self.assertRaises(ValueError) as raised:
            changelog.release_notes(self.root)
        message = str(raised.exception)
        for copy in [
            "Cargo.lock (moonshine-management)",
            "pyroshine-ui/src-tauri/Cargo.lock (moonshine-ui)",
            "pyroshine-ui/src-tauri/Cargo.toml",
            "pyroshine-ui/package.json",
            'pyroshine-ui/package-lock.json (packages."".version)',
        ]:
            self.assertIn(copy, message)
        self.assertIn("sync-versions", message)

    def test_sync_changes_only_the_version_copies(self):
        before = {
            path: (self.root / path).read_text()
            for path in [
                "Cargo.lock",
                "pyroshine-ui/src-tauri/Cargo.lock",
                "pyroshine-ui/src-tauri/Cargo.toml",
                "pyroshine-ui/package.json",
                "pyroshine-ui/package-lock.json",
            ]
        }
        self.assertEqual(changelog.sync_versions(self.root), [])
        self.set_version("2.0.0-rc.1")
        self.assertEqual(sorted(changelog.sync_versions(self.root)), sorted(before))
        self.assertEqual(changelog.version_mismatches(self.root), [])
        for path, text in before.items():
            after = (self.root / path).read_text()
            changed = [(old, new) for old, new in zip(text.splitlines(), after.splitlines()) if old != new]
            self.assertEqual(len(text.splitlines()), len(after.splitlines()), path)
            for old, new in changed:
                self.assertEqual(old.replace("1.2.3", "2.0.0-rc.1"), new, path)
            if path.endswith(".lock"):
                # A dependency that happens to share the old version is untouched.
                self.assertIn('name = "serde"\nversion = "1.2.3"', after, path)
        self.assertIn('serde = "1.2.3"', (self.root / "pyroshine-ui/src-tauri/Cargo.toml").read_text())
        lock = json.loads((self.root / "pyroshine-ui/package-lock.json").read_text())
        self.assertEqual(lock["packages"]["node_modules/x"]["version"], "1.2.3")
        self.assertEqual(json.loads((self.root / "pyroshine-ui/package.json").read_text())["dependencies"]["x"], "1.2.3")

    def test_prepare_preserves_history_and_moves_unreleased(self):
        self.set_version("1.2.4")
        old_history = self.path.read_text().split("## [v1.2.3]", 1)[1]
        self.assertEqual(changelog.prepare(self.root, "2026-10-01"), "v1.2.4")
        text = self.path.read_text()
        self.assertEqual(text.split("## [v1.2.3]", 1)[1], old_history)
        self.assertEqual(changelog.sections(text)["Unreleased"][2], "")
        self.assertIn("Next release fix.", changelog.release_notes(self.root, "v1.2.4"))
        self.assertIn("## [v1.2.4] - 2026-10-01", text)
        self.assertEqual(changelog.version_mismatches(self.root), [], "prepare synchronizes versions")

    def test_invalid_preparation_does_not_modify_file(self):
        original = self.path.read_text()
        with self.assertRaisesRegex(ValueError, "already has"):
            changelog.prepare(self.root, "2026-10-01")
        self.assertEqual(self.path.read_text(), original)
        self.set_version("1.2.4")
        self.path.write_text(original.replace("- Next release fix.", ""))
        before = self.path.read_text()
        with self.assertRaisesRegex(ValueError, "Unreleased"):
            changelog.prepare(self.root, "2026-10-01")
        self.assertEqual(self.path.read_text(), before)

    def test_new_tags_require_semver_but_preserve_legacy_history(self):
        self.path.write_text(self.path.read_text() + "\n## [v0.16.1.1] - 2026-09-28\n\n- Legacy notes.\n")
        changelog.release_notes(self.root)
        for tag in ["v1.2.3.4", "v01.2.3", "v1.2.3-01"]:
            with self.subTest(tag=tag), self.assertRaisesRegex(ValueError, "SemVer"):
                changelog.release_notes(self.root, tag)
        self.set_version("1.2.4-rc.1")
        changelog.prepare(self.root, "2026-10-01")
        changelog.release_notes(self.root, "v1.2.4-rc.1")


if __name__ == "__main__":
    unittest.main()
