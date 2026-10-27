"""Tests for the benchmark result, baseline and hardware-test tooling."""

import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import bench_results  # noqa: E402
import hardware_tests  # noqa: E402
import record_baseline  # noqa: E402


def distribution(p50, p99=None, low=None, high=None):
    p99 = p99 if p99 is not None else p50
    low = low if low is not None else p50
    high = high if high is not None else p99
    return {"avg": float(p50), "min": low, "p50": p50, "p95": p99, "p99": p99, "max": high}


def run(total_p99=2000, fps=60.0, status="ok", codec="hevc"):
    """A run record shaped like `moonshine-bench --json` output."""
    record = {
        "workload": {"codec": codec, "resolution": "1920x1080", "fps": 60},
        "status": status,
        "stats_lagged": 0,
    }
    if status == "ok":
        record["summary"] = {
            "frames": 600,
            "fps": fps,
            "encoded_mbps": 14.0,
            "latency_us": {
                **{name: distribution(100) for name in bench_results.DISTRIBUTIONS},
                "total": distribution(1500, total_p99, 1200, total_p99 + 100),
            },
            "breakdown_avg_us": {"convert": 300.0},
            "capture": {"direct": 590, "direct_late": 0, "composited": 10, "composition_reasons": {"cursor": 10}},
        }
        record["pacing"] = {
            "delivered_frames": 600,
            "delivery_ratio": 1.0,
            "arrival": {"intervals": 599, "interval_us": distribution(16666, 17200, 16000, 17500),
                        "stddev_us": 300.0, "missed_frame_slots": 0},
        }
        record["resources"] = {
            "process": {"cpu_percent_of_one_core": 3.0, "gpu_engine_busy_ms_per_s": {"enc": 70.0, "compute": 10.0}},
            "process_counts": {"peak_rss_kib": 90000, "dmabuf_fds": 10},
        }
    elif status == "unsupported":
        record["reason"] = "the capture GPU does not encode AV1"
    elif status == "failed":
        record["error"] = "Failed to launch session"
    return record


def document(*runs):
    return {
        "schema": bench_results.BENCH_SCHEMA,
        "tool": {"name": "moonshine-bench", "version": "0.18.0"},
        "host": {"kernel": "7.2.10", "render_nodes": [{"node": "/dev/dri/renderD128", "driver": "amdgpu"}]},
        "capabilities": {"profiles": ["h264", "hevc"]},
        "runs": list(runs),
    }


class ValidateTest(unittest.TestCase):
    def test_well_formed_document_passes(self):
        doc = document(run(), run(status="unsupported", codec="av1"), run(status="failed"))
        self.assertEqual(bench_results.validate_bench(doc, "doc"), [])

    def test_inconsistent_documents_are_reported(self):
        broken = run()
        broken["summary"]["latency_us"]["total"]["p95"] = 99999
        broken["summary"]["capture"]["direct"] = 1
        broken["pacing"]["delivered_frames"] = 5
        unsupported = run(status="unsupported")
        del unsupported["reason"]
        errors = bench_results.validate_bench(document(broken, unsupported), "doc")
        text = "\n".join(errors)
        self.assertIn("percentiles are not ordered", text)
        self.assertIn("capture paths sum", text)
        self.assertIn("pacing delivered", text)
        self.assertIn("unsupported without a reason", text)

    def test_schema_and_host_are_required(self):
        doc = document(run())
        doc["schema"] = "pyroshine-bench/0"
        self.assertTrue(bench_results.validate_bench(doc, "doc"))
        doc = document(run())
        doc["host"] = {}
        self.assertTrue(any("host" in error for error in bench_results.validate_bench(doc, "doc")))

    def test_baseline_manifest_validates_its_bench_documents(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            bad = run()
            bad["summary"]["frames"] = 0
            (directory / "a.1.json").write_text(json.dumps(document(bad)))
            manifest = {
                "schema": bench_results.BASELINE_SCHEMA,
                "checkout": {"sha": "abc"},
                "host": {"kernel": "7.2"},
                "runs": [{"workload": "a", "repeat": 1, "exit_code": 0, "bench_json": "a.1.json"}],
            }
            (directory / "manifest.json").write_text(json.dumps(manifest))
            kind, loaded, base = bench_results.load(directory)
            self.assertEqual(kind, "baseline")
            self.assertTrue(bench_results.validate_baseline(loaded, base, "baseline"))


class CompareTest(unittest.TestCase):
    def grouped(self, *runs):
        return {"hevc-1080p60": list(runs)}

    def test_medians_use_only_measured_runs(self):
        medians = bench_results.medians([run(total_p99=1000), run(total_p99=3000), run(total_p99=2000),
                                         run(status="failed")])
        self.assertEqual(medians["ok"], 3)
        self.assertEqual(medians["total_p99_us"], 2000)
        self.assertEqual(medians["gpu_engine_ms_per_s"], 80.0)

    def test_reports_without_thresholds(self):
        rows, regressions = bench_results.compare(self.grouped(run(2000)), self.grouped(run(4000)), {})
        self.assertEqual(regressions, [])
        verdicts = {metric: verdict for _, metric, _, _, _, verdict in rows}
        self.assertEqual(verdicts["total_p99_us"], "worse")

    def test_threshold_turns_worse_change_into_regression(self):
        limits = bench_results.parse_fail_on(["total_p99_us=10", "fps=5"])
        rows, regressions = bench_results.compare(
            self.grouped(run(2000, fps=60.0)), self.grouped(run(2100, fps=50.0)), limits
        )
        self.assertEqual([(metric) for _, metric, _ in regressions], ["fps"])
        _, faster = bench_results.compare(self.grouped(run(2000)), self.grouped(run(1000)), limits)
        self.assertEqual(faster, [], "an improvement is never a regression")

    def test_informational_metrics_cannot_gate(self):
        with self.assertRaises(SystemExit):
            bench_results.parse_fail_on(["encoded_mbps=5"])

    def test_unmeasured_workloads_are_not_compared(self):
        rows, _ = bench_results.compare(self.grouped(run(status="unsupported")), self.grouped(run()), {})
        self.assertEqual(rows[0][-1], "not comparable")


class BaselineTest(unittest.TestCase):
    def test_workload_names_are_unique_and_stable(self):
        workloads = record_baseline.workload_set("all")
        names = [workload[0] for workload in workloads]
        self.assertEqual(len(names), len(set(names)))
        # The original set is unchanged and contained in `all`.
        self.assertIn(("hevc-1080p60", "hevc", "1920x1080", 60, []), workloads)

    def test_phase0_covers_every_codec_resolution_and_rate(self):
        covered = {(codec, resolution, fps) for _, codec, resolution, fps, _ in record_baseline.phase0_workloads()}
        for codec in ("h264", "hevc", "av1", "pyrowave"):
            for resolution in ("1920x1080", "2560x1440", "3840x2160"):
                for fps in (60, 120):
                    self.assertIn((codec, resolution, fps), covered)

    def test_cycle_codecs_follow_capabilities(self):
        capabilities = {"profiles": ["h264", "hevc", "hevc_main10", "pyrowave"]}
        self.assertEqual(record_baseline.cycle_codecs(capabilities, True), ["h264", "hevc", "pyrowave"])
        self.assertEqual(record_baseline.cycle_codecs(capabilities, False), ["h264", "hevc"])

    def test_nvidia_samples_skip_warmup(self):
        text = "\n".join([
            "2026/10/15 10:00:00.000, 0, 00000000:01:00.0, 99, 99, 900, 300.0, 1900",
            "2026/10/15 10:00:05.000, 0, 00000000:01:00.0, 20, 10, 900, 120.5, 1900",
            "2026/10/15 10:00:05.500, 0, 00000000:01:00.0, 30, 20, 900, 130.5, 1900",
        ])
        summary = record_baseline.summarize_nvidia(text, skip_seconds=2)
        device = summary["00000000:01:00.0"]
        self.assertEqual(device["gpu_percent"]["avg"], 25.0)
        self.assertEqual(device["encoder_percent"]["max"], 20.0)
        self.assertEqual(device["gpu_percent"]["samples"], 2)


class HardwareTestsTest(unittest.TestCase):
    def test_registry_is_well_formed(self):
        entries = hardware_tests.load_registry()
        self.assertTrue(entries)
        self.assertTrue(all(entry["kind"] in hardware_tests.KINDS for entry in entries))

    def test_registry_differences(self):
        entries = [{"crate": "moonshine-core", "name": "a::b"}, {"crate": "moonshine-core", "name": "gone"}]
        missing, stale = hardware_tests.registry_differences(entries, {"moonshine-core": {"a::b", "new::test"}})
        self.assertEqual(missing, [("moonshine-core", "new::test")])
        self.assertEqual(stale, [("moonshine-core", "gone")])

    def test_list_output_is_parsed(self):
        output = "a::b: test\nc::d: test\n\n2 tests, 0 benchmarks\n"
        self.assertEqual(hardware_tests.parse_test_list(output), {"a::b", "c::d"})

    def test_only_one_passed_test_counts_as_passed(self):
        self.assertTrue(hardware_tests.passed(0, "test result: ok. 1 passed; 0 failed; 0 ignored"))
        self.assertFalse(hardware_tests.passed(0, "test result: ok. 0 passed; 0 failed; 1 filtered out"))
        self.assertFalse(hardware_tests.passed(101, "test result: FAILED. 0 passed; 1 failed"))

    def test_missing_requirements_skip_with_reason(self):
        entries = [
            {"name": "gpu", "kind": "test", "requires": ["render-node"], "env": {}},
            {"name": "helper", "kind": "helper", "requires": [], "env": {}},
            {"name": "timing", "kind": "informational", "requires": [], "env": {}},
        ]
        found = {name: (False, "absent") for name in hardware_tests.REQUIREMENTS}
        planned = dict((entry["name"], reason) for entry, reason in hardware_tests.plan(entries, found))
        self.assertIn("render-node", planned["gpu"])
        self.assertIn("helper", planned["helper"])
        self.assertIn("informational", planned["timing"])
        available = copy.deepcopy(found)
        available["render-node"] = (True, "/dev/dri/renderD128")
        planned = dict((entry["name"], reason) for entry, reason in hardware_tests.plan(entries, available, True))
        self.assertIsNone(planned["gpu"])
        self.assertIsNone(planned["timing"])


if __name__ == "__main__":
    unittest.main()
