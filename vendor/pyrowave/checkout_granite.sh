#!/bin/bash

# Only checks out what is necessary to build standalone.
#
GRANITE_COMMIT=53090429539bf3404250d18fe58db971f0907c04

if [ -d Granite ]; then
	cd Granite
	git fetch origin
	git checkout $GRANITE_COMMIT
else
	git clone https://github.com/Themaister/Granite
	cd Granite
	git checkout $GRANITE_COMMIT
fi

cd ..

update() {
	git submodule sync $1
	git submodule update --init $1
}

cd Granite
update third_party/volk
update third_party/khronos/vulkan-headers

# Maintained Granite changes, applied in order (later patches build on
# earlier ones): HDR-to-SDR normalization, 1:1 scaler overlays, then the
# BT.2408 reference white. shaders/slangmosh_scaler.hpp is generated from the
# patched scaler; the last patch changes only host code.
#
# Every run starts from the pinned files: a later patch edits lines an earlier
# one added, so an already patched tree can neither take nor reverse them.
# The patches are applied inside Granite's own checkout (-p2 strips
# a/Granite/): run from this directory, git apply would resolve their paths
# against any work tree this directory is nested in and skip the files.
git checkout -- .
for name in granite-sdr-normalization.patch granite-scaler-overlay.patch granite-sdr-reference-white-203.patch; do
	git apply -p2 "../patches/$name" || exit 1
done
