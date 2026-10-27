#!/usr/bin/env python3
"""Compare each independent GPU decoder with its encoder's self-decode.

The default is the deterministic 256x144 fixture. --matrix exercises frame
budgets at streaming resolutions/rates; it does not simulate a live stream.
"""
import argparse
import hashlib
import json
import shutil
import subprocess
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("harness", type=Path)
    parser.add_argument("native_library", type=Path)
    parser.add_argument("reference_library", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--matrix", action="store_true")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    dimensions = [(1920, 1080), (2560, 1440), (3840, 2160)] if args.matrix else [(256, 144)]
    rates = [50000, 100000, 200000, 400000, 600000, 800000] if args.matrix else [0]
    fps_values = [60, 120] if args.matrix else [0]
    results = []
    for width, height in dimensions:
        for rate in rates:
            for fps in fps_values:
                scenario = args.output / f"{width}x{height}-{rate}kbps-{fps}fps"
                scenario.mkdir(exist_ok=True)
                extra = [str(width), str(height), str(rate), str(fps)] if rate else []
                for encoder, decoder, name, phase in [
                    (args.native_library, args.native_library, "native-self", "encode"),
                    (args.native_library, args.reference_library, "native-to-nonary", "decode"),
                    (args.reference_library, args.reference_library, "nonary-self", "encode"),
                    (args.reference_library, args.native_library, "nonary-to-native", "decode"),
                ]:
                    directory = scenario / name
                    directory.mkdir(exist_ok=True)
                    if phase == "decode":
                        source = scenario / ("native-self" if name.startswith("native") else "nonary-self")
                        for fixture in source.glob("*.bin"):
                            shutil.copyfile(fixture, directory / fixture.name)
                    with (directory / "gpu.log").open("w") as log:
                        subprocess.run([str(args.harness.resolve()), str(encoder.resolve()),
                                        str(decoder.resolve()), str(directory.resolve()), phase] + extra,
                                       stdout=log, stderr=subprocess.STDOUT, check=True)
                for direction, baseline in [("native-to-nonary", "native-self"),
                                            ("nonary-to-native", "nonary-self")]:
                    for mode in ["sdr420", "sdr444", "hdr420", "hdr444"]:
                        actual = (scenario / direction / f"{mode}.decoded").read_bytes()
                        expected = (scenario / baseline / f"{mode}.decoded").read_bytes()
                        if actual != expected:
                            raise RuntimeError(f"GPU decoder mismatch: {scenario.name}/{direction}/{mode}")
                        fixture = (scenario / direction / f"{mode}.bin").read_bytes()
                        results.append(dict(width=width, height=height, bitrate_kbps=rate, fps=fps,
                                            direction=direction, mode=mode, fixture_bytes=len(fixture),
                                            fixture_sha256=hashlib.sha256(fixture).hexdigest(),
                                            decoded_sha256=hashlib.sha256(actual).hexdigest(),
                                            max_r16_sample_difference=0))
                print(f"PASS {scenario.name}: eight cross-decoder/profile comparisons", flush=True)
                # Matrix results/logs suffice; avoid accumulating multi-GB readbacks.
                if args.matrix:
                    for binary in scenario.glob("*/*"):
                        if binary.suffix in (".decoded", ".bin"):
                            binary.unlink()
                (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
