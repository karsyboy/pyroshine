#!/bin/sh
# Post-install script for the Pyroshine tray package.

gtk-update-icon-cache -q -t /usr/share/icons/hicolor 2>/dev/null || true
update-desktop-database -q /usr/share/applications 2>/dev/null || true

# The replaced desktop app's autostart entry can outlive it (a Debian
# conffile stays until purged); it would start a program that no longer
# exists at every login. Remove it only once that program is gone.
if [ -f /etc/xdg/autostart/pyroshine-ui.desktop ] && [ ! -e /usr/bin/pyroshine-ui ]; then
  rm -f /etc/xdg/autostart/pyroshine-ui.desktop
fi

echo "pyroshine-tray: the status icon starts at your next desktop login, or run"
echo "  'pyroshine-tray' now. Manage Pyroshine in the browser (application menu: Pyroshine)."
