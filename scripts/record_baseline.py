#!/usr/bin/env python3
"""Record an identified hardware/software baseline for later comparison.

Every run writes `manifest.json` into `--out`: the exact checkout (SHA,
dirty state, Cargo.lock hash, pinned Git dependencies and native pins), the
toolchain, the host (kernel, CPU, memory, GPU/driver, governor), the built
`moonshine-bench` binary hash and the PyroWave library actually loaded. With
`--metadata-only` nothing else runs.

Otherwise it builds `moonshine-bench` from this checkout immediately before
measuring (so a stale binary cannot be measured), then runs a fixed workload
list and the lifecycle/reconnect resource cycles, keeping raw logs beside
parsed summaries. Baselines are evidence, not repository content: write them
outside the checkout.

The benchmark creates the `moonshine-session.service` user unit and binds the
default stream ports. It refuses to run while that unit is active, because a
live Pyroshine session on the same account would be replaced. `--netns` runs
each benchmark in a private network namespace (no port or mDNS collisions with
a running service); the systemd user unit is still shared.
"""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import tomllib

ROOT = Path(__file__).resolve().parents[1]
ANSI = re.compile(r"\x1b\[[0-9;]*m")

# name, codec, resolution, fps, extra arguments. Fixed so runs are comparable;
# extend only by adding names, never by changing an existing entry.
WORKLOADS = [
    ("h264-1080p60", "h264", "1920x1080", 60, []),
    ("hevc-1080p60", "hevc", "1920x1080", 60, []),
    ("av1-1080p60", "av1", "1920x1080", 60, []),
    ("hevc-1440p120", "hevc", "2560x1440", 120, []),
    ("hevc-2160p60", "hevc", "3840x2160", 60, []),
    ("hevc-1080p60-composited", "hevc", "1920x1080", 60, ["--composited"]),
    ("hevc-1080p60-encrypted-autofec", "hevc", "1920x1080", 60, ["--encrypt-video", "--fec-mode", "auto"]),
    ("pyrowave-1080p60-444", "pyrowave", "1920x1080", 60, ["--chroma", "444"]),
]


def run(command, **kwargs):
    return subprocess.run(command, capture_output=True, text=True, **kwargs)


def output(command):
    try:
        result = run(command)
    except FileNotFoundError:
        return None
    return result.stdout.strip() if result.returncode == 0 else None


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        for chunk in iter(lambda: file.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def checkout():
    status = output(["git", "-C", str(ROOT), "status", "--porcelain"])
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
    pinned = {
        package["name"]: package["source"]
        for package in lock["package"]
        if package.get("source", "").startswith("git+")
    }
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())
    pyrowave = dict(
        re.findall(r'^([A-Z_]+_REVISION)="([0-9a-f]+)"', (ROOT / "scripts" / "build-pyrowave.sh").read_text(), re.M)
    )
    inputtino_base = re.search(r"at\s+`([0-9a-f]{40})`", (ROOT / "vendor" / "inputtino" / "LOCAL_CHANGES.md").read_text())
    return {
        "sha": output(["git", "-C", str(ROOT), "rev-parse", "HEAD"]),
        "branch": output(["git", "-C", str(ROOT), "rev-parse", "--abbrev-ref", "HEAD"]),
        "describe": output(["git", "-C", str(ROOT), "describe", "--always", "--dirty"]),
        "dirty": bool(status),
        "dirty_paths": status.splitlines() if status else [],
        "version": workspace["workspace"]["package"]["version"],
        "cargo_lock_sha256": sha256(ROOT / "Cargo.lock"),
        "pinned_git_dependencies": pinned,
        "pyrowave_pins": pyrowave,
        "pyrowave_patches": sorted(
            f"{path.name}:{sha256(path)[:16]}" for path in (ROOT / "nix" / "patches").glob("*.patch")
        ),
        "inputtino_base": inputtino_base[1] if inputtino_base else None,
    }


def vulkan():
    summary = output(["vulkaninfo", "--summary"]) or ""
    devices = []
    for block in re.split(r"\nGPU\d+:\n", summary)[1:]:
        fields = dict(re.findall(r"^\s*(\w+)\s*=\s*(.+)$", block, re.M))
        devices.append({key: fields.get(key) for key in ("deviceName", "deviceType", "driverName", "driverInfo", "apiVersion", "driverVersion", "vendorID", "deviceID")})
    return devices


def host():
    cpu = {}
    for line in (output(["lscpu"]) or "").splitlines():
        key, _, value = line.partition(":")
        if key.strip() in ("Model name", "CPU(s)", "Thread(s) per core", "CPU max MHz"):
            cpu[key.strip()] = value.strip()
    os_release = dict(
        line.split("=", 1) for line in (read("/etc/os-release") or "").splitlines() if "=" in line
    )
    memory = re.search(r"MemTotal:\s+(\d+)", read("/proc/meminfo") or "")
    return {
        "hostname": os.uname().nodename,
        "kernel": " ".join(os.uname()[2:4]),
        "os": os_release.get("PRETTY_NAME", "").strip('"'),
        "cpu": cpu,
        "memory_kib": int(memory[1]) if memory else None,
        "cpu_governor": read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        "gpus": vulkan(),
        "render_nodes": sorted(path.name for path in Path("/dev/dri").glob("renderD*")),
        "amdgpu_power_profile": [
            read(path) for path in Path("/sys/class/drm").glob("card*/device/power_dpm_force_performance_level")
        ],
    }


def toolchain():
    return {
        "rustc": output(["rustc", "-vV"]),
        "cargo": output(["cargo", "-V"]),
        "cc": (output(["cc", "--version"]) or "").splitlines()[:1],
        "cmake": (output(["cmake", "--version"]) or "").splitlines()[:1],
        "ffmpeg": (output(["ffmpeg", "-version"]) or "").splitlines()[:1],
    }


def pyrowave_library():
    configured = os.environ.get("MOONSHINE_PYROWAVE_LIBRARY")
    candidates = [Path(configured)] if configured else []
    candidates += [ROOT / "target" / "release" / "libpyrowave-shared.so.1", Path("/usr/lib/libpyrowave-shared.so.1")]
    for path in candidates:
        if path.exists():
            return {"path": str(path), "configured": bool(configured), "sha256": sha256(path.resolve())}
    return None


def build_bench():
    started = time.time()
    build = run(["cargo", "build", "--release", "--locked", "-p", "moonshine-tools", "--bin", "moonshine-bench"], cwd=ROOT)
    if build.returncode != 0:
        sys.exit(build.stderr[-4000:])
    binary = ROOT / "target" / "release" / "moonshine-bench"
    return {
        "path": str(binary),
        "sha256": sha256(binary),
        "built_at": datetime.datetime.fromtimestamp(binary.stat().st_mtime, datetime.UTC).isoformat(),
        "build_seconds": round(time.time() - started, 1),
    }


def session_unit_active():
    return output(["systemctl", "--user", "is-active", "moonshine-session.service"]) == "active"


def isolated(command):
    """Run in private user+network namespaces with loopback, keeping the uid."""
    uid, gid = os.getuid(), os.getgid()
    return [
        "unshare", "--user", "--map-root-user", "--net", "sh", "-c",
        f'ip link set lo up && exec unshare --user --map-user={uid} --map-group={gid} "$@"', "sh", *command,
    ]


def number(text):
    return float(text) if "." in text else int(text)


def parse_summary(log):
    """The final `Session [...]` summary of a moonshine-bench run."""
    lines = [ANSI.sub("", line) for line in log.splitlines()]
    starts = [i for i, line in enumerate(lines) if "Session [" in line]
    if not starts:
        return None
    block = lines[starts[-1] : starts[-1] + 14]
    head = re.search(r"Session \[(\d+) frames, ([\d.]+) fps, ([\d.]+) Mbps encoded(?:, ([\d.]+) Mbps ([^\]]+))?\]", block[0])
    if not head:
        return None
    summary = {"frames": int(head[1]), "fps": float(head[2]), "encoded_mbps": float(head[3])}
    if head[4]:
        # "submitted UDP payload" in current builds; older builds said "wire".
        summary["transport_mbps"] = {"value": float(head[4]), "label": head[5]}
    stage = None
    for line in block[1:]:
        named = re.search(r"\b(total|submit|enc_wait):\s+avg=(\d+)us\s+min=(\d+)us\s+max=(\d+)us", line)
        if named:
            stage = named[1]
            summary[stage] = {"avg_us": int(named[2]), "min_us": int(named[3]), "max_us": int(named[4])}
            continue
        percentiles = re.search(r"p50=(\d+)us\s+p95=(\d+)us\s+p99=(\d+)us", line)
        if percentiles and stage and "p50_us" not in summary[stage]:
            summary[stage].update(p50_us=int(percentiles[1]), p95_us=int(percentiles[2]), p99_us=int(percentiles[3]))
            continue
        if "avg breakdown:" in line:
            summary["breakdown_us"] = {key: number(value) for key, value in re.findall(r"(\w+)=([\d.]+)us", line)}
        elif "frame size:" in line:
            summary["frame_size"] = {key: number(value) for key, value in re.findall(r"([\w/]+)=([\d.]+)B?", line)}
        elif "key_frames:" in line:
            summary["key_frames"] = int(line.rsplit(":", 1)[1])
        elif "stale compositor frames dropped:" in line:
            summary["stale_frames_dropped"] = int(line.rsplit(":", 1)[1])
    return summary


def measure(command, log_path, env):
    """Run one benchmark, recording its log, exit status and CPU/peak RSS.

    Resource usage covers the benchmark process and the children it reaped;
    the application runs in its own systemd unit and is not included.
    """
    started = time.monotonic()
    with open(log_path, "w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, env=env, cwd=ROOT)
        _, status, usage = os.wait4(process.pid, 0)
        process.returncode = os.waitstatus_to_exitcode(status)
    return {
        "argv": command,
        "exit_code": process.returncode,
        "wall_seconds": round(time.monotonic() - started, 2),
        "user_cpu_seconds": round(usage.ru_utime, 2),
        "system_cpu_seconds": round(usage.ru_stime, 2),
        "max_rss_kib": usage.ru_maxrss,
        "log": log_path.name,
    }


def cycles_summary(path):
    records = [json.loads(line) for line in path.read_text().splitlines() if line.strip()] if path.exists() else []
    if not records:
        return {"cycles": 0}
    keys = ("fds", "threads", "rss_kib")
    return {
        "cycles": len(records),
        "passed": sum(bool(record.get("pass")) for record in records),
        "first": {key: records[0].get(key) for key in keys},
        "last": {key: records[-1].get(key) for key in keys},
        "max": {key: max(record.get(key, 0) for record in records) for key in keys},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=Path, required=True, help="output directory (outside the checkout)")
    parser.add_argument("--metadata-only", action="store_true")
    parser.add_argument("--allow-dirty", action="store_true", help="measure an uncommitted tree (recorded)")
    parser.add_argument("--netns", action="store_true", help="run benchmarks in private network namespaces")
    parser.add_argument("--app", default="/usr/bin/vkcube", help="application rendered during the benchmark")
    parser.add_argument("--duration", type=int, default=20)
    parser.add_argument("--warmup", type=int, default=4)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--only", nargs="*", help="workload names to run (default: all)")
    parser.add_argument("--cycles", type=int, default=10, help="lifecycle and reconnect cycles (0 to skip)")
    args = parser.parse_args()

    out = args.out.resolve()
    if out.is_relative_to(ROOT):
        sys.exit("write baselines outside the checkout")
    out.mkdir(parents=True, exist_ok=True)
    manifest = {
        "recorded_at": datetime.datetime.now(datetime.UTC).isoformat(),
        "command": sys.argv,
        "checkout": checkout(),
        "toolchain": toolchain(),
        "host": host(),
        "pyrowave_library": pyrowave_library(),
        "runs": [],
        "not_run": [],
    }
    write = lambda: (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    if manifest["checkout"]["dirty"] and not args.allow_dirty and not args.metadata_only:
        sys.exit("the checkout has uncommitted changes; commit them or pass --allow-dirty")
    if args.metadata_only:
        manifest["not_run"].append("benchmarks (--metadata-only)")
        write()
        return 0
    if session_unit_active():
        sys.exit("moonshine-session.service is active: a live session would be replaced; run when it is idle")

    manifest["bench"] = build_bench()
    env = dict(os.environ, MOONSHINE_LOG=os.environ.get("MOONSHINE_LOG", "info"), NO_COLOR="1")
    if manifest["pyrowave_library"]:
        # Load exactly the recorded library rather than whatever dlopen finds.
        env["MOONSHINE_PYROWAVE_LIBRARY"] = manifest["pyrowave_library"]["path"]
    bench = manifest["bench"]["path"]
    wrap = isolated if args.netns else (lambda command: command)
    selected = [workload for workload in WORKLOADS if not args.only or workload[0] in args.only]
    for name, codec, resolution, fps, extra in selected:
        if codec == "pyrowave" and not manifest["pyrowave_library"]:
            manifest["not_run"].append(f"{name}: no PyroWave library found")
            continue
        for repeat in range(1, args.repeats + 1):
            command = wrap([
                bench, "--codec", codec, "--resolution", resolution, "--fps", str(fps),
                "--duration", str(args.duration), "--warmup", str(args.warmup), *extra, args.app,
            ])
            record = measure(command, out / f"{name}.{repeat}.log", env)
            record.update(workload=name, repeat=repeat, summary=parse_summary((out / record["log"]).read_text()))
            manifest["runs"].append(record)
            write()
            print(f"{name} #{repeat}: exit {record['exit_code']}, {record['summary'] and record['summary']['fps']} fps")
    if args.cycles:
        for kind in ("cycles", "reconnect-cycles"):
            cycle_log = out / f"{kind}.jsonl"
            command = wrap([bench, f"--{kind}", str(args.cycles), "--cycle-log", str(cycle_log), args.app])
            record = measure(command, out / f"{kind}.log", env)
            record.update(workload=kind, summary=cycles_summary(cycle_log))
            manifest["runs"].append(record)
            write()
            print(f"{kind}: exit {record['exit_code']}, {record['summary']}")
    write()
    failed = [run for run in manifest["runs"] if run["exit_code"] != 0 or not run["summary"]]
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
