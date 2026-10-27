#!/bin/sh
# Post-remove script for Pyroshine packages.

udevadm control --reload || true
gtk-update-icon-cache -q -t /usr/share/icons/hicolor 2>/dev/null || true
update-desktop-database -q /usr/share/applications 2>/dev/null || true
