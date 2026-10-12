#!/usr/bin/env bash
# Regenerate vendor/pixelforge: upstream Pixelforge at a pinned revision plus
# the numbered patches in vendor/pixelforge-patches/, applied in order.
#
#   scripts/vendor-pixelforge.sh             # regenerate at the pinned revision
#   scripts/vendor-pixelforge.sh v0.10.0     # move to another upstream ref
#   scripts/vendor-pixelforge.sh --check     # verify the tree matches (no changes)
#   scripts/vendor-pixelforge.sh --test      # run the vendored crate's unit tests
#
# Never edit vendor/pixelforge by hand: add or update a patch, then rerun this
# script. See vendor/pixelforge/LOCAL_CHANGES.md.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
upstream="https://github.com/hgaiser/pixelforge"
vendor="${root}/vendor/pixelforge"
patches="${root}/vendor/pixelforge-patches"
base_file="${patches}/UPSTREAM"
# Files of the crate that the build uses; examples, tests data and CI are not vendored.
files=(Cargo.toml LICENSE README.md CHANGELOG.md src shader)

check=false
ref=""
case "${1:-}" in
  --test)
    # The crate is a [patch] path inside this workspace, so Cargo refuses to
    # test it in place; test a temporary copy instead.
    copy="$(mktemp -d)"
    trap 'rm -rf "${copy}"' EXIT
    cp -r "${vendor}/." "${copy}/"
    printf '\n[workspace]\n' >> "${copy}/Cargo.toml"
    cargo test --manifest-path "${copy}/Cargo.toml" --target-dir "${root}/target/vendor-pixelforge" --lib
    exit 0
    ;;
  --check) check=true ;;
  "") ;;
  *) ref="$1" ;;
esac
pinned="$(grep -E '^commit ' "${base_file}" | cut -d' ' -f2)"
[[ -n "${ref}" ]] || ref="${pinned}"

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT
git clone --quiet "${upstream}" "${work}/src"
git -C "${work}/src" checkout --quiet "${ref}"
commit="$(git -C "${work}/src" rev-parse HEAD)"
describe="$(git -C "${work}/src" describe --tags --always)"

for patch in "${patches}"/[0-9][0-9][0-9][0-9]-*.patch; do
  [[ -e "${patch}" ]] || continue
  if ! git -C "${work}/src" apply --check "${patch}" 2>"${work}/apply.log"; then
    echo "error: $(basename "${patch}") no longer applies to ${describe} (${commit}):" >&2
    cat "${work}/apply.log" >&2
    echo "Rebase the patch onto the new upstream, or drop it if upstream fixed the issue." >&2
    exit 1
  fi
  git -C "${work}/src" apply "${patch}"
done

mkdir -p "${work}/out"
for file in "${files[@]}"; do
  cp -r "${work}/src/${file}" "${work}/out/"
done

if "${check}"; then
  if [[ "${commit}" != "${pinned}" ]]; then
    echo "error: ${base_file} pins ${pinned}, but resolved ${commit}" >&2
    exit 1
  fi
  if ! diff -r "${work}/out" "${vendor}" --exclude=LOCAL_CHANGES.md; then
    echo "error: vendor/pixelforge differs from upstream ${describe} plus the patches." >&2
    echo "Move local edits into a patch in vendor/pixelforge-patches/ and rerun this script." >&2
    exit 1
  fi
  echo "vendor/pixelforge matches upstream ${describe} (${commit}) plus $(ls "${patches}"/[0-9]*.patch | wc -l) patch(es)."
  exit 0
fi

# Replace everything except the maintained notes.
find "${vendor}" -mindepth 1 -maxdepth 1 ! -name LOCAL_CHANGES.md -exec rm -rf {} +
cp -r "${work}/out/." "${vendor}/"
printf 'commit %s\ndescribe %s\n' "${commit}" "${describe}" > "${base_file}"
echo "vendor/pixelforge regenerated from ${describe} (${commit})."
echo "If the version changed, update the git tag in moonshine-core/Cargo.toml and run cargo update -p pixelforge."
