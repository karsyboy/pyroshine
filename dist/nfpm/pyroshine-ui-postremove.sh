#!/bin/sh
# Post-remove script for the Pyroshine desktop app package.

gtk-update-icon-cache -q -t /usr/share/icons/hicolor 2>/dev/null || true
update-desktop-database -q /usr/share/applications 2>/dev/null || true
