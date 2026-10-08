#!/bin/bash
# Point this AUR package at a published release: set pkgver (resetting pkgrel
# for a new version), refresh the checksums from the release assets and
# regenerate .SRCINFO. Needs makepkg and updpkgsums (pacman-contrib) and must
# run as a non-root user.
#
# Usage: update.sh <version>   (e.g. 0.17.2 or v0.17.2; stable releases only)

set -euo pipefail

fail()
{
  echo "$1" >&2
  exit 1
}

[ $# -eq 1 ] || fail "Usage: $0 <version>"
VERSION=${1#v}
# pkgver cannot hold '-'; prereleases stay off the AUR.
[[ $VERSION =~ ^[0-9]+(\.[0-9]+)+$ ]] || fail "Not a stable release version: $1"

cd "$(dirname "$0")"
if [ "$(sed -n 's/^pkgver=//p' PKGBUILD)" != "$VERSION" ]; then
  sed -i -e "s/^pkgver=.*/pkgver=$VERSION/" -e 's/^pkgrel=.*/pkgrel=1/' PKGBUILD
fi
updpkgsums
makepkg --printsrcinfo > .SRCINFO
