#!/usr/bin/env bash
set -euo pipefail

# Reproducible build of Pyroshine's PyroWave dependency: the generated
# vendor/pyrowave tree (upstream plus vendor/pyrowave-patches/, see
# vendor/README.md) and the Granite revisions its checkout script pins.
GRANITE_REVISION="53090429539bf3404250d18fe58db971f0907c04"
VOLK_REVISION="47cddf7ed97b94118a08aacb548a411188e016cc"
VULKAN_HEADERS_REVISION="6802bb4733b63ed5efd3adb308a6c885ef180ea1"

workdir="${1:?usage: build-pyrowave.sh WORKDIR PREFIX}"
prefix="${2:?usage: build-pyrowave.sh WORKDIR PREFIX}"
vendor="$(cd "$(dirname "${BASH_SOURCE[0]}")/../vendor/pyrowave" && pwd)"
src="${workdir}/src"

# Copy the vendored sources fresh on every run, keeping only the Granite
# clone, which is reset to its pinned files below.
mkdir -p "${src}"
find "${src}" -mindepth 1 -maxdepth 1 ! -name Granite -exec rm -rf {} +
cp -a "${vendor}/." "${src}/"

# A reused work directory keeps the previous run's patched Granite files, on
# which the Granite patches may neither apply nor reverse once one changed.
if [[ -d "${src}/Granite/.git" ]]; then
  git -C "${src}/Granite" checkout -- .
fi

# The vendored helper checks out only Granite plus the two submodules used by
# the standalone library and applies the maintained Granite patches in
# patches/. Verify every transitive revision after it runs.
(cd "${src}" && ./checkout_granite.sh)
[[ "$(git -C "${src}/Granite" rev-parse HEAD)" == "${GRANITE_REVISION}" ]]
[[ "$(git -C "${src}/Granite/third_party/volk" rev-parse HEAD)" == "${VOLK_REVISION}" ]]
[[ "$(git -C "${src}/Granite/third_party/khronos/vulkan-headers" rev-parse HEAD)" == "${VULKAN_HEADERS_REVISION}" ]]

cmake -S "${src}" -B "${workdir}/build" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX="${prefix}" \
  -DPYROWAVE_DEVEL=OFF \
  -DPYROWAVE_UTILS=OFF \
  -DBUILD_TESTING=OFF
cmake --build "${workdir}/build" --parallel
ctest --test-dir "${workdir}/build" --output-on-failure -R '^packet-validation$'
cmake --install "${workdir}/build"

test -e "${prefix}/lib/libpyrowave-shared.so.1"
