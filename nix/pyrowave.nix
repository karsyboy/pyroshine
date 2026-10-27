{
  lib,
  stdenv,
  cmake,
  pkg-config,
  vulkan-loader,
}:

let
  # Generated from upstream plus vendor/pyrowave-patches/ (see
  # vendor/README.md); scripts/check-vendor-pins.py keeps this revision equal
  # to vendor/pyrowave-patches/UPSTREAM.
  upstreamUrl = "https://github.com/Themaister/pyrowave";
  upstreamRevision = "7fe0e2667cd3076b8b2bf04bcb80bc8d691b800b";
  graniteRevision = "53090429539bf3404250d18fe58db971f0907c04";
  volkRevision = "47cddf7ed97b94118a08aacb548a411188e016cc";
  vulkanHeadersRevision = "6802bb4733b63ed5efd3adb308a6c885ef180ea1";

  granite = builtins.fetchGit {
    url = "https://github.com/Themaister/Granite";
    rev = graniteRevision;
  };
  volk = builtins.fetchGit {
    url = "https://github.com/zeux/volk";
    rev = volkRevision;
  };
  vulkanHeaders = builtins.fetchGit {
    url = "https://github.com/KhronosGroup/Vulkan-Headers";
    rev = vulkanHeadersRevision;
  };
in
stdenv.mkDerivation {
  pname = "pyrowave-pyroshine";
  version = "1.1.0-${builtins.substring 0 8 upstreamRevision}";
  src = ../vendor/pyrowave;

  # The Granite patches checkout_granite.sh applies, in its order; Nix
  # provides Granite itself (postUnpack) instead of running the script.
  patches = map (name: ../vendor/pyrowave/patches + "/${name}") [
    "granite-sdr-normalization.patch"
    "granite-scaler-overlay.patch"
    "granite-sdr-reference-white-203.patch"
  ];

  nativeBuildInputs = [
    cmake
    pkg-config
  ];
  buildInputs = [ vulkan-loader ];

  postUnpack = ''
    cp -r ${granite} $sourceRoot/Granite
    chmod -R u+w $sourceRoot/Granite
    mkdir -p $sourceRoot/Granite/third_party/khronos
    rm -rf $sourceRoot/Granite/third_party/volk
    rm -rf $sourceRoot/Granite/third_party/khronos/vulkan-headers
    cp -r ${volk} $sourceRoot/Granite/third_party/volk
    cp -r ${vulkanHeaders} $sourceRoot/Granite/third_party/khronos/vulkan-headers
  '';

  cmakeFlags = [
    "-DPYROWAVE_DEVEL=OFF"
    "-DPYROWAVE_UTILS=OFF"
    "-DBUILD_TESTING=OFF"
  ];

  # Runtime tests need a Vulkan GPU; the CPU-only packet validation runs in
  # scripts/build-pyrowave.sh, and Pyroshine checks the ABI at startup.
  doCheck = false;

  meta = {
    description = "Pinned PyroWave build for Pyroshine";
    homepage = upstreamUrl;
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
