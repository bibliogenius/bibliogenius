#!/usr/bin/env bash
#
# Build + vendor a cr-sqlite STATIC archive for one mobile target, applying the
# symbol-localization relink that lets it embed ALONGSIDE the app's own Rust
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
#   ./vendor-static.sh <ios|android> [cr-sqlite-core-dir]
#   (both targets are arm64: aarch64-apple-ios / aarch64-linux-android)
#
# PREREQS (see README.md):
#   - Rust nightly + rust-src component (for -Zbuild-std). Override the toolchain
#     with RUSTUP_TOOLCHAIN (default: nightly-2023-10-05, the cr-sqlite v0.16.3 era).
#   - ios:     a full Xcode (iPhoneOS SDK), `aarch64-apple-ios` rust target.
#   - android: Android NDK (ANDROID_NDK_HOME, or auto-detected under
#              ~/Library/Android/sdk/ndk/*), `cargo-ndk`, `aarch64-linux-android` target.
#
# Re-run on a cr-sqlite version bump (and re-verify CHECKSUMS.txt downstream).
set -euo pipefail

TARGET="${1:?usage: vendor-static.sh <ios|android> [cr-sqlite-core-dir]}"
VENDOR="$(cd "$(dirname "$0")" && pwd)"
CORE="${2:-$VENDOR/../../../_ressources/cr-sqlite/core}"
NIGHTLY="${RUSTUP_TOOLCHAIN:-nightly-2023-10-05}"

[ -d "$CORE" ] || { echo "error: cr-sqlite core dir not found: $CORE" >&2
  echo "       clone it (see README.md) and pass its core/ dir as arg 2." >&2; exit 1; }

case "$TARGET" in
  ios)     TRIPLE=aarch64-apple-ios ;;
  android) TRIPLE=aarch64-linux-android ;;
  *) echo "error: unknown target '$TARGET' (expected ios|android)" >&2; exit 1 ;;
esac
ARCHIVE="crsqlite-$TRIPLE.a"

echo ">> building cr-sqlite static for $TRIPLE (this recompiles std via -Zbuild-std)"
( cd "$CORE" && make clean >/dev/null 2>&1 || true )

if [ "$TARGET" = ios ]; then
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
  # older devices → `dlopen` failure on Android below this level. Bump BOTH together.
  API="${ANDROID_API:-24}"
  SHIM="$(mktemp -d)"; ln -sf "$BIN/llvm-ar" "$SHIM/ar"
  ( cd "$CORE" && PATH="$SHIM:$PATH" \
      RUSTUP_TOOLCHAIN="$NIGHTLY" \
      ANDROID_NDK_HOME="$NDK" \
      ANDROID_TARGET="$TRIPLE" \
      CC_aarch64_linux_android="$BIN/aarch64-linux-android$API-clang" \
      CXX_aarch64_linux_android="$BIN/aarch64-linux-android$API-clang++" \
      AR_aarch64_linux_android="$BIN/llvm-ar" \
      CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$BIN/aarch64-linux-android$API-clang" \
      make static )
  rm -rf "$SHIM"
fi

BUILT="$CORE/dist/$ARCHIVE"
[ -f "$BUILT" ] || { echo "error: expected build output missing: $BUILT" >&2; exit 1; }

echo ">> relinking: export ONLY sqlite3_crsqlite_init, localize everything else"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
( cd "$WORK"
  if [ "$TARGET" = ios ]; then
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
