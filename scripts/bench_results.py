#!/usr/bin/env python3
"""Validate, summarize and compare Pyroshine benchmark results.

Inputs are `moonshine-bench --json` documents (`pyroshine-bench/1`) or
baseline directories/manifests written by `scripts/record_baseline.py`
(`pyroshine-baseline/1`, whose runs reference per-run bench documents).

- `validate PATH...`: structural and consistency checks; exit 1 on errors.
- `summarize PATH`: per-workload medians across repeats.
- `compare BASE NEW`: per-workload median deltas for the key metrics. It only
  reports by default. `--fail-on METRIC=PERCENT` (repeatable) turns a change
  for the worse beyond PERCENT into a failure. No thresholds are built in:
  choose them from repeated runs on the hardware being compared.

Only the standard library is used.
"""

import argparse
import json
from pathlib import Path
import re
import statistics
import sys

BENCH_SCHEMA = "pyroshine-bench/1"
BASELINE_SCHEMA = "pyroshine-baseline/1"
STATUSES = {"ok", "no_frames", "failed", "interrupted", "unsupported"}
CODECS = {"h264", "hevc", "av1", "pyrowave"}
DISTRIBUTIONS = ("total", "frame_age", "submit", "encode_wait", "slot_wait", "source_interval")


def total_gpu_ms(run):
    engines = (((run.get("resources") or {}).get("process") or {}).get("gpu_engine_busy_ms_per_s")) or {}
    return sum(engines.values()) if engines else None


def path(*keys):
    def get(run):
        value = run
        for key in keys:
            if not isinstance(value, dict):
                return None
            value = value.get(key)
        return value
    return get


# name: (extractor, direction). direction +1: higher is better, -1: lower is
# better, 0: informational (never a regression).
METRICS = {
    "fps": (path("summary", "fps"), +1),
    "delivery_ratio": (path("pacing", "delivery_ratio"), +1),
    "total_p50_us": (path("summary", "latency_us", "total", "p50"), -1),
    "total_p99_us": (path("summary", "latency_us", "total", "p99"), -1),
    "total_max_us": (path("summary", "latency_us", "total", "max"), -1),
    "frame_age_p99_us": (path("summary", "latency_us", "frame_age", "p99"), -1),
    "convert_avg_us": (path("summary", "breakdown_avg_us", "convert"), -1),
    "encode_wait_p50_us": (path("summary", "latency_us", "encode_wait", "p50"), -1),
    "arrival_interval_p99_us": (path("pacing", "arrival", "interval_us", "p99"), -1),
    "arrival_stddev_us": (path("pacing", "arrival", "stddev_us"), -1),
    "missed_frame_slots": (path("pacing", "arrival", "missed_frame_slots"), -1),
    "cpu_percent": (path("resources", "process", "cpu_percent_of_one_core"), -1),
    "gpu_engine_ms_per_s": (total_gpu_ms, -1),
    "peak_rss_kib": (path("resources", "process_counts", "peak_rss_kib"), -1),
    "encoded_mbps": (path("summary", "encoded_mbps"), 0),
    "composited_frames": (path("summary", "capture", "composited"), 0),
}


def check_distribution(errors, where, distribution):
    if not isinstance(distribution, dict):
        errors.append(f"{where}: missing")
        return
    keys = ("min", "p50", "p95", "p99", "max")
    if any(not isinstance(distribution.get(key), (int, float)) for key in (*keys, "avg")):
        errors.append(f"{where}: needs numeric avg/min/p50/p95/p99/max")
        return
    values = [distribution[key] for key in keys]
    if values != sorted(values):
        errors.append(f"{where}: percentiles are not ordered: {values}")
    if values[-1] and not values[0] <= distribution["avg"] <= values[-1]:
        errors.append(f"{where}: average {distribution['avg']} outside [min, max]")


def validate_run(run, where):
    errors = []
    workload = run.get("workload")
    if not isinstance(workload, dict):
        return [f"{where}: missing workload"]
    if workload.get("codec") not in CODECS:
        errors.append(f"{where}: unknown codec {workload.get('codec')!r}")
    if not re.fullmatch(r"\d+x\d+", str(workload.get("resolution"))):
        errors.append(f"{where}: resolution {workload.get('resolution')!r} is not WxH")
    if not isinstance(workload.get("fps"), int) or workload["fps"] <= 0:
        errors.append(f"{where}: fps must be a positive integer")
    status = run.get("status")
    if status not in STATUSES:
        return errors + [f"{where}: unknown status {status!r}"]
    if status == "unsupported" and not run.get("reason"):
        errors.append(f"{where}: unsupported without a reason")
    if status == "failed" and not run.get("error"):
        errors.append(f"{where}: failed without an error")
    if status != "ok":
        return errors
    summary = run.get("summary")
    if not isinstance(summary, dict) or not summary.get("frames"):
        return errors + [f"{where}: ok without frames"]
    for name in DISTRIBUTIONS:
        check_distribution(errors, f"{where}: latency {name}", (summary.get("latency_us") or {}).get(name))
    capture = summary.get("capture") or {}
    captured = sum(capture.get(key, 0) for key in ("direct", "direct_late", "composited"))
    if captured != summary["frames"]:
        errors.append(f"{where}: capture paths sum to {captured}, not {summary['frames']} frames")
    pacing = run.get("pacing")
    if not isinstance(pacing, dict):
        errors.append(f"{where}: ok without pacing")
    else:
        if pacing.get("delivered_frames") != summary["frames"]:
            errors.append(f"{where}: pacing delivered {pacing.get('delivered_frames')} != {summary['frames']} frames")
        arrival = pacing.get("arrival") or {}
        if arrival.get("intervals"):
            check_distribution(errors, f"{where}: arrival interval", arrival.get("interval_us"))
    resources = run.get("resources")
    if not isinstance(resources, dict) or "process_counts" not in resources:
        errors.append(f"{where}: missing resources")
    return errors


def validate_bench(document, where):
    if document.get("schema") != BENCH_SCHEMA:
        return [f"{where}: schema {document.get('schema')!r}, expected {BENCH_SCHEMA}"]
    errors = []
    host = document.get("host")
    if not isinstance(host, dict) or not host.get("kernel") or not isinstance(host.get("render_nodes"), list):
        errors.append(f"{where}: host must identify the kernel and render nodes")
    if not isinstance((document.get("tool") or {}).get("version"), str):
        errors.append(f"{where}: missing tool version")
    capabilities = document.get("capabilities")
    if capabilities is not None and not isinstance(capabilities.get("profiles"), list):
        errors.append(f"{where}: capabilities without profiles")
    runs = document.get("runs")
    if not isinstance(runs, list):
        return errors + [f"{where}: runs must be a list"]
    for index, run in enumerate(runs):
        errors += validate_run(run, f"{where} run {index}")
    return errors


def load(path):
    """(kind, document, directory) for a bench document, manifest or baseline directory."""
    path = Path(path)
    if path.is_dir():
        path = path / "manifest.json"
    document = json.loads(path.read_text())
    if document.get("schema") == BENCH_SCHEMA:
        return "bench", document, path.parent
    if "runs" in document and "checkout" in document:
        return "baseline", document, path.parent
    raise ValueError(f"{path}: neither a bench document nor a baseline manifest")


def validate_baseline(manifest, directory, where):
    errors = []
    if manifest.get("schema") not in (None, BASELINE_SCHEMA):
        errors.append(f"{where}: schema {manifest.get('schema')!r}, expected {BASELINE_SCHEMA}")
    checkout = manifest.get("checkout") or {}
    if not checkout.get("sha"):
        errors.append(f"{where}: checkout has no commit")
    if not (manifest.get("host") or {}).get("kernel"):
        errors.append(f"{where}: host has no kernel")
    for run in manifest.get("runs", []):
        name = f"{where} {run.get('workload')} #{run.get('repeat', 1)}"
        if "exit_code" not in run:
            errors.append(f"{name}: no exit code")
        if run.get("bench_json"):
            errors += validate_bench(json.loads((directory / run["bench_json"]).read_text()), name)
    return errors


def runs_by_workload(path):
    """Workload name -> bench run records (repeats)."""
    kind, document, directory = load(path)
    grouped = {}
    if kind == "bench":
        for run in document["runs"]:
            workload = run["workload"]
            key = f"{workload['codec']}-{workload['resolution']}@{workload['fps']}"
            grouped.setdefault(key, []).append(run)
        return grouped
    for run in document.get("runs", []):
        if run.get("bench_json"):
            bench = json.loads((directory / run["bench_json"]).read_text())
            grouped.setdefault(run["workload"], []).extend(bench["runs"])
    return grouped


def medians(runs):
    """Median of each metric over the runs that measured it (status ok)."""
    measured = [run for run in runs if run.get("status") == "ok"]
    result = {"runs": len(runs), "ok": len(measured), "status": sorted({run.get("status") for run in runs})}
    for name, (extract, _) in METRICS.items():
        values = [value for value in map(extract, measured) if isinstance(value, (int, float))]
        result[name] = statistics.median(values) if values else None
    return result


def compare(base, new, fail_on):
    """Rows of (workload, metric, base, new, percent change, verdict)."""
    rows = []
    regressions = []
    for workload in sorted(set(base) | set(new)):
        if workload not in base or workload not in new:
            rows.append((workload, "-", None, None, None, "only in " + ("new" if workload in new else "base")))
            continue
        b, n = medians(base[workload]), medians(new[workload])
        if not b["ok"] or not n["ok"]:
            rows.append((workload, "status", b["status"], n["status"], None, "not comparable"))
            continue
        for metric, (_, direction) in METRICS.items():
            old, current = b[metric], n[metric]
            if old is None or current is None:
                continue
            change = None if old == 0 else (current - old) / abs(old) * 100
            verdict = ""
            if direction and change is not None and change != 0:
                verdict = "better" if change * direction > 0 else "worse"
                limit = fail_on.get(metric)
                if verdict == "worse" and limit is not None and abs(change) > limit:
                    verdict = f"REGRESSION (> {limit}%)"
                    regressions.append((workload, metric, change))
            rows.append((workload, metric, old, current, change, verdict))
    return rows, regressions


def parse_fail_on(values):
    limits = {}
    for value in values or []:
        metric, _, percent = value.partition("=")
        if metric not in METRICS or METRICS[metric][1] == 0:
            raise SystemExit(f"--fail-on: {metric!r} is not a directional metric ({', '.join(m for m, (_, d) in METRICS.items() if d)})")
        limits[metric] = float(percent)
    return limits


def number(value):
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:.2f}"
    return str(value)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    validate = commands.add_parser("validate")
    validate.add_argument("paths", nargs="+", type=Path)
    summarize = commands.add_parser("summarize")
    summarize.add_argument("path", type=Path)
    summarize.add_argument("--json", type=Path)
    comparison = commands.add_parser("compare")
    comparison.add_argument("base", type=Path)
    comparison.add_argument("new", type=Path)
    comparison.add_argument("--fail-on", action="append", metavar="METRIC=PERCENT")
    comparison.add_argument("--json", type=Path)
    args = parser.parse_args()

    if args.command == "validate":
        errors = []
        for path in args.paths:
            kind, document, directory = load(path)
            errors += validate_bench(document, str(path)) if kind == "bench" else validate_baseline(document, directory, str(path))
        for error in errors:
            print(error)
        print(f"{len(args.paths)} file(s), {len(errors)} error(s)")
        return 1 if errors else 0

    if args.command == "summarize":
        table = {workload: medians(runs) for workload, runs in runs_by_workload(args.path).items()}
        if args.json:
            args.json.write_text(json.dumps(table, indent=2) + "\n")
        columns = ("fps", "total_p50_us", "total_p99_us", "arrival_interval_p99_us", "cpu_percent", "gpu_engine_ms_per_s")
        print(f"{'workload':<34} {'ok':>5} " + " ".join(f"{column:>24}" for column in columns))
        for workload, row in table.items():
            print(f"{workload:<34} {row['ok']:>2}/{row['runs']:<2} " + " ".join(f"{number(row[column]):>24}" for column in columns)
                  + ("" if row["ok"] else f"  {','.join(map(str, row['status']))}"))
        return 0

    limits = parse_fail_on(args.fail_on)
    rows, regressions = compare(runs_by_workload(args.base), runs_by_workload(args.new), limits)
    for workload, metric, old, current, change, verdict in rows:
        delta = "-" if change is None else f"{change:+.1f}%"
        print(f"{workload:<34} {metric:<24} {number(old):>12} {number(current):>12} {delta:>9}  {verdict}")
    if args.json:
        args.json.write_text(json.dumps(
            [dict(zip(("workload", "metric", "base", "new", "change_percent", "verdict"), row)) for row in rows], indent=2
        ) + "\n")
    print(f"{len(regressions)} regression(s) beyond --fail-on limits")
    return 1 if regressions else 0


if __name__ == "__main__":
    sys.exit(main())
