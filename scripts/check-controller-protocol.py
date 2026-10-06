#!/usr/bin/env python3
"""Check host capability assignments against the matching common-c header."""
import argparse
from pathlib import Path
import re


def check(header: Path) -> None:
    wire = header.read_text()
    host = (Path(__file__).resolve().parent.parent /
            "moonshine-core/src/session/stream/control/input/gamepad.rs").read_text()
    names = {
        "DualSenseEdge": "DUALSENSE_EDGE",
        "XboxElite": "XBOX_ELITE",
        "XboxEliteSeries2": "XBOX_ELITE_SERIES_2",
        "SteamController": "STEAM_CONTROLLER",
        "SteamDeck": "STEAM_DECK",
    }
    for model, capability in names.items():
        h = re.search(rf"\b{model}\s*=\s*(0x[0-9a-fA-F]+)", host)
        w = re.search(rf"#define\s+LI_CCAP_{capability}\s+(0x[0-9a-fA-F]+)\b", wire)
        if h is None or w is None or int(h[1], 16) != int(w[1], 16):
            raise SystemExit(f"Controller capability mismatch: {model} / LI_CCAP_{capability}")
    print("Controller capability assignments match common-c")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("header", type=Path, help="Matching common-c src/Limelight.h")
    check(parser.parse_args().header)
