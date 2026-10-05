#!/bin/sh
# Post-install script for the Pyroshine desktop app package.

gtk-update-icon-cache -q -t /usr/share/icons/hicolor 2>/dev/null || true
update-desktop-database -q /usr/share/applications 2>/dev/null || true

echo "pyroshine-ui: starts in the system tray at your next desktop login,"
echo "  or run 'pyroshine-ui' now. It manages a running pyroshine@<user> service."
