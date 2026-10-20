#!/usr/bin/env python3
"""Run the hardware-dependent (ignored) tests a host supports and report the rest.

Ordinary `cargo test` reports these tests as ignored, never as passed.
`scripts/hardware_tests.toml` lists every one with its requirements. This tool:

- `check-registry`: fails unless the registry and the workspace's ignored
  tests match exactly (run in CI, so no hardware test goes unlisted);
- `report`: probes requirements and writes which tests this host can run,
  without running anything (CI writes it as its GPU-availability report);
- `run`: runs every available test, or `--only` names, each alone, and
  records passed/failed/skipped with the missing requirement. A test passes
  only when its harness reports exactly one passed test.

On a host without a Rust toolchain, build the test binary elsewhere
(`cargo test -p moonshine-core --lib --no-run --release` prints its path),
copy it, and pass `--test-binary`.
"""

import argparse
import datetime
import glob
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
REGISTRY = Path(__file__).with_name("hardware_tests.toml")
KINDS = ("test", "informational", "helper")
REQUIREMENTS = (
    "render-node",
    "vulkan-video-encode",
    "xwayland",
    "ffmpeg",
    "pacat",
    "systemd-user",
    "input-devices",
    "nvidia-gpu",
    "vaapi-encode",
    "nvenc",
    "pyrowave-library",
    "unsupported-render-node",
)


def load_registry(path=REGISTRY):
    entries = tomllib.loads(Path(path).read_text())["test"]
    names = set()
    for entry in entries:
        entry.setdefault("crate", "moonshine-core")
        entry.setdefault("kind", "test")
        entry.setdefault("requires", [])
        entry.setdefault("env", {})
        entry.setdefault("timeout", None)
        if entry["kind"] not in KINDS:
            raise ValueError(f"{entry['name']}: unknown kind {entry['kind']!r}")
        unknown = set(entry["requires"]) - set(REQUIREMENTS)
        if unknown:
            raise ValueError(f"{entry['name']}: unknown requirements {sorted(unknown)}")
        if entry["name"] in names:
            raise ValueError(f"{entry['name']}: listed twice")
        names.add(entry["name"])
    return entries


def parse_test_list(output):
    """Test names from `cargo test -- --list` output (`name: test` lines)."""
    return {line[: -len(": test")] for line in output.splitlines() if line.endswith(": test")}


def ignored_tests(crate):
    result = subprocess.run(
        ["cargo", "test", "-p", crate, "--lib", "--all-features", "--", "--list", "--ignored"],
        cwd=ROOT, capture_output=True, text=True,
    )
    if result.returncode != 0:
        sys.exit(result.stderr[-4000:])
    return parse_test_list(result.stdout)


def registry_differences(entries, listed_by_crate):
    """(missing from the registry, listed but no longer ignored tests)."""
    registered = {(entry["crate"], entry["name"]) for entry in entries}
    listed = {(crate, name) for crate, names in listed_by_crate.items() for name in names}
    return sorted(listed - registered), sorted(registered - listed)


def writable(path):
    return os.path.exists(path) and os.access(path, os.R_OK | os.W_OK)


def drm_driver(node):
    try:
        return os.path.basename(os.readlink(f"/sys/class/drm/{os.path.basename(node)}/device/driver"))
    except OSError:
        return None


def pci_address(node):
    try:
        return os.path.basename(os.path.realpath(f"/sys/class/drm/{os.path.basename(node)}/device"))
    except OSError:
        return None


def vaapi_encode(node):
    """Whether libva initializes on `node` and offers an H.264/HEVC encoder."""
    import ctypes

    try:
        va = ctypes.CDLL("libva.so.2")
        drm = ctypes.CDLL("libva-drm.so.2")
    except OSError as error:
        return False, str(error)
    drm.vaGetDisplayDRM.restype = ctypes.c_void_p
    for name in ("vaInitialize", "vaTerminate", "vaMaxNumEntrypoints", "vaQueryConfigEntrypoints"):
        getattr(va, name).argtypes = None
    try:
        fd = os.open(node, os.O_RDWR)
    except OSError as error:
        return False, f"{node}: {error}"
    try:
        display = ctypes.c_void_p(drm.vaGetDisplayDRM(fd))
        major, minor = ctypes.c_int(), ctypes.c_int()
        if not display.value or va.vaInitialize(display, ctypes.byref(major), ctypes.byref(minor)) != 0:
            return False, f"no VA-API driver for {node}"
        try:
            # VAProfileH264High (7), VAProfileHEVCMain (17); EncSlice (6), EncSliceLP (8).
            entrypoints = (ctypes.c_int * max(va.vaMaxNumEntrypoints(display), 1))()
            for profile in (7, 17):
                count = ctypes.c_int()
                if va.vaQueryConfigEntrypoints(display, profile, entrypoints, ctypes.byref(count)) == 0 and any(
                    entrypoints[i] in (6, 8) for i in range(count.value)
                ):
                    return True, f"VA-API encoder on {node}"
            return False, f"no VA-API H.264/HEVC encoder on {node}"
        finally:
            va.vaTerminate(display)
    finally:
        os.close(fd)


def nvenc_available(nodes):
    import ctypes

    if not any(drm_driver(node) == "nvidia" for node in nodes):
        return False, "no NVIDIA render node"
    try:
        ctypes.CDLL("libcuda.so.1")
        ctypes.CDLL("libnvidia-encode.so.1")
    except OSError as error:
        return False, str(error)
    return True, "libcuda.so.1 and libnvidia-encode.so.1"


def probe(args):
    """Each requirement: (available, detail). Probing never runs a test."""
    nodes = sorted(glob.glob("/dev/dri/renderD*"))
    usable = [node for node in nodes if writable(node)]
    found = {}
    found["render-node"] = (bool(usable), ", ".join(f"{n} ({drm_driver(n)})" for n in usable) or "no accessible render node")
    vulkaninfo = shutil.which("vulkaninfo")
    if vulkaninfo:
        result = subprocess.run([vulkaninfo], capture_output=True, text=True)
        encode = "VK_KHR_video_encode_queue" in result.stdout
        found["vulkan-video-encode"] = (encode, "VK_KHR_video_encode_queue " + ("present" if encode else "absent"))
    else:
        found["vulkan-video-encode"] = (False, "vulkaninfo not installed; cannot confirm Vulkan Video encode")
    for requirement, binary in (("xwayland", "Xwayland"), ("ffmpeg", "ffmpeg"), ("pacat", "pacat")):
        path = shutil.which(binary)
        found[requirement] = (bool(path), path or f"{binary} not found")
    systemd = subprocess.run(["systemctl", "--user", "is-system-running"], capture_output=True, text=True) \
        if shutil.which("systemctl") else None
    state = systemd.stdout.strip() if systemd else ""
    found["systemd-user"] = (state in ("running", "degraded"), f"user manager: {state or 'unreachable'}")
    devices = writable("/dev/uinput") and writable("/dev/uhid")
    found["input-devices"] = (devices, "/dev/uinput and /dev/uhid writable" if devices else "/dev/uinput or /dev/uhid not writable")
    nvidia = os.environ.get("MOONSHINE_TEST_NVML") or next(
        (pci_address(node) for node in nodes if drm_driver(node) == "nvidia"), None
    )
    found["nvidia-gpu"] = (bool(nvidia), f"NVML device {nvidia}" if nvidia else "no NVIDIA render node")
    test_node = os.environ.get("MOONSHINE_TEST_RENDER_NODE") or (usable[0] if usable else None)
    found["vaapi-encode"] = vaapi_encode(test_node) if test_node else (False, "no accessible render node")
    found["nvenc"] = nvenc_available(nodes)
    library = args.pyrowave_library or os.environ.get("MOONSHINE_PYROWAVE_LIBRARY")
    found["pyrowave-library"] = (
        bool(library and os.path.exists(library)),
        library or "no PyroWave library (--pyrowave-library or MOONSHINE_PYROWAVE_LIBRARY)",
    )
    unsupported = os.environ.get("MOONSHINE_TEST_UNSUPPORTED_RENDER_NODE")
    found["unsupported-render-node"] = (
        bool(unsupported and writable(unsupported)),
        unsupported or "MOONSHINE_TEST_UNSUPPORTED_RENDER_NODE not set",
    )
    environment = {}
    if nvidia:
        environment["MOONSHINE_TEST_NVML"] = nvidia
    if library:
        environment["MOONSHINE_PYROWAVE_LIBRARY"] = library
    return found, environment


def plan(entries, found, include_informational=False, only=None):
    """Each entry with `run` or the reason it is skipped."""
    planned = []
    for entry in entries:
        missing = [requirement for requirement in entry["requires"] if not found[requirement][0]]
        if only is not None and entry["name"] not in only:
            reason = "not selected"
        elif entry["kind"] == "helper":
            reason = "helper process of another test"
        elif entry["kind"] == "informational" and not include_informational:
            reason = "informational (--include-informational)"
        elif missing:
            reason = "missing " + ", ".join(f"{m} ({found[m][1]})" for m in missing)
        else:
            reason = None
        planned.append((entry, reason))
    return planned


def passed(exit_code, output):
    """Exactly one test ran and passed: a filter that matched nothing fails."""
    return exit_code == 0 and re.search(r"test result: ok\. 1 passed; 0 failed", output) is not None


def run_test(entry, environment, args, log_path):
    if args.test_binary:
        command = [args.test_binary, entry["name"], "--ignored", "--exact", "--nocapture", "--test-threads=1"]
    else:
        command = [
            "cargo", "test", *(["--release"] if args.release else []), "-p", entry["crate"], "--lib",
            "--all-features", entry["name"], "--", "--ignored", "--exact", "--nocapture", "--test-threads=1",
        ]
    env = dict(os.environ, **environment, **entry["env"])
    limit = entry["timeout"] or args.timeout
    started = datetime.datetime.now(datetime.UTC)
    try:
        result = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True, timeout=limit)
        exit_code, output = result.returncode, result.stdout + result.stderr
    except subprocess.TimeoutExpired as timeout:
        exit_code = None
        output = (timeout.stdout or b"").decode(errors="replace") + (timeout.stderr or b"").decode(errors="replace")
        output += f"\n[hardware_tests] timed out after {limit} s\n"
    log_path.write_text(output)
    return {
        "status": "passed" if exit_code is not None and passed(exit_code, output) else "failed",
        **({"timed_out_after_s": limit} if exit_code is None else {}),
        "exit_code": exit_code,
        "seconds": round((datetime.datetime.now(datetime.UTC) - started).total_seconds(), 1),
        "log": log_path.name,
        "env": {**entry["env"], **{k: v for k, v in environment.items() if k in ("MOONSHINE_TEST_NVML", "MOONSHINE_PYROWAVE_LIBRARY")}},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("command", choices=("check-registry", "report", "run"))
    parser.add_argument("--json", type=Path, help="write the report here")
    parser.add_argument("--out", type=Path, help="run: directory for per-test logs (default: beside --json)")
    parser.add_argument("--only", nargs="*", help="run: test names to run (default: all available)")
    parser.add_argument("--include-informational", action="store_true")
    parser.add_argument("--require", nargs="*", default=[], choices=REQUIREMENTS,
                        help="fail when one of these requirements is unavailable (for a host that must test them)")
    parser.add_argument("--pyrowave-library", help="pinned PyroWave library for PyroWave tests")
    parser.add_argument("--test-binary", help="prebuilt moonshine-core test binary instead of cargo")
    parser.add_argument("--release", action="store_true", help="run through cargo in release mode")
    parser.add_argument("--timeout", type=int, default=900, help="seconds per test")
    args = parser.parse_args()

    entries = load_registry()
    if args.command == "check-registry":
        crates = sorted({entry["crate"] for entry in entries} | {"moonshine-core"})
        missing, stale = registry_differences(entries, {crate: ignored_tests(crate) for crate in crates})
        for crate, name in missing:
            print(f"ignored test missing from {REGISTRY.name}: {crate} {name}")
        for crate, name in stale:
            print(f"{REGISTRY.name} lists a test that is not an ignored test: {crate} {name}")
        if missing or stale:
            return 1
        print(f"{len(entries)} ignored tests registered")
        return 0

    found, environment = probe(args)
    planned = plan(entries, found, args.include_informational, set(args.only) if args.only else None)
    report = {
        "recorded_at": datetime.datetime.now(datetime.UTC).isoformat(),
        "host": os.uname().nodename,
        "kernel": os.uname().release,
        "requirements": {name: {"available": ok, "detail": detail} for name, (ok, detail) in found.items()},
        "tests": [],
    }
    log_dir = args.out or (args.json.parent if args.json else None)
    if args.command == "run" and log_dir is None:
        sys.exit("run needs --json or --out for its logs")
    if log_dir:
        log_dir.mkdir(parents=True, exist_ok=True)
    failures = 0
    for entry, reason in planned:
        record = {"name": entry["name"], "kind": entry["kind"], "requires": entry["requires"]}
        if reason is not None:
            record.update(status="skipped", reason=reason)
        elif args.command == "report":
            record.update(status="available")
        else:
            print(f"running {entry['name']} ...", flush=True)
            record.update(run_test(entry, environment, args, log_dir / (entry["name"].replace("::", ".") + ".log")))
            failures += record["status"] == "failed"
        print(f"{record['status']:>9}  {entry['name']}" + (f"  ({record['reason']})" if "reason" in record else ""))
        report["tests"].append(record)
    counts = {}
    for record in report["tests"]:
        counts[record["status"]] = counts.get(record["status"], 0) + 1
    report["counts"] = counts
    unavailable = [name for name in args.require if not found[name][0]]
    report["required_but_unavailable"] = unavailable
    if args.json:
        args.json.write_text(json.dumps(report, indent=2) + "\n")
    print(", ".join(f"{count} {status}" for status, count in sorted(counts.items())))
    for name in unavailable:
        print(f"required but unavailable: {name} ({found[name][1]})")
    return 1 if failures or unavailable else 0


if __name__ == "__main__":
    sys.exit(main())
