#!/usr/bin/env python3
"""Check that every known-defect characterization still fails for its reason.

`scripts/known_defects.toml` lists tests that state a corrected contract for a
confirmed finding and are expected to fail until that finding is fixed. This
runs each one and requires a failure whose output contains the entry's
marker. It reports an error when an entry passes (remove its ignore marker
and manifest entry with the fix), fails for another reason, cannot be found,
or when an ignored "known defect" test is missing from the manifest.

GPU entries run only with `--gpu` (on a host with a Vulkan Video GPU and
ffmpeg); otherwise they are reported as not executed, never as expected
failures. Native entries build the vendored Inputtino tests with
AddressSanitizer/UBSan in `--native-build`.
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "scripts" / "known_defects.toml"
IGNORE = re.compile(r'#\[ignore = "known defect: review 2026-10-05 ([A-Z]+-\d+)[^"]*"\]\s*(?:#\[[^\]]*\]\s*)*(?:async\s+)?fn\s+(\w+)')
SANITIZE = "-fsanitize=address,undefined -fno-omit-frame-pointer -fno-sanitize-recover=undefined"


def declared_tests():
    """(finding, function name) of every ignored known-defect Rust test."""
    found = set()
    for path in (ROOT / "moonshine-core" / "src").rglob("*.rs"):
        for match in IGNORE.finditer(path.read_text()):
            found.add((match[1], match[2]))
    return found


def check_manifest(manifest):
    errors = []
    listed = {(entry["finding"], entry["test"].rsplit("::", 1)[-1]) for entry in manifest.get("rust", [])}
    declared = declared_tests()
    for finding, name in sorted(declared - listed):
        errors.append(f"{finding} {name}: ignored as a known defect but missing from {MANIFEST.name}")
    for finding, name in sorted(listed - declared):
        errors.append(f"{finding} {name}: listed in {MANIFEST.name} but not ignored as a known defect")
    return errors


def test_binary():
    """Build the moonshine-core library tests once and return the executable."""
    command = ["cargo", "test", "-p", "moonshine-core", "--all-features", "--lib", "--no-run", "--message-format=json"]
    result = subprocess.run(command, cwd=ROOT, stdout=subprocess.PIPE, text=True, check=True)
    for line in result.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message.get("executable") and message["target"]["kind"] == ["lib"]:
            return message["executable"]
    sys.exit("could not find the moonshine-core library test executable")


def classify(output, returncode, marker):
    if "running 0 tests" in output or "0 passed; 0 failed" in output:
        return "missing", "test not found or not ignored"
    if returncode == 0:
        return "passed", "passes: remove its ignore marker and manifest entry with the fix"
    if marker in output:
        return "expected-failure", "fails for the recorded reason"
    return "wrong-failure", "fails without the recorded marker"


def run_rust(entries, gpu):
    results = []
    if not entries:
        return results
    binary = test_binary()
    for entry in entries:
        if entry.get("gpu") and not gpu:
            results.append({**entry, "status": "not-run", "detail": "needs --gpu"})
            continue
        env = dict(os.environ, RUST_BACKTRACE="0")
        if entry.get("gpu"):
            env["MOONSHINE_TEST_GPU"] = "1"
        command = [binary, "--ignored", "--exact", entry["test"], "--nocapture", "--test-threads=1"]
        run = subprocess.run(command, cwd=ROOT / "moonshine-core", env=env, capture_output=True, text=True, timeout=600)
        output = run.stdout + run.stderr
        status, detail = classify(output, run.returncode, entry["marker"])
        results.append({**entry, "status": status, "detail": detail, "output": output[-4000:]})
    return results


def build_native(build):
    source = ROOT / "vendor" / "inputtino"
    # CMake's default (or CC/CXX) compiler; GCC and Clang both report the overread.
    configure = [
        "cmake", "-S", str(source), "-B", str(build),
        "-DCMAKE_BUILD_TYPE=Debug", "-DBUILD_TESTING=OFF",
        "-DINPUTTINO_PS5_FEATURE_TESTS=ON", "-DINPUTTINO_EDGE_TESTS=ON", "-DINPUTTINO_ELITE_TESTS=ON",
        f"-DCMAKE_CXX_FLAGS={SANITIZE}",
        "-DCMAKE_EXE_LINKER_FLAGS=-fsanitize=address,undefined",
        "-DCMAKE_SHARED_LINKER_FLAGS=-fsanitize=address,undefined",
    ]
    subprocess.run(configure, check=True, stdout=subprocess.DEVNULL)
    subprocess.run(["cmake", "--build", str(build), "--parallel"], check=True, stdout=subprocess.DEVNULL)


def run_native(entries, build):
    results = []
    build_native(build)
    # The enabled native tests are behavior to retain; they must pass.
    env = dict(os.environ, ASAN_OPTIONS="detect_leaks=0")
    subprocess.run(["ctest", "--output-on-failure"], cwd=build, check=True, env=env)
    for entry in entries:
        command = [str(build / entry["command"][0]), *entry["command"][1:]]
        env = dict(os.environ, ASAN_OPTIONS="detect_leaks=0", UBSAN_OPTIONS="print_stacktrace=1")
        run = subprocess.run(command, env=env, capture_output=True, text=True, timeout=600)
        output = run.stdout + run.stderr
        status, detail = classify(output, run.returncode, entry["marker"])
        results.append({**entry, "status": status, "detail": detail, "output": output[-4000:]})
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--gpu", action="store_true", help="also run GPU entries (MOONSHINE_TEST_GPU=1)")
    parser.add_argument("--skip-rust", action="store_true", help="skip Rust entries")
    parser.add_argument("--skip-native", action="store_true", help="skip native entries")
    parser.add_argument("--native-build", type=Path, default=ROOT / "target" / "known-defects" / "inputtino")
    parser.add_argument("--json", type=Path, help="write the results to this file")
    args = parser.parse_args()

    manifest = tomllib.loads(MANIFEST.read_text())
    errors = check_manifest(manifest)
    results = []
    if not args.skip_rust:
        results += run_rust(manifest.get("rust", []), args.gpu)
    if not args.skip_native:
        results += run_native(manifest.get("native", []), args.native_build)

    for result in results:
        name = result.get("test") or result.get("ctest")
        print(f"{result['status']:>16}  {result['finding']:<8} batch {result['batch']}  {name}: {result['detail']}")
        if result["status"] not in ("expected-failure", "not-run"):
            errors.append(f"{result['finding']} {name}: {result['detail']}")
            print(result.get("output", ""), file=sys.stderr)
    skipped = sum(result["status"] == "not-run" for result in results)
    print(f"{sum(r['status'] == 'expected-failure' for r in results)} expected failures, {skipped} not run, {len(errors)} errors")
    if args.json:
        args.json.write_text(json.dumps({"results": results, "errors": errors}, indent=2) + "\n")
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
