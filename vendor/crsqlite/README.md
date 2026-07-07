# Vendored cr-sqlite

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

## Vendor a STATIC archive (iOS / Android / macOS / Linux)

Use `./vendor-static.sh <target>` — it builds cr-sqlite for the target/ABI AND
applies the mandatory symbol-localization **relink**, then updates `CHECKSUMS.txt`.
Do NOT skip the relink: `crsql_bundle` is built with `-Zbuild-std` +
`#![feature(lang_items)]`, so it defines its own `rust_eh_personality` / allocator
shims. Linked next to the app's std they collide (`duplicate symbol
'_rust_eh_personality'`). The script re-exports **only** `sqlite3_crsqlite_init`
and localizes the rest. It merges the objects into one FIRST (Mach-O `ld -r`,
ELF `ld.lld -r`) before localizing — localizing a multi-object archive directly
strands internal cross-object refs and crashes at load with
`cannot locate symbol "crsql_changesModule"`.

```sh
# from a v0.16.3 checkout (see above); pass its core/ dir if not at ../../../_ressources/cr-sqlite/core
./vendor-static.sh ios
./vendor-static.sh android          # arm64-v8a (alias: android-arm64)
./vendor-static.sh android-armv7    # armeabi-v7a
./vendor-static.sh android-x86_64   # x86_64
./vendor-static.sh darwin           # aarch64-apple-darwin (alias: darwin-arm64)
./vendor-static.sh darwin-x86_64    # x86_64-apple-darwin (Intel slice of the universal app)
./vendor-static.sh linux            # x86_64-unknown-linux-gnu (AppImage; alias: linux-x86_64)
```

Prereqs: Rust **nightly + rust-src** (`-Zbuild-std`; override via `RUSTUP_TOOLCHAIN`);
iOS needs a full **Xcode** (iPhoneOS SDK) + the `aarch64-apple-ios` target; Android
needs the **NDK** (`ANDROID_NDK_HOME` or auto-detected) + **`cargo-ndk`** + the
matching rust target (`aarch64-linux-android` / `armv7-linux-androideabi` /
`x86_64-linux-android`). The darwin targets skip `-Zbuild-std` but need the rust
target on the nightly (`rustup target add x86_64-apple-darwin --toolchain <nightly>`
for the Intel slice). The linux target needs **Docker** and the AppImage toolchain
image (`bibliogenius-linux-build:22.04`, see `make build-linux`): the script re-execs
itself inside it so the archive shares the AppImage's glibc floor.

⚠️ **macOS Release ships BOTH darwin archives**: the app builds universal
(arm64 + x86_64) and the Fastfile mac lane cargo-builds both backend targets. And
the darwin archives MUST be the relinked single-object form — the pre-relink arm64
archive (loose bitcode-bearing objects) broke the app's **fat LTO** link with
`failed to get bitcode from object file for LTO (Can't find section __bitcode)`.

⚠️ **Android API level = app minSdk.** The Android archive is compiled at
`ANDROID_API` (default **24** = `flutter.minSdkVersion`). Building at a HIGHER API
than the app's minSdk links libc symbols missing on older devices → `dlopen`
failure on Android below that level. If the app bumps `minSdk`, bump `ANDROID_API`
(or set it: `ANDROID_API=26 ./vendor-static.sh android`) and re-vendor. (iOS is
unaffected: the archive targets a very low `LC_VERSION_MIN_IPHONEOS`.)

⚠️ **Ship coverage:** an Android appbundle builds `arm64-v8a` + `armeabi-v7a` +
`x86_64` by default, and all three are now vendored + wired in `build.rs`. Re-vendor
**all three** (plus iOS) on a cr-sqlite version bump — `build.rs` `panic!`s on any
target ABI whose archive/checksum is missing. If you ever restrict the shipped ABIs
via `abiFilters`, only those need re-vendoring.
