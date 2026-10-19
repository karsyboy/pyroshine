#!/bin/sh
# Post-install script for Pyroshine packages.

# Remove manifests left by previous portable/manual releases. Native package
# upgrades remove their old /usr/share manifests through package ownership.
rm -f /etc/vulkan/implicit_layer.d/VkLayer_pyroshine_wsi.json \
  /etc/vulkan/implicit_layer.d/VkLayer_moonshine_wsi.json

# The server starts the tray itself now. The replaced desktop app's
# autostart entry can outlive it (a Debian conffile stays until purged) and
# would start a program that no longer exists at every login.
if [ ! -e /usr/bin/pyroshine-ui ]; then
  rm -f /etc/xdg/autostart/pyroshine-ui.desktop
fi

udevadm control --reload || true
udevadm trigger || true
systemd-sysusers 2>/dev/null || true
modprobe uinput || true
modprobe uhid || true
gtk-update-icon-cache -q -t /usr/share/icons/hicolor 2>/dev/null || true
update-desktop-database -q /usr/share/applications 2>/dev/null || true

echo "pyroshine: enable for your user with:"
echo "  sudo loginctl enable-linger <user>   # optional, for headless use"
echo "  sudo systemctl enable --now pyroshine@<user>"
echo "Then run 'pyroshine web setup' as that user and open http://127.0.0.1:47990/"
echo "(application menu: Pyroshine). In a desktop session the tray icon appears on its own."
