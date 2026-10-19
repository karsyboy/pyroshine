#!/usr/bin/env python3
"""Regenerate Pyroshine's icons (web page, tray, desktop entry) from assets/logo-no-text.png.

Requires Pillow. Run from the repository root:

    python3 web/scripts/generate-icons.py
"""

from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "assets" / "logo-no-text.png"
TRAY_ICON = ROOT / "src" / "tray" / "tray.png"
HICOLOR = ROOT / "dist" / "icons" / "hicolor"
PUBLIC = ROOT / "web" / "public"

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


if __name__ == "__main__":
    main()
