#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SPV_PATH="$SCRIPT_DIR/color_convert.spv"

if ! command -v glslc &>/dev/null; then
    echo "Error: glslc not found. Install the Vulkan SDK or glslc package." >&2
    exit 1
fi

echo "Compiling color_convert.comp → color_convert.spv"
glslc -x glsl --target-env=vulkan1.3 -O \
    "$SCRIPT_DIR/color_convert.comp" \
    -o "$SPV_PATH"

FILE_SIZE=$(stat -c%s "$SPV_PATH" 2>/dev/null || stat -f%z "$SPV_PATH" 2>/dev/null)
echo "Written $FILE_SIZE bytes to $SPV_PATH"
