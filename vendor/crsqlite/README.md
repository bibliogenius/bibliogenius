# Vendored cr-sqlite (ST-05 Phase C2)

The cr-sqlite native artifacts are **not committed** (platform-specific build blobs).
Only `CHECKSUMS.txt` is tracked. This directory holds the loadable extension used by
the local dev/test path of the `crsqlite` Cargo feature (see
`src/services/crsqlite_engine.rs` and `ADR-044`).

- **Version pinned:** cr-sqlite **v0.16.3** (vlcn.io).
- **Needed for:** `cargo test --features crsqlite` (the real-engine convergence spike).
  The default build/CI does **not** need it.

## Obtain `crsqlite.dylib` (macOS, dev path)

Build from source at the pinned tag:

```sh
git clone --depth 1 --branch v0.16.3 --recurse-submodules \
  https://github.com/vlcn-io/cr-sqlite
cd cr-sqlite/core && make loadable
# -> dist/crsqlite.dylib
cp dist/crsqlite.dylib <repo>/bibliogenius/vendor/crsqlite/crsqlite.dylib
```

(If you cloned without `--recurse-submodules`, run
`git submodule update --init --recursive` first, or the build fails on the
missing `sqlite_nostd` dependency.)

Alternatively download the official `crsqlite-darwin-aarch64` artifact from the
v0.16.3 GitHub release and unzip it here.

## Verify

```sh
shasum -a 256 -c CHECKSUMS.txt
```

`CHECKSUMS.txt` records the SHA-256 of the macOS arm64 `crsqlite.dylib` this repo was
validated against. Only vendor artifacts from the official vlcn releases (or your own
build at the pinned tag), and re-checksum on any version bump.

## Shipping note

This dynamic `.dylib` is the **dev/test path only**. The shipped app links cr-sqlite
**statically** and registers it in-process (iOS forbids runtime extension loading) —
see ADR-044 sections 2-3.
