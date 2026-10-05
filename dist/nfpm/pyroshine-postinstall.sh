#!/bin/sh
# Post-install script for Pyroshine packages.

# Remove manifests left by previous portable/manual releases. Native package
# upgrades remove their old /usr/share manifests through package ownership.
rm -f /etc/vulkan/implicit_layer.d/VkLayer_pyroshine_wsi.json \
  /etc/vulkan/implicit_layer.d/VkLayer_moonshine_wsi.json

udevadm control --reload || true
udevadm trigger || true
systemd-sysusers 2>/dev/null || true
modprobe uinput || true
modprobe uhid || true

echo "pyroshine: enable for your user with:"
echo "  sudo loginctl enable-linger <user>   # optional, for headless use"
echo "  sudo systemctl enable --now pyroshine@<user>"
