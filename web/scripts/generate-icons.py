#!/usr/bin/env python3
"""Regenerate Pyroshine's icons (web page, tray, desktop entry) from assets/logo-no-text.png,
and the tray menu's Material icons from @mui/icons-material (the same icons the web
interface uses).

Requires Pillow, rsvg-convert and `npm ci` in web/. Run from the repository root:

    python3 web/scripts/generate-icons.py
"""

import re
import subprocess
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "assets" / "logo-no-text.png"
TRAY_ICON = ROOT / "src" / "tray" / "tray.png"
HICOLOR = ROOT / "dist" / "icons" / "hicolor"
PUBLIC = ROOT / "web" / "public"
MATERIAL = ROOT / "web" / "node_modules" / "@mui" / "icons-material"
MENU_ICONS = ROOT / "src" / "tray" / "menu-icons"

# Tray menu entries and their Material icons (Apache License 2.0, Google;
# packaged by @mui/icons-material). White alpha masks the tray tints to the
# desktop's light or dark style.
MENU = {
    "dashboard": "SpaceDashboard",
    "pair": "AddLink",
    "pairing": "Dialpad",
    "end-session": "StopCircle",
    "history": "History",
    "updates": "NewReleases",
    "hide": "VisibilityOff",
}
MENU_SIZE = 48

# The tray embeds this and renders its status badges on it.
TRAY_SIZE = 128
# Installed for the desktop entry and notifications.
HICOLOR_SIZES = [16, 22, 24, 32, 48, 64, 128, 256, 512]


def square(image: Image.Image) -> Image.Image:
    """Crop transparent margins, then center on a square canvas with padding."""
    box = image.getbbox()
    image = image.crop(box)
    side = round(max(image.size) * 1.06)
    canvas = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    canvas.paste(image, ((side - image.width) // 2, (side - image.height) // 2), image)
    return canvas


def menu_icons() -> None:
    MENU_ICONS.mkdir(parents=True, exist_ok=True)
    for name, material in MENU.items():
        paths = re.findall(r'd: "([^"]+)"', (MATERIAL / f"{material}.js").read_text())
        if not paths:
            raise SystemExit(f"no path data in {material}.js")
        svg = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">' + "".join(
            f'<path fill="#fff" d="{d}"/>' for d in paths
        ) + "</svg>"
        png = subprocess.run(
            ["rsvg-convert", "-w", str(MENU_SIZE), "-h", str(MENU_SIZE)],
            input=svg.encode(), capture_output=True, check=True,
        ).stdout
        (MENU_ICONS / f"{name}.png").write_bytes(png)


def main() -> None:
    logo = square(Image.open(SOURCE).convert("RGBA"))
    TRAY_ICON.parent.mkdir(parents=True, exist_ok=True)
    logo.resize((TRAY_SIZE, TRAY_SIZE), Image.LANCZOS).save(TRAY_ICON, optimize=True)
    for size in HICOLOR_SIZES:
        target = HICOLOR / f"{size}x{size}" / "apps" / "pyroshine.png"
        target.parent.mkdir(parents=True, exist_ok=True)
        logo.resize((size, size), Image.LANCZOS).save(target, optimize=True)
    PUBLIC.mkdir(parents=True, exist_ok=True)
    logo.resize((256, 256), Image.LANCZOS).save(PUBLIC / "logo.png", optimize=True)
    menu_icons()


if __name__ == "__main__":
    main()
