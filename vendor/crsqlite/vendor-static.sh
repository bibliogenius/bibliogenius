#!/usr/bin/env bash
#
# Build + vendor a cr-sqlite STATIC archive for one mobile target/ABI, applying
# the symbol-localization relink that lets it embed ALONGSIDE the app's own Rust
# without a duplicate-symbol link error or a runtime `dlopen` failure.
#
# WHY THE RELINK: cr-sqlite's `crsql_bundle` is built with `-Zbuild-std` and
# `#![feature(lang_items)]`, so it defines its OWN copy of `rust_eh_personality`,
# `__rust_alloc`, etc. Linked next to the app's std those collide (`duplicate
# symbol '_rust_eh_personality'`). We relink each archive so ONLY the extension
# entry point `sqlite3_crsqlite_init` stays global and every other symbol becomes
# local. The merge-into-one-object step is REQUIRED first: localizing symbols in
# a multi-object archive breaks INTERNAL cross-object refs (e.g. the runtime error
# `cannot locate symbol "crsql_changesModule"`); after merging they resolve locally.
# cr-sqlite only ever crosses the boundary via the SQLite C ABI, never the Rust
# heap, so localizing its allocator shims is safe. (ADR-044; validated 2026-07-02.)
#
# USAGE:
#   ./vendor-static.sh <target> [cr-sqlite-core-dir]
#   target = ios                                        (aarch64-apple-ios)
#            android | android-arm64                    (aarch64-linux-android)
#            android-armv7                              (armv7-linux-androideabi)
#            android-x86_64                             (x86_64-linux-android)
#   An Android appbundle ships arm64-v8a + armeabi-v7a + x86_64, so all three
#   Android ABIs must be vendored for a full ship (else build.rs panics on the
#   missing ones). iOS ships arm64 only.
#
# PREREQS (see README.md):
#   - Rust nightly + rust-src component (for -Zbuild-std). Override the toolchain
#     with RUSTUP_TOOLCHAIN (default: nightly-2023-10-05, the cr-sqlite v0.16.3 era).
#   - ios:     a full Xcode (iPhoneOS SDK), `aarch64-apple-ios` rust target.
#   - android: Android NDK (ANDROID_NDK_HOME, or auto-detected under
#              ~/Library/Android/sdk/ndk/*), `cargo-ndk`, and the matching rust
#              target (`aarch64-linux-android` / `armv7-linux-androideabi` /
#              `x86_64-linux-android`).
#
# Re-run on a cr-sqlite version bump (and re-verify CHECKSUMS.txt downstream).
set -euo pipefail

TARGET="${1:?usage: vendor-static.sh <ios|android|android-arm64|android-armv7|android-x86_64> [cr-sqlite-core-dir]}"
VENDOR="$(cd "$(dirname "$0")" && pwd)"
CORE="${2:-$VENDOR/../../../_ressources/cr-sqlite/core}"
NIGHTLY="${RUSTUP_TOOLCHAIN:-nightly-2023-10-05}"

[ -d "$CORE" ] || { echo "error: cr-sqlite core dir not found: $CORE" >&2
  echo "       clone it (see README.md) and pass its core/ dir as arg 2." >&2; exit 1; }

# Resolve the target to a Rust triple, a build family (ios|android) and, for
# Android, the NDK clang wrapper prefix (which differs from the Rust triple for
# armv7: the compiler is `armv7a-linux-androideabi`, note the trailing `a`).
KIND=android
CLANG_PREFIX=""
case "$TARGET" in
  ios)             KIND=ios;     TRIPLE=aarch64-apple-ios ;;
  android|android-arm64)  TRIPLE=aarch64-linux-android;   CLANG_PREFIX=aarch64-linux-android ;;
  android-armv7)   TRIPLE=armv7-linux-androideabi;        CLANG_PREFIX=armv7a-linux-androideabi ;;
  android-x86_64)  TRIPLE=x86_64-linux-android;           CLANG_PREFIX=x86_64-linux-android ;;
  *) echo "error: unknown target '$TARGET'" >&2
     echo "       expected: ios | android[-arm64] | android-armv7 | android-x86_64" >&2; exit 1 ;;
esac
ARCHIVE="crsqlite-$TRIPLE.a"

echo ">> building cr-sqlite static for $TRIPLE (this recompiles std via -Zbuild-std)"
( cd "$CORE" && make clean >/dev/null 2>&1 || true )

if [ "$KIND" = ios ]; then
  # C amalgamation cross-compiles fine via the Makefile's --target/-isysroot.
  ( cd "$CORE" && RUSTUP_TOOLCHAIN="$NIGHTLY" IOS_TARGET="$TRIPLE" make static )
else
  # Android needs two things the Makefile's `static` target does NOT set up on a
  # macOS host: (1) the target C compiler / archiver env for build-std's `unwind`
  # crate (else "Unable to invoke compiler"); (2) `ar` = NDK llvm-ar for the final
  # assembly (the macOS BSD `ar` chokes on the GNU/ELF archive with "Read-only
  # file system" on the /NNNN long-name table).
  NDK="${ANDROID_NDK_HOME:-$(ls -d "$HOME"/Library/Android/sdk/ndk/* 2>/dev/null | sort -V | tail -1)}"
  [ -d "$NDK" ] || { echo "error: Android NDK not found; set ANDROID_NDK_HOME" >&2; exit 1; }
  BIN="$NDK/toolchains/llvm/prebuilt/darwin-x86_64/bin"
  # MUST match the app's minSdk (flutter.minSdkVersion, currently 24 = Android 7).
  # Building at a HIGHER API than the app's minSdk links libc symbols absent on
  # older devices → `dlopen` failure on Android below this level. Bump BOTH
  # together. We override the Makefile's hardcoded ANDROID_API_VERSION=33 (used to
  # compile the C ext) AND point build-std's CC at the same per-API clang, so the
  # whole archive targets the floor, not 33.
  API="${ANDROID_API:-24}"
  CC_BIN="$BIN/$CLANG_PREFIX$API-clang"
  [ -x "$CC_BIN" ] || { echo "error: NDK clang not found: $CC_BIN" >&2; exit 1; }
  # cc-rs / cargo env var infixes derived from the Rust triple.
  LOWER="${TRIPLE//-/_}"
  UPPER="$(printf '%s' "$LOWER" | tr '[:lower:]' '[:upper:]')"
  SHIM="$(mktemp -d)"; ln -sf "$BIN/llvm-ar" "$SHIM/ar"
  ( cd "$CORE" && env \
      PATH="$SHIM:$PATH" \
      RUSTUP_TOOLCHAIN="$NIGHTLY" \
      ANDROID_NDK_HOME="$NDK" \
      ANDROID_TARGET="$TRIPLE" \
      "CC_$LOWER=$CC_BIN" \
      "CXX_$LOWER=${CC_BIN}++" \
      "AR_$LOWER=$BIN/llvm-ar" \
      "CARGO_TARGET_${UPPER}_LINKER=$CC_BIN" \
      make ANDROID_API_VERSION="$API" static )
  rm -rf "$SHIM"
fi

BUILT="$CORE/dist/$ARCHIVE"
[ -f "$BUILT" ] || { echo "error: expected build output missing: $BUILT" >&2; exit 1; }

echo ">> relinking: export ONLY sqlite3_crsqlite_init, localize everything else"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
( cd "$WORK"
  if [ "$KIND" = ios ]; then
    # Mach-O: a single partial link that re-exports only the entry point.
    printf '_sqlite3_crsqlite_init\n' > exports.txt
    xcrun ld -r -arch arm64 -x "$BUILT" -exported_symbols_list exports.txt -o merged.o
    xcrun ar -rcs "$WORK/out.a" merged.o
  else
    # ELF: merge first (ld.lld -r), THEN localize with objcopy (localizing a
    # multi-object archive directly would strand internal cross-object refs).
    NDK="${ANDROID_NDK_HOME:-$(ls -d "$HOME"/Library/Android/sdk/ndk/* 2>/dev/null | sort -V | tail -1)}"
    BIN="$NDK/toolchains/llvm/prebuilt/darwin-x86_64/bin"
    "$BIN/llvm-ar" x "$BUILT"
    "$BIN/ld.lld" -r ./*.o -o merged.o
    "$BIN/llvm-objcopy" --keep-global-symbol=sqlite3_crsqlite_init merged.o out_obj.o
    "$BIN/llvm-ar" rcs "$WORK/out.a" out_obj.o
  fi )

cp "$WORK/out.a" "$VENDOR/$ARCHIVE"

echo ">> updating CHECKSUMS.txt"
NEW="$(shasum -a 256 "$VENDOR/$ARCHIVE" | awk '{print $1}')"
CS="$VENDOR/CHECKSUMS.txt"
touch "$CS"
grep -v " $ARCHIVE\$" "$CS" > "$CS.tmp" || true
echo "$NEW  $ARCHIVE" >> "$CS.tmp"
sort "$CS.tmp" -o "$CS"
rm -f "$CS.tmp"

echo ">> done: $VENDOR/$ARCHIVE"
echo "   $NEW"
