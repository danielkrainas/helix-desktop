#!/bin/sh
# Load the extension sandbox's AppArmor profile where AppArmor is on.
if [ -d /sys/kernel/security/apparmor ] && command -v apparmor_parser >/dev/null 2>&1 \
    && [ -f /etc/apparmor.d/helix-extension-sandbox ]; then
    apparmor_parser -r -W /etc/apparmor.d/helix-extension-sandbox || true
fi
exit 0
