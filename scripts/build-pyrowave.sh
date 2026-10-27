#!/usr/bin/env bash
set -euo pipefail

# Reproducible build of Pyroshine's authoritative PyroWave dependency.
SOURCE_URL="https://github.com/karsyboy/pyrowave"
SOURCE_REVISION="78084ece21b1725d193442a7619660625f3f735a"
GRANITE_REVISION="53090429539bf3404250d18fe58db971f0907c04"
VOLK_REVISION="47cddf7ed97b94118a08aacb548a411188e016cc"
VULKAN_HEADERS_REVISION="6802bb4733b63ed5efd3adb308a6c885ef180ea1"

workdir="${1:?usage: build-pyrowave.sh WORKDIR PREFIX}"
prefix="${2:?usage: build-pyrowave.sh WORKDIR PREFIX}"
src="${workdir}/src"

mkdir -p "${workdir}"
if [[ ! -d "${src}/.git" ]]; then
  git clone "${SOURCE_URL}" "${src}"
fi
git -C "${src}" fetch origin "${SOURCE_REVISION}"
git -C "${src}" checkout --detach "${SOURCE_REVISION}"

# The fork's helper and the loop below patch Granite in place on every run.
# A reused work directory keeps the previous run's patched files, on which
# the fork's patches may neither apply nor reverse once a host patch changed
# them; start from the pinned Granite files each time.
if [[ -d "${src}/Granite/.git" ]]; then
  git -C "${src}/Granite" checkout -- .
fi

# This fork-owned helper checks out only Granite plus the two submodules used
# by the standalone library and applies the fork's Granite patches. Verify
# every transitive revision after it runs.
(cd "${src}" && ./checkout_granite.sh)
[[ "$(git -C "${src}" rev-parse HEAD)" == "${SOURCE_REVISION}" ]]
[[ "$(git -C "${src}/Granite" rev-parse HEAD)" == "${GRANITE_REVISION}" ]]
[[ "$(git -C "${src}/Granite/third_party/volk" rev-parse HEAD)" == "${VOLK_REVISION}" ]]
[[ "$(git -C "${src}/Granite/third_party/khronos/vulkan-headers" rev-parse HEAD)" == "${VULKAN_HEADERS_REVISION}" ]]

# Apply the audited, wire-compatible fixes to the pinned dependency.
patch_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../nix/patches" && pwd)"
# The fork already applied the shared ones; only a patch that neither applies
# nor reverses is an error, reported by the reverse check.
for patch in "$patch_dir"/*.patch; do
  if git -C "${src}" apply --check "$patch" 2>/dev/null; then
    git -C "${src}" apply "$patch"
  else
    git -C "${src}" apply --reverse --check "$patch"
  fi
done

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
