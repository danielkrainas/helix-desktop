# Tests

Contract and integration tests for Helix.

## `contract/` — conformance against helix-desktop-spec

`cargo test -p helix-contract-tests` starts each D-Bus service on a private
bus, introspects it, and compares the result with the contract XML in
[helix-desktop-spec](https://github.com/danielkrainas/helix-desktop-spec). It
fails on:

- contract members that are missing or have a different signature,
- exported members that are not in the contract,
- entries in a test's `KNOWN_UNIMPLEMENTED` list that are now implemented.

The spec is found at `../helix-desktop-spec` (a sibling checkout) or at
`$HELIX_SPEC_DIR`.

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
