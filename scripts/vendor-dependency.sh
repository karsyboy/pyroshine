#!/usr/bin/env bash
# Shared implementation for vendor-inputtino.sh and vendor-pixelforge.sh.
# See vendor/README.md. Do not invoke this helper directly.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dependency="${1:?missing dependency}"
shift
case "${dependency}" in
  pixelforge)
    upstream="https://github.com/hgaiser/pixelforge"
    files=(Cargo.toml LICENSE README.md CHANGELOG.md src shader)
    ;;
  inputtino)
    upstream="https://github.com/games-on-whales/inputtino"
    files=(CMakeLists.txt LICENSE bindings cmake include share src tests)
    ;;
  *) echo "error: unknown dependency ${dependency}" >&2; exit 1 ;;
esac
vendor="${root}/vendor/${dependency}"
patches="${root}/vendor/${dependency}-patches"
base_file="${patches}/UPSTREAM"
mode=regenerate
ref=""
source="${upstream}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --check|--test)
      [[ "${mode}" == regenerate && -z "${ref}" ]] || { echo "error: choose one operation" >&2; exit 1; }
      mode="${1#--}"
      ;;
    --source)
      [[ $# -ge 2 && -d "$2" ]] || { echo "error: --source requires a local Git repository" >&2; exit 1; }
      source="$2"
      shift
      ;;
    -*) echo "error: unknown option $1" >&2; exit 1 ;;
    *)
      [[ "${mode}" == regenerate && -z "${ref}" ]] || { echo "error: choose one operation" >&2; exit 1; }
      ref="$1"
      ;;
  esac
  shift
done

if [[ "${mode}" == test ]]; then
  if [[ "${dependency}" == pixelforge ]]; then
    copy="$(mktemp -d)"
    trap 'rm -rf "${copy}"' EXIT
    cp -a "${vendor}/." "${copy}/"
    printf '\n[workspace]\n' >> "${copy}/Cargo.toml"
    cargo test --manifest-path "${copy}/Cargo.toml" \
      --target-dir "${CARGO_TARGET_DIR:-${root}/target}/vendor-pixelforge" --lib
  else
    build="${CARGO_TARGET_DIR:-${root}/target}/vendor-inputtino"
    sanitize="-fsanitize=address,undefined -fno-omit-frame-pointer"
    cmake -S "${vendor}" -B "${build}" -DCMAKE_BUILD_TYPE=Debug \
      -DBUILD_TESTING=OFF -DINPUTTINO_EDGE_TESTS=ON -DINPUTTINO_ELITE_TESTS=ON \
      -DINPUTTINO_PS5_FEATURE_TESTS=ON "-DCMAKE_CXX_FLAGS=${sanitize}" \
      -DCMAKE_EXE_LINKER_FLAGS=-fsanitize=address,undefined \
      -DCMAKE_SHARED_LINKER_FLAGS=-fsanitize=address,undefined
    cmake --build "${build}" --parallel
    ASAN_OPTIONS=detect_leaks=0 UBSAN_OPTIONS=halt_on_error=1:print_stacktrace=1 \
      ctest --test-dir "${build}" --output-on-failure
  fi
  exit 0
fi

pinned="$(sed -n 's/^commit //p' "${base_file}")"
[[ "${pinned}" =~ ^[0-9a-f]{40}$ ]] || { echo "error: invalid commit in ${base_file}" >&2; exit 1; }
[[ -n "${ref}" ]] || ref="${pinned}"
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT
git clone --quiet --no-hardlinks "${source}" "${work}/src"
git -C "${work}/src" checkout --quiet --detach "${ref}"
commit="$(git -C "${work}/src" rev-parse HEAD)"
describe="$(git -C "${work}/src" describe --tags --always)"

count=0
for patch in "${patches}"/[0-9][0-9][0-9][0-9]-*.patch; do
  [[ -e "${patch}" ]] || continue
  if ! git -C "${work}/src" apply --check "${patch}" 2>"${work}/apply.log"; then
    echo "error: $(basename "${patch}") no longer applies to ${describe} (${commit}):" >&2
    cat "${work}/apply.log" >&2
    echo "Rebase the patch onto the new upstream, or drop it if upstream fixed the issue." >&2
    exit 1
  fi
  git -C "${work}/src" apply "${patch}"
  count=$((count + 1))
done
mkdir "${work}/out"
for file in "${files[@]}"; do
  cp -a "${work}/src/${file}" "${work}/out/"
done

if [[ "${mode}" == check ]]; then
  cp -a "${vendor}/LOCAL_CHANGES.md" "${work}/out/"
  if ! git diff --no-index --exit-code -- "${work}/out" "${vendor}"; then
    echo "error: vendor/${dependency} differs from upstream ${describe} plus the patches." >&2
    echo "Move local edits into vendor/${dependency}-patches/ and regenerate." >&2
    exit 1
  fi
  if [[ "${dependency}" == pixelforge ]]; then
    tag="$(sed -n 's/^describe //p' "${base_file}")"
    [[ "$(git -C "${work}/src" rev-parse "${tag}^{commit}")" == "${commit}" ]] || {
      echo "error: Pixelforge tag does not resolve to the pinned commit" >&2; exit 1;
    }
  fi
  python3 "${root}/scripts/check-vendor-pins.py" "${dependency}"
  echo "vendor/${dependency} matches ${commit} plus ${count} patch(es); Cargo pins agree."
  exit 0
fi

# Finish reconstruction before replacing files; preserve the maintained notes.
mkdir -p "${vendor}"
find "${vendor}" -mindepth 1 -maxdepth 1 ! -name LOCAL_CHANGES.md -exec rm -rf {} +
cp -a "${work}/out/." "${vendor}/"
printf 'commit %s\ndescribe %s\n' "${commit}" "${describe}" > "${base_file}"
echo "vendor/${dependency} regenerated from ${describe} (${commit}), ${count} patch(es)."
echo "When changing the base, update Cargo pins and LOCAL_CHANGES.md, then run --check and --test."
