#!/bin/sh
# greetd.service gained a drop-in (the Helix greeter's configuration).
if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
fi
exit 0
