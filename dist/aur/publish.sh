#!/bin/bash
# Publish this package to the AUR for a release: run update.sh, check that the
# PKGBUILD packages, then commit PKGBUILD, .SRCINFO and install files to the
# AUR repository named after pkgbase and push. Uses your ssh configuration for
# aur@aur.archlinux.org (GIT_SSH_COMMAND overrides it); the release workflow's
# aur job runs it the same way.
#
# Usage: publish.sh <version>   (e.g. 0.17.2 or v0.17.2; stable releases only)

set -euo pipefail

[ $# -eq 1 ] || { echo "Usage: $0 <version>" >&2; exit 1; }

cd "$(dirname "$0")"
./update.sh "$1"
makepkg --force --nodeps --noarchive

pkgbase=$(sed -n 's/^pkgbase = //p' .SRCINFO)
release=$(sed -n 's/^\tpkgver = //p' .SRCINFO)-$(sed -n 's/^\tpkgrel = //p' .SRCINFO)
shopt -s nullglob
files=(PKGBUILD .SRCINFO *.install)

repo=$(mktemp -d)
trap 'rm -rf "$repo"' EXIT
git clone --quiet "ssh://aur@aur.archlinux.org/${pkgbase}.git" "$repo"
cp "${files[@]}" "$repo/"
git -C "$repo" add "${files[@]}"
if git -C "$repo" diff --cached --quiet; then
  echo "AUR ${pkgbase} is already at ${release}"
  exit 0
fi
git -C "$repo" commit --quiet -m "Update to ${release}"
git -C "$repo" push origin HEAD:master
echo "Published ${pkgbase} ${release} to the AUR"
