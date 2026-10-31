#!/usr/bin/env python3
"""High-bitrate PyroWave transport matrix on one host.

Runs moonshine-bench once per bitrate (and repeat) with an incompressible
source, optionally beside a GPU-bound probe and/or CPU load, and prints one
row per run: delivered FPS, missed capture slots, frame age, transport and
packetization time, encoded vs submitted UDP bitrate, effective FEC, frames
outstanding at submission, process CPU and peak RSS, and with --gpu-load the
probe's own (the "game's") frame rate and 1% low. Every run's
moonshine-bench JSON is kept in --out.

The benchmark sends to its own loopback receiver: it measures the server's
capture/encode/packetize/pace/send pipeline, not a physical link or a client.

  scripts/pyrowave_high_bitrate/run.py --label after --bench target/release/moonshine-bench \\
      --pyrowave-lib /usr/lib/libpyrowave-shared.so.1 --out runs/after \\
      [--bitrates 500,800,1000,1150,1300,1500] [--chroma 444] [--hdr] \\
      [--gpu-load 1800:96] [--cpu-load 12] [-- <extra moonshine-bench args>]
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
GPULOAD = os.path.join(HERE, "..", "gpu_interference", "build", "gpuload")


def source_command(args):
    if args.source == "vkcube":
        return ["vkcube"]
    width, height = args.resolution.split("x")
    return [
        "mpv", "--no-config", "--fs", "--loop=inf", "--vo=gpu-next", "--gpu-api=vulkan",
        "--glsl-shader=" + os.path.join(HERE, "noise.glsl"),
        f"av://lavfi:color=c=black:s={width}x{height}:r={args.fps}",
    ]


def start_contention(args, probe_json):
    """Start the GPU-bound probe (if any) and CPU load. The probe measures its
    own frame rate over roughly the benchmark's measured window and exits."""
    probe = None
    if args.gpu_load:
        iters, taps = args.gpu_load.split(":")[:2]
        # moonshine-bench takes a few seconds to start before its warmup.
        probe = subprocess.Popen(
            [GPULOAD, "--iters", iters, "--taps", taps, "--seconds", str(args.seconds + args.warmup + 6),
             "--warmup", str(args.warmup + 6), "--json", probe_json],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    busy = []
    for _ in range(args.cpu_load):
        # A plain busy loop per thread at normal priority stands in for a
        # CPU-heavy game competing with the pipeline threads.
        busy.append(subprocess.Popen([sys.executable, "-c", "while True: pass"]))
    return probe, busy


def stop_contention(probe, busy):
    for proc in busy:
        proc.terminate()
    for proc in busy + ([probe] if probe else []):
        try:
            proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            proc.kill()


def row(label, bitrate, repeat, document, probe):
    run = document["runs"][0]
    if run.get("status") != "ok":
        return {"label": label, "mbps": bitrate, "repeat": repeat, "status": run.get("status")}
    game = {}
    if probe:
        game = {"game_fps": probe.get("fps"), "game_1pct_low_fps": probe.get("one_percent_low_fps")}
    summary, pacing, resources = run["summary"], run["pacing"], run.get("resources", {})
    latency = summary["latency_us"]
    breakdown = summary["breakdown_avg_us"]
    fec = summary.get("fec", {})
    outstanding = summary.get("outstanding_at_submit", {})
    process = resources.get("process", {})
    counts = resources.get("process_counts", {})
    return {
        "label": label,
        "mbps": bitrate,
        "repeat": repeat,
        "status": "ok",
        "fps": summary["fps"],
        "delivery": pacing["delivery_ratio"],
        "missed_slots": pacing["arrival"]["missed_frame_slots"],
        "age_p50_us": latency["frame_age"]["p50"],
        "age_p99_us": latency["frame_age"]["p99"],
        "age_max_us": latency["frame_age"]["max"],
        "send_avg_us": breakdown["send"],
        "enqueue_avg_us": breakdown["enqueue"],
        "packetize_avg_us": breakdown["packetize"],
        "encoded_mbps": summary["encoded_mbps"],
        "udp_mbps": summary["submitted_udp_payload_mbps"],
        "fec_pct": fec.get("effective_percentage"),
        "fec_reduced_frames": fec.get("reduced_frames"),
        "outstanding_1/2/3": "/".join(str(outstanding.get(k, 0)) for k in ("1", "2", "3")),
        "cpu_pct": process.get("cpu_percent_of_one_core"),
        "peak_rss_mib": round(counts.get("peak_rss_kib", 0) / 1024, 1),
        **game,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--label", required=True)
    parser.add_argument("--bench", required=True)
    parser.add_argument("--pyrowave-lib", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--bitrates", default="500,800,1000,1150,1300,1500", help="Mbps, comma separated")
    parser.add_argument("--resolution", default="3840x2160")
    parser.add_argument("--fps", type=int, default=120)
    parser.add_argument("--chroma", default="420")
    parser.add_argument("--hdr", action="store_true")
    parser.add_argument("--packet-size", type=int, default=1392)
    parser.add_argument("--seconds", type=int, default=20)
    parser.add_argument("--warmup", type=int, default=4)
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--source", choices=["noise", "vkcube"], default="noise")
    parser.add_argument("--gpu-load", help="gpuload ITERS:TAPS beside the stream (build it with gpu_interference/build.sh)")
    parser.add_argument("--cpu-load", type=int, default=0, help="busy threads beside the stream")
    parser.add_argument("extra", nargs="*", help="additional moonshine-bench arguments (after --)")
    args = parser.parse_args()

    for tool in (["mpv"] if args.source == "noise" else ["vkcube"]):
        if not shutil.which(tool):
            sys.exit(f"{tool} is required for --source {args.source}")
    if args.gpu_load and not os.access(GPULOAD, os.X_OK):
        sys.exit(f"{GPULOAD} missing; run scripts/gpu_interference/build.sh")
    os.makedirs(args.out, exist_ok=True)
    env = dict(os.environ, MOONSHINE_PYROWAVE_LIBRARY=args.pyrowave_lib)
    # Never replace a live service's application unit.
    env.setdefault("MOONSHINE_APPLICATION_UNIT", "moonshine-bench-high-bitrate.service")

    rows = []
    for bitrate in [int(b) for b in args.bitrates.split(",")]:
        for repeat in range(args.repeats):
            name = f"{args.label}-{bitrate}-{args.chroma}{'-hdr' if args.hdr else ''}-{repeat}"
            path = os.path.join(args.out, name + ".json")
            command = [
                args.bench, "--codec", "pyrowave", "--resolution", args.resolution, "--fps", str(args.fps),
                "--bitrate", str(bitrate * 1_000_000), "--chroma", args.chroma, "--full-range",
                "--packet-size", str(args.packet_size), "--duration", str(args.seconds + args.warmup),
                "--warmup", str(args.warmup), "--json", path,
            ]
            if args.hdr:
                command.append("--hdr")
            command += args.extra
            command += ["--"] + source_command(args)
            probe_json = os.path.join(args.out, name + ".probe.json")
            probe, busy = start_contention(args, probe_json)
            time.sleep(2 if probe or busy else 0)
            try:
                with open(os.path.join(args.out, name + ".log"), "w") as log:
                    result = subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT,
                                            timeout=args.seconds + args.warmup + 120)
            finally:
                stop_contention(probe, busy)
            probe_result = None
            if probe and os.path.exists(probe_json):
                with open(probe_json) as f:
                    probe_result = json.load(f)
            if result.returncode != 0 or not os.path.exists(path):
                rows.append({"label": args.label, "mbps": bitrate, "repeat": repeat, "status": f"exit {result.returncode}"})
            else:
                with open(path) as f:
                    rows.append(row(args.label, bitrate, repeat, json.load(f), probe_result))
            print(json.dumps(rows[-1]), flush=True)
            time.sleep(2)

    with open(os.path.join(args.out, f"{args.label}-summary.json"), "w") as f:
        json.dump(rows, f, indent=1)
    keys = list(rows[0].keys()) if rows else []
    for r in rows:
        keys += [k for k in r if k not in keys]
    print("| " + " | ".join(keys) + " |")
    print("|" + "---|" * len(keys))
    for r in rows:
        print("| " + " | ".join(str(r.get(k, "")) for k in keys) + " |")


if __name__ == "__main__":
    main()
