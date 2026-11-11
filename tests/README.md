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
`$HELIX_SPEC_DIR`. The tests need `dbus-daemon` and are skipped without it.

Shared helpers (private test bus, the comparison itself) live in
`crates/helix-test-support`.
