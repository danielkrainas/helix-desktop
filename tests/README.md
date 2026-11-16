# Tests

Contract and integration tests for Helix.

## `contract/` — conformance against helixdesktop/spec

`cargo test -p helix-contract-tests` starts each D-Bus service on a private
bus, introspects it, and compares the result with the contract XML in
[helixdesktop/spec](https://github.com/helixdesktop/spec). It
fails on:

- contract members that are missing or have a different signature,
- exported members that are not in the contract,
- entries in a test's `KNOWN_UNIMPLEMENTED` list that are now implemented.

The spec is found at `$HELIX_SPEC_DIR`, or else in a sibling checkout,
`../spec` (or `../helix-desktop-spec`, its former name).

The GSettings check compares the installed schemas with the spec's key
tables, which describe GNOME 51. A key with a different type fails; a key
missing from older installed schemas only warns.

## Environment switches

| Variable | Effect |
|----------|--------|
| `HELIX_SPEC_DIR` | Where the spec checkout is (default: sibling directory) |
| `HELIX_REQUIRE_DBUS=1` | Fail, instead of skip, D-Bus tests when `dbus-daemon` is missing (set in CI) |
| `HELIX_STRICT_GSETTINGS=1` | Also fail on contract keys missing from the installed schemas (use on a GNOME 51 system) |

CI (`.github/workflows/ci.yml`) runs clippy with warnings denied and the
whole test suite on Ubuntu 24.04, with the spec checked out alongside.

Shared helpers (private test bus, the comparison itself) live in
`crates/helix-test-support`.
