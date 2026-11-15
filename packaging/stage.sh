#!/bin/sh
# Lay out an installed Helix under DESTDIR, as packages and the release
# tarball ship it.
#
#   packaging/stage.sh DESTDIR [debian|redhat|arch]
#
# Binaries come from $BIN_DIR (default target/release). The PAM service
# file differs by distribution family (default debian). Translations are
# compiled with msgfmt (GNU gettext); packaging/nfpm-locales.sh lists
# them for nfpm.
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

for b in helix-comp helix-shell helix-session helix-portals helix-bluetooth-sendto helix-extensions; do
    install -Dm755 "$bin/$b" "$dest/usr/bin/$b"
done
# The extension sandbox's launcher and its AppArmor profile.
install -Dm755 "$bin/helix-extension-sandbox" "$dest/usr/libexec/helix/helix-extension-sandbox"
install -Dm644 "$root/packaging/apparmor/helix-extension-sandbox" "$dest/etc/apparmor.d/helix-extension-sandbox"
# The session's name and comment in every language of po/LINGUAS
# (Name[de]=…), for login screens.
install -d "$dest/usr/share/wayland-sessions"
msgfmt --desktop --template="$root/session/helix.desktop" -d "$root/po" \
    -o "$dest/usr/share/wayland-sessions/helix.desktop"
chmod 644 "$dest/usr/share/wayland-sessions/helix.desktop"
# Translations (TRANSLATING.md): one catalogue, helix-desktop, for the
# shell, portals, session and greeter.
for lang in $(sed 's/#.*//' "$root/po/LINGUAS"); do
    install -d "$dest/usr/share/locale/$lang/LC_MESSAGES"
    msgfmt --check -o "$dest/usr/share/locale/$lang/LC_MESSAGES/helix-desktop.mo" "$root/po/$lang.po"
done
install -Dm644 "$root/session/helix.portal" "$dest/usr/share/xdg-desktop-portal/portals/helix.portal"
install -Dm644 "$root/session/helix-portals.conf" "$dest/usr/share/xdg-desktop-portal/helix-portals.conf"
install -Dm644 "$root/session/helix-session.toml" "$dest/etc/xdg/helix/session.toml"
install -Dm644 "$root/session/helix-session.target" "$dest/usr/lib/systemd/user/helix-session.target"
# systemd-oomd spares the session bus and other units the session needs.
grep -v '^#' "$root/session/systemd/oomd-avoid.units" | while read -r unit; do
    [ -n "$unit" ] || continue
    install -Dm644 "$root/session/systemd/helix-oomd-avoid.conf" \
        "$dest/usr/lib/systemd/user/$unit.d/50-helix-oomd-avoid.conf"
done
install -Dm644 "$pam_file" "$dest/etc/pam.d/helix"
for schema in "$root"/data/*.gschema.xml; do
    install -Dm644 "$schema" "$dest/usr/share/glib-2.0/schemas/$(basename "$schema")"
done
# The greeter (packaged separately as helix-greeter).
install -Dm755 "$bin/helix-greeter" "$dest/usr/bin/helix-greeter"
install -Dm755 "$root/greeter/helix-greeter-session" "$dest/usr/libexec/helix/helix-greeter-session"
install -Dm644 "$root/greeter/greetd-helix-greeter.toml" "$dest/etc/greetd/helix-greeter.toml"
install -Dm644 "$root/greeter/greetd.service-helix-greeter.conf" "$dest/usr/lib/systemd/system/greetd.service.d/helix-greeter.conf"
install -Dm644 "$root/greeter/README" "$dest/usr/share/doc/helix-greeter/README"
install -Dm644 "$root/greeter/greeter.toml.example" "$dest/usr/share/doc/helix-greeter/greeter.toml.example"
install -Dm644 "$root/LICENSE" "$dest/usr/share/licenses/helix-desktop/LICENSE"
install -Dm644 "$root/README.md" "$dest/usr/share/doc/helix-desktop/README.md"
