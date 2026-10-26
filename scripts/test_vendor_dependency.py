"""Exercise patch replay and drift detection against a local upstream fixture."""
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent


class VendorDependencyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "fork"
        self.upstream = Path(self.temp.name) / "upstream"
        self.upstream.mkdir()
        self.root.mkdir()
        (self.root / "scripts").mkdir()
        for name in ("vendor-dependency.sh", "vendor-pixelforge.sh", "check-vendor-pins.py"):
            shutil.copy2(SCRIPTS / name, self.root / "scripts" / name)
        for name in ("benches", "src", "shader", "tests"):
            (self.upstream / name).mkdir()
            (self.upstream / name / "file").write_text("upstream\n")
        (self.upstream / "Cargo.toml").write_text('[package]\nname = "pixelforge"\nversion = "0.9.1"\n')
        for name in ("LICENSE", "README.md", "CHANGELOG.md"):
            (self.upstream / name).write_text("upstream\n")
        self.git("init", "-q")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.invalid")
        self.git("add", ".")
        self.git("commit", "-qm", "base")
        self.git("tag", "v0.9.1")
        self.commit = self.git("rev-parse", "HEAD").strip()
        self.vendor = self.root / "vendor/pixelforge"
        self.vendor.mkdir(parents=True)
        (self.vendor / "LOCAL_CHANGES.md").write_text("maintained notes\n")
        self.patches = self.root / "vendor/pixelforge-patches"
        self.patches.mkdir()
        (self.patches / "UPSTREAM").write_text(f"commit {self.commit}\ndescribe v0.9.1\n")
        (self.root / "moonshine-core").mkdir()
        (self.root / "moonshine-core/Cargo.toml").write_text(
            '[dependencies]\npixelforge = { git = "https://github.com/hgaiser/pixelforge", tag = "v0.9.1" }\n')
        (self.root / "Cargo.toml").write_text(
            '[patch."https://github.com/hgaiser/pixelforge"]\npixelforge = { path = "vendor/pixelforge" }\n')
        (self.root / "Cargo.lock").write_text('[[package]]\nname = "pixelforge"\nversion = "0.9.1"\n')

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.upstream), *args], text=True)

    def run_vendor(self, *args, success=True):
        result = subprocess.run(
            ["bash", str(self.root / "scripts/vendor-pixelforge.sh"), *args,
             "--source", str(self.upstream)], capture_output=True, text=True)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def add_patch(self):
        (self.upstream / "src/file").write_text("patched\n")
        patch = self.git("diff")
        (self.patches / "0001-change.patch").write_text(patch)
        self.git("checkout", "--", "src/file")

    def test_replay_is_repeatable_and_notes_survive(self):
        self.add_patch()
        self.run_vendor()
        self.run_vendor("--check")
        self.assertEqual((self.vendor / "src/file").read_text(), "patched\n")
        self.assertEqual((self.vendor / "LOCAL_CHANGES.md").read_text(), "maintained notes\n")
        self.run_vendor()
        self.run_vendor("--check")

    def test_empty_patch_series(self):
        self.run_vendor()
        self.assertIn("0 patch(es)", self.run_vendor("--check").stdout)

    def test_detects_source_and_nested_notes_drift(self):
        self.run_vendor()
        (self.vendor / "src/LOCAL_CHANGES.md").write_text("untracked source\n")
        self.run_vendor("--check", success=False)
        (self.vendor / "src/LOCAL_CHANGES.md").unlink()
        (self.vendor / "src/file").write_text("drift\n")
        self.run_vendor("--check", success=False)

    def test_conflict_does_not_replace_vendor_or_pin(self):
        self.add_patch()
        self.run_vendor()
        pin = (self.patches / "UPSTREAM").read_bytes()
        (self.patches / "0001-change.patch").write_text("invalid patch\n")
        self.run_vendor(success=False)
        self.assertEqual((self.vendor / "src/file").read_text(), "patched\n")
        self.assertEqual((self.patches / "UPSTREAM").read_bytes(), pin)

    def test_rejects_cargo_pin_mismatch(self):
        self.run_vendor()
        manifest = self.root / "moonshine-core/Cargo.toml"
        manifest.write_text(manifest.read_text().replace("v0.9.1", "v0.10.0"))
        self.assertIn("must agree", self.run_vendor("--check", success=False).stderr)

    def test_detects_executable_mode_drift(self):
        self.run_vendor()
        (self.vendor / "shader/file").chmod(0o755)
        self.run_vendor("--check", success=False)

    def test_inputtino_public_and_native_pins_must_agree(self):
        repository = "https://github.com/games-on-whales/inputtino"
        native = self.root / "vendor/inputtino/bindings/rust/inputtino-sys"
        native.mkdir(parents=True)
        (native / "Cargo.toml").write_text('[package]\nname = "inputtino-sys"\nversion = "0.1.0"\n')
        patches = self.root / "vendor/inputtino-patches"
        patches.mkdir()
        (patches / "UPSTREAM").write_text(f"commit {self.commit}\ndescribe base\n")
        (self.root / "Cargo.toml").write_text(
            f'[patch."{repository}"]\ninputtino-sys = {{ path = "vendor/inputtino/bindings/rust/inputtino-sys" }}\n')
        core = self.root / "moonshine-core/Cargo.toml"
        core.write_text(f'[dependencies]\ninputtino = {{ git = "{repository}", rev = "{self.commit}" }}\n')
        (self.root / "Cargo.lock").write_text(
            f'[[package]]\nname = "inputtino"\nversion = "0.1.0"\n'
            f'source = "git+{repository}?rev={self.commit}#{self.commit}"\n'
            '[[package]]\nname = "inputtino-sys"\nversion = "0.1.0"\n')
        command = ["python3", str(self.root / "scripts/check-vendor-pins.py"), "inputtino"]
        subprocess.run(command, check=True, capture_output=True)
        core.write_text(core.read_text().replace(self.commit, "f" * 40))
        result = subprocess.run(command, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("same revision", result.stderr)

    def test_applies_numbered_patches_in_order(self):
        self.add_patch()
        (self.upstream / "src/file").write_text("patched\n")
        self.git("add", ".")
        self.git("commit", "-qm", "first patch")
        (self.upstream / "src/file").write_text("second patch\n")
        (self.patches / "0002-change.patch").write_text(self.git("diff"))
        self.run_vendor()
        self.run_vendor("--check")
        self.assertEqual((self.vendor / "src/file").read_text(), "second patch\n")

    def test_rejects_unknown_options(self):
        self.run_vendor("--chek", success=False)


if __name__ == "__main__":
    unittest.main()
