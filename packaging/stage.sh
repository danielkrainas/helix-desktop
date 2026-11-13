#!/bin/sh
# Lay out an installed Helix under DESTDIR, as packages and the release
# tarball ship it.
#
#   packaging/stage.sh DESTDIR [debian|redhat|arch]
#
# Binaries come from $BIN_DIR (default target/release). The PAM service
# file differs by distribution family (default debian).
set -eu

dest=${1:?usage: packaging/stage.sh DESTDIR [debian|redhat|arch]}
pam=${2:-debian}
bin=${BIN_DIR:-target/release}
root=$(cd "$(dirname "$0")/.." && pwd)

case $pam in
    debian) pam_file=$root/session/pam.d/helix ;;
    redhat|arch) pam_file=$root/packaging/pam.d/helix-redhat ;;
    *) echo "unknown PAM variant: $pam" >&2; exit 2 ;;
esac

for b in helix-comp helix-shell helix-session helix-portals helix-bluetooth-sendto; do
    install -Dm755 "$bin/$b" "$dest/usr/bin/$b"
done
install -Dm644 "$root/session/helix.desktop" "$dest/usr/share/wayland-sessions/helix.desktop"
install -Dm644 "$root/session/helix.portal" "$dest/usr/share/xdg-desktop-portal/portals/helix.portal"
install -Dm644 "$root/session/helix-portals.conf" "$dest/usr/share/xdg-desktop-portal/helix-portals.conf"
install -Dm644 "$root/session/helix-session.toml" "$dest/etc/xdg/helix/session.toml"
install -Dm644 "$pam_file" "$dest/etc/pam.d/helix"
install -Dm644 "$root/data/org.helix.shell.gschema.xml" "$dest/usr/share/glib-2.0/schemas/org.helix.shell.gschema.xml"
install -Dm644 "$root/LICENSE" "$dest/usr/share/licenses/helix-desktop/LICENSE"
install -Dm644 "$root/README.md" "$dest/usr/share/doc/helix-desktop/README.md"
