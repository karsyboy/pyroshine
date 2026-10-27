#!/usr/bin/env python3
"""Record an identified hardware/software baseline for later comparison.

Every run writes `manifest.json` into `--out`: the exact checkout (SHA,
dirty state, Cargo.lock hash, pinned Git dependencies and native pins), the
toolchain, the host (kernel, CPU, memory, GPU/driver, governor), the built
`moonshine-bench` binary hash and the PyroWave library actually loaded. With
`--metadata-only` nothing else runs.

Otherwise it builds `moonshine-bench` from this checkout immediately before
measuring (so a stale binary cannot be measured), probes the capture GPU's
capabilities, then runs a named workload set and the lifecycle/reconnect
resource cycles. Each run keeps its raw log and the benchmark's own
`pyroshine-bench/1` JSON; workloads the GPU cannot encode are recorded as
`unsupported`, never run. On NVIDIA hosts `nvidia-smi` is sampled during each
run for device and encoder utilization, which the driver does not expose
through DRM fdinfo. Baselines are evidence, not repository content: write them
outside the checkout. `scripts/bench_results.py` validates and compares them.

The benchmark binds the default stream ports and creates an application unit
(`moonshine-bench.service` here, so a live session's unit is never replaced).
It refuses to run while a live `moonshine-session.service` is active: that
session would compete for the GPU. `--netns` runs each benchmark in a private
network namespace (no port or mDNS collisions with a running service).

On a host without a Rust toolchain or checkout, record the checkout on the
build host with `--metadata-only`, copy the release `moonshine-bench` and that
manifest, and pass `--bench-binary` and `--checkout-from`.
"""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import tomllib

ROOT = Path(__file__).resolve().parents[1]
ANSI = re.compile(r"\x1b\[[0-9;]*m")

# name, codec, resolution, fps, extra arguments. Fixed so runs are comparable;
# extend only by adding names, never by changing an existing entry. A name
# means the same workload in every set.
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

# PyroWave is intra-only and is used at LAN rates; a conventional 20 Mbps
# budget would not exercise its packetization.
PYROWAVE_RATE = ["--bitrate", "500000000"]


def phase0_workloads():
    """Every codec at 1080p/1440p/2160p x 60/120 FPS (SDR 8-bit 4:2:0), plus
    format, capture-path and transport variants at 1080p."""
    workloads = []
    for label, resolution in (("1080p", "1920x1080"), ("1440p", "2560x1440"), ("2160p", "3840x2160")):
        for fps in (60, 120):
            for codec in ("h264", "hevc", "av1"):
                workloads.append((f"{codec}-{label}{fps}", codec, resolution, fps, []))
            workloads.append((f"pyrowave-{label}{fps}-500m", "pyrowave", resolution, fps, PYROWAVE_RATE))
    workloads += [
        ("h264-1080p60-444", "h264", "1920x1080", 60, ["--chroma", "444"]),
        ("hevc-1080p60-444", "hevc", "1920x1080", 60, ["--chroma", "444"]),
        ("av1-1080p60-444", "av1", "1920x1080", 60, ["--chroma", "444"]),
        ("hevc-1080p60-10bit", "hevc", "1920x1080", 60, ["--bit-depth", "10"]),
        ("hevc-1080p60-hdr10", "hevc", "1920x1080", 60, ["--hdr"]),
        ("av1-1080p60-hdr10", "av1", "1920x1080", 60, ["--hdr"]),
        ("hevc-1080p60-444-hdr10", "hevc", "1920x1080", 60, ["--chroma", "444", "--hdr"]),
        ("pyrowave-1080p60-444-500m", "pyrowave", "1920x1080", 60, ["--chroma", "444", *PYROWAVE_RATE]),
        ("pyrowave-1080p60-hdr10-500m", "pyrowave", "1920x1080", 60, ["--hdr", *PYROWAVE_RATE]),
        ("hevc-1080p60-composited", "hevc", "1920x1080", 60, ["--composited"]),
        ("pyrowave-1080p60-composited-500m", "pyrowave", "1920x1080", 60, ["--composited", *PYROWAVE_RATE]),
        ("hevc-1080p60-cursor", "hevc", "1920x1080", 60, ["--cursor", "moving"]),
        ("hevc-1080p120-vrr", "hevc", "1920x1080", 120, ["--vrr"]),
        ("hevc-1080p60-encrypted-autofec", "hevc", "1920x1080", 60, ["--encrypt-video", "--fec-mode", "auto"]),
    ]
    return workloads


WORKLOAD_SETS = {"default": WORKLOADS, "phase0": phase0_workloads()}


def workload_set(name):
    """A named set, or `all`: every workload once, checking that a name never
    means two different workloads."""
    if name != "all":
        return WORKLOAD_SETS[name]
    merged = {}
    for workloads in WORKLOAD_SETS.values():
        for workload in workloads:
            if merged.setdefault(workload[0], workload) != workload:
                raise ValueError(f"workload {workload[0]} is defined differently in two sets")
    return list(merged.values())


# Exit status of a benchmark run whose workload the GPU cannot encode.
EXIT_UNSUPPORTED = 3
# Applications started by the baseline run use their own unit.
BENCH_UNIT = "moonshine-bench.service"


def run(command, **kwargs):
    return subprocess.run(command, capture_output=True, text=True, **kwargs)


def output(command):
    try:
        result = run(command, timeout=10)
    except (FileNotFoundError, subprocess.TimeoutExpired):
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
    vendor_bases = {
        name: dict(line.split(" ", 1) for line in
                   (ROOT / f"vendor/{name}-patches/UPSTREAM").read_text().splitlines())["commit"]
        for name in ("inputtino", "pixelforge")
    }
    return {
        "sha": output(["git", "-C", str(ROOT), "rev-parse", "HEAD"]),
        "branch": output(["git", "-C", str(ROOT), "rev-parse", "--abbrev-ref", "HEAD"]),
        "describe": output(["git", "-C", str(ROOT), "describe", "--always", "--dirty"]),
        "dirty": bool(status),
        "dirty_paths": status.splitlines() if status else [],
        "version": workspace["workspace"]["package"]["version"],
        "cargo_lock_sha256": sha256(ROOT / "Cargo.lock"),
        "capture_encode_sources": {
            str(path.relative_to(ROOT)): sha256(path)
            for directory, pattern in (
                ("moonshine-core/src/session/capture", "*.rs"),
                ("moonshine-core/src/session/stream/video/pipeline", "*.rs"),
                ("moonshine-core/src/desktop", "*.rs"),
                ("moonshine-core/protocols", "*.xml"),
            )
            for path in sorted((ROOT / directory).rglob(pattern))
        },
        "pinned_git_dependencies": pinned,
        "pyrowave_pins": pyrowave,
        "pyrowave_patches": sorted(
            f"{path.name}:{sha256(path)[:16]}" for path in (ROOT / "nix" / "patches").glob("*.patch")
        ),
        "inputtino_base": vendor_bases["inputtino"],
        "pixelforge_base": vendor_bases["pixelforge"],
        "vendor_patches": {
            name: sorted(f"{path.name}:{sha256(path)}" for path in
                         (ROOT / f"vendor/{name}-patches").glob("*.patch"))
            for name in vendor_bases
        },
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
        "desktop": {
            "current_desktop": os.environ.get("XDG_CURRENT_DESKTOP"),
            "session_type": os.environ.get("XDG_SESSION_TYPE"),
            "kwin": output(["kwin_wayland", "--version"]),
            "gnome_shell": output(["gnome-shell", "--version"]),
            "pipewire": output(["pipewire", "--version"]),
        },
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


def existing_bench(path):
    binary = Path(path).resolve()
    return {
        "path": str(binary),
        "sha256": sha256(binary),
        "built_at": datetime.datetime.fromtimestamp(binary.stat().st_mtime, datetime.UTC).isoformat(),
        "prebuilt": True,
    }


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


NVIDIA_FIELDS = "timestamp,index,pci.bus_id,utilization.gpu,utilization.encoder,memory.used,power.draw,clocks.gr"


class NvidiaSampler:
    """`nvidia-smi` every 500 ms while one benchmark runs, into a CSV beside
    its log. Device-wide: it includes every other GPU user."""

    def __init__(self, csv_path):
        self.csv_path = csv_path
        self.started = time.monotonic()
        self.file = open(csv_path, "w")
        self.process = subprocess.Popen(
            ["nvidia-smi", f"--query-gpu={NVIDIA_FIELDS}", "--format=csv,noheader,nounits", "-lms", "500"],
            stdout=self.file, stderr=subprocess.STDOUT,
        )

    def finish(self, skip_seconds):
        self.process.terminate()
        self.process.wait()
        self.file.close()
        return summarize_nvidia(self.csv_path.read_text(), skip_seconds)


def summarize_nvidia(text, skip_seconds):
    """Average/p95/max of each device's GPU and encoder utilization, excluding
    the first `skip_seconds` (benchmark setup and warmup)."""
    rows = [[field.strip() for field in line.split(",")] for line in text.splitlines() if line.count(",") >= 7]
    stamps = []
    for row in rows:
        try:
            stamps.append(datetime.datetime.strptime(row[0], "%Y/%m/%d %H:%M:%S.%f"))
        except ValueError:
            stamps.append(None)
    first = next((stamp for stamp in stamps if stamp), None)
    devices = {}
    for row, stamp in zip(rows, stamps):
        if first is None or stamp is None or (stamp - first).total_seconds() < skip_seconds:
            continue
        device = devices.setdefault(row[2], {"gpu_percent": [], "encoder_percent": [], "power_w": []})
        for key, value in (("gpu_percent", row[3]), ("encoder_percent", row[4]), ("power_w", row[6])):
            try:
                device[key].append(float(value))
            except ValueError:
                pass
    def stats(values):
        if not values:
            return None
        ordered = sorted(values)
        return {
            "avg": round(sum(ordered) / len(ordered), 2),
            "p95": ordered[min(len(ordered) - 1, max(0, -(-len(ordered) * 95 // 100) - 1))],
            "max": ordered[-1],
            "samples": len(ordered),
        }
    return {address: {key: stats(values) for key, values in fields.items()} for address, fields in devices.items()}


def measure(command, log_path, env, sample_nvidia=False, skip_seconds=0):
    """Run one benchmark, recording its log, exit status and CPU/peak RSS.

    Resource usage covers the benchmark process and the children it reaped;
    the application runs in its own systemd unit and is not included.
    """
    started = time.monotonic()
    sampler = NvidiaSampler(log_path.with_suffix(".nvidia.csv")) if sample_nvidia else None
    with open(log_path, "w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, env=env, cwd=ROOT)
        _, status, usage = os.wait4(process.pid, 0)
        process.returncode = os.waitstatus_to_exitcode(status)
    nvidia = sampler.finish(skip_seconds) if sampler else None
    return {
        "argv": command,
        "exit_code": process.returncode,
        "wall_seconds": round(time.monotonic() - started, 2),
        "user_cpu_seconds": round(usage.ru_utime, 2),
        "system_cpu_seconds": round(usage.ru_stime, 2),
        "max_rss_kib": usage.ru_maxrss,
        "log": log_path.name,
        **({"nvidia_smi": nvidia} if sampler else {}),
    }


def cycles_summary(path):
    records = [json.loads(line) for line in path.read_text().splitlines() if line.strip()] if path.exists() else []
    if not records:
        return {"cycles": 0}
    keys = ("fds", "dmabuf_fds", "threads", "rss_kib")
    return {
        "cycles": len(records),
        "passed": sum(bool(record.get("pass")) for record in records),
        "first": {key: records[0].get(key) for key in keys},
        "last": {key: records[-1].get(key) for key in keys},
        "max": {key: max(record.get(key, 0) for record in records) for key in keys},
        "failed_checks": sorted({check for record in records for check in record.get("failed", [])}),
    }


def probe_capabilities(bench, wrap, env, out):
    """The capture GPU's capabilities as the benchmark (and server) probe them."""
    path = out / "capabilities.json"
    result = run(wrap([bench, "--capabilities", "--json", str(path)]), env=env, cwd=ROOT)
    if result.returncode != 0 or not path.exists():
        return {"error": (result.stdout + result.stderr)[-2000:]}
    return json.loads(path.read_text())["capabilities"]


def cycle_codecs(capabilities, have_pyrowave):
    """Codecs the lifecycle cycles rotate through: only those the GPU encodes."""
    profiles = set(capabilities.get("profiles", []))
    codecs = [codec for codec in ("h264", "hevc", "av1") if codec in profiles]
    if have_pyrowave and "pyrowave" in profiles:
        codecs.append("pyrowave")
    return codecs


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
    parser.add_argument("--set", default="default", choices=[*WORKLOAD_SETS, "all"], help="workload set")
    parser.add_argument("--only", nargs="*", help="workload names to run (default: the whole set)")
    parser.add_argument("--cycles", type=int, default=10, help="lifecycle and reconnect cycles (0 to skip)")
    parser.add_argument("--bench-binary", help="measure this prebuilt moonshine-bench instead of building one")
    parser.add_argument("--checkout-from", type=Path,
                        help="take the checkout provenance from this manifest (recorded where the binary was built)")
    parser.add_argument("--application-unit", default=BENCH_UNIT, help="application unit name for benchmark runs")
    args = parser.parse_args()

    out = args.out.resolve()
    if out.is_relative_to(ROOT):
        sys.exit("write baselines outside the checkout")
    out.mkdir(parents=True, exist_ok=True)
    provenance = json.loads(args.checkout_from.read_text())["checkout"] if args.checkout_from else checkout()
    manifest = {
        "schema": "pyroshine-baseline/1",
        "recorded_at": datetime.datetime.now(datetime.UTC).isoformat(),
        "command": sys.argv,
        "workload_set": args.set,
        "checkout": provenance,
        "toolchain": toolchain(),
        "host": host(),
        "pyrowave_library": pyrowave_library(),
        "runs": [],
        "not_run": [],
    }
    write = lambda: (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    if manifest["checkout"].get("dirty") and not args.checkout_from:
        # An uncommitted tree is reproducible only with its exact changes.
        diff = run(["git", "-C", str(ROOT), "diff", "HEAD", "--binary"]).stdout
        untracked = output(["git", "-C", str(ROOT), "ls-files", "--others", "--exclude-standard"]) or ""
        (out / "checkout.diff").write_text(diff)
        manifest["checkout"]["diff"] = {
            "file": "checkout.diff",
            "sha256": hashlib.sha256(diff.encode()).hexdigest(),
            "untracked": untracked.splitlines(),
        }
    if manifest["checkout"]["dirty"] and not args.allow_dirty and not args.metadata_only:
        sys.exit("the checkout has uncommitted changes; commit them or pass --allow-dirty")
    if args.metadata_only:
        manifest["not_run"].append("benchmarks (--metadata-only)")
        write()
        return 0
    if session_unit_active():
        sys.exit("moonshine-session.service is active: a live session would be replaced; run when it is idle")

    manifest["bench"] = existing_bench(args.bench_binary) if args.bench_binary else build_bench()
    env = dict(
        os.environ,
        MOONSHINE_LOG=os.environ.get("MOONSHINE_LOG", "info"),
        NO_COLOR="1",
        MOONSHINE_APPLICATION_UNIT=args.application_unit,
    )
    if manifest["pyrowave_library"]:
        # Load exactly the recorded library rather than whatever dlopen finds.
        env["MOONSHINE_PYROWAVE_LIBRARY"] = manifest["pyrowave_library"]["path"]
    bench = manifest["bench"]["path"]
    wrap = isolated if args.netns else (lambda command: command)
    manifest["capabilities"] = probe_capabilities(bench, wrap, env, out)
    sample_nvidia = shutil.which("nvidia-smi") is not None and any(
        (gpu.get("vendorID") or "").lower() == "0x10de" for gpu in manifest["host"]["gpus"]
    )
    write()
    selected = [workload for workload in workload_set(args.set) if not args.only or workload[0] in args.only]
    for name, codec, resolution, fps, extra in selected:
        if codec == "pyrowave" and not manifest["pyrowave_library"]:
            manifest["not_run"].append(f"{name}: no PyroWave library found")
            continue
        for repeat in range(1, args.repeats + 1):
            json_path = out / f"{name}.{repeat}.json"
            command = wrap([
                bench, "--skip-unsupported", "--json", str(json_path), "--codec", codec,
                "--resolution", resolution, "--fps", str(fps), "--duration", str(args.duration),
                "--warmup", str(args.warmup), *extra, args.app,
            ])
            record = measure(command, out / f"{name}.{repeat}.log", env, sample_nvidia, args.warmup + 2)
            result = json.loads(json_path.read_text())["runs"][0] if json_path.exists() else None
            record.update(
                workload=name,
                repeat=repeat,
                bench_json=json_path.name if result else None,
                status=result["status"] if result else "failed",
                summary=parse_summary((out / record["log"]).read_text()),
            )
            manifest["runs"].append(record)
            write()
            if record["status"] == "unsupported":
                print(f"{name}: unsupported ({result.get('reason')})")
                manifest["not_run"].append(f"{name}: unsupported: {result.get('reason')}")
                break
            print(f"{name} #{repeat}: exit {record['exit_code']}, {record['status']}, "
                  f"{record['summary'] and record['summary']['fps']} fps")
    if args.cycles:
        codecs = cycle_codecs(manifest["capabilities"], bool(manifest["pyrowave_library"]))
        for kind in ("cycles", "reconnect-cycles"):
            if not codecs:
                manifest["not_run"].append(f"{kind}: the GPU encodes no codec")
                continue
            cycle_log = out / f"{kind}.jsonl"
            command = wrap([
                bench, f"--{kind}", str(args.cycles), "--cycle-codecs", ",".join(codecs),
                "--cycle-log", str(cycle_log), args.app,
            ])
            record = measure(command, out / f"{kind}.log", env)
            record.update(workload=kind, cycle_codecs=codecs, summary=cycles_summary(cycle_log))
            record["status"] = "ok" if record["exit_code"] == 0 else "failed"
            manifest["runs"].append(record)
            write()
            print(f"{kind}: exit {record['exit_code']}, {record['summary']}")
    # The benchmark's documents must satisfy the schema the comparison relies on.
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from bench_results import validate_bench
    for record in manifest["runs"]:
        if record.get("bench_json"):
            errors = validate_bench(json.loads((out / record["bench_json"]).read_text()), record["bench_json"])
            record["validation_errors"] = errors
            if errors:
                record["status"] = "invalid"
    write()
    failed = [
        run for run in manifest["runs"]
        if run["status"] != "unsupported" and (run["exit_code"] != 0 or run["status"] not in ("ok",))
    ]
    for record in failed:
        print(f"FAILED: {record['workload']} #{record.get('repeat', 1)} ({record['status']}, exit {record['exit_code']})")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
