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
#            darwin | darwin-arm64                      (aarch64-apple-darwin)
#            darwin-x86_64                              (x86_64-apple-darwin)
#            linux | linux-x86_64                       (x86_64-unknown-linux-gnu)
#   An Android appbundle ships arm64-v8a + armeabi-v7a + x86_64, so all three
#   Android ABIs must be vendored for a full ship (else build.rs panics on the
#   missing ones). iOS ships arm64 only. macOS Release is UNIVERSAL (no explicit
#   ARCHS in the Xcode project => arm64 + x86_64, and the Fastfile mac lane
#   cargo-builds both backend targets), so BOTH darwin archives must be vendored.
#   The linux target serves the AppImage (`make build-linux`); on a macOS host
#   the script re-execs itself inside the same Docker toolchain image the
#   AppImage build uses (build it first: `make build-linux` at the repo root, or
#   the `docker buildx build` line of that target).
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
# macOS min-version stamp for the darwin targets (compile env AND `ld -r
# -platform_version`). Must match the app build's MACOSX_DEPLOYMENT_TARGET
# exported in the cargokit podspec script phase.
MACOS_MIN="${MACOS_MIN:-11.0}"

[ -d "$CORE" ] || { echo "error: cr-sqlite core dir not found: $CORE" >&2
  echo "       clone it (see README.md) and pass its core/ dir as arg 2." >&2; exit 1; }

# Resolve the target to a Rust triple, a build family (ios|android) and, for
# Android, the NDK clang wrapper prefix (which differs from the Rust triple for
# armv7: the compiler is `armv7a-linux-androideabi`, note the trailing `a`).
KIND=android
CLANG_PREFIX=""
case "$TARGET" in
  ios)             KIND=ios;     TRIPLE=aarch64-apple-ios ;;
  darwin|darwin-arm64) KIND=darwin; TRIPLE=aarch64-apple-darwin; MACH_ARCH=arm64 ;;
  darwin-x86_64)   KIND=darwin;  TRIPLE=x86_64-apple-darwin;     MACH_ARCH=x86_64 ;;
  linux|linux-x86_64) KIND=linux; TRIPLE=x86_64-unknown-linux-gnu ;;
  android|android-arm64)  TRIPLE=aarch64-linux-android;   CLANG_PREFIX=aarch64-linux-android ;;
  android-armv7)   TRIPLE=armv7-linux-androideabi;        CLANG_PREFIX=armv7a-linux-androideabi ;;
  android-x86_64)  TRIPLE=x86_64-linux-android;           CLANG_PREFIX=x86_64-linux-android ;;
  *) echo "error: unknown target '$TARGET'" >&2
     echo "       expected: ios | android[-arm64] | android-armv7 | android-x86_64 | darwin[-arm64] | darwin-x86_64 | linux[-x86_64]" >&2; exit 1 ;;
esac

# The linux archive must be built with the SAME toolchain image as the AppImage
# (Ubuntu 22.04 glibc floor). On a non-Linux host, re-exec this script inside
# that image; everything below then runs as the in-container Linux branch.
if [ "$KIND" = linux ] && [ "$(uname -s)" != "Linux" ]; then
  IMAGE="${LINUX_BUILD_IMAGE:-bibliogenius-linux-build:22.04}"
  docker image inspect "$IMAGE" >/dev/null 2>&1 || {
    echo "error: Docker image $IMAGE not found; build it first (see 'make build-linux')" >&2
    exit 1
  }
  CRSQLITE_REPO="$(cd "$CORE/.." && pwd)"
  exec docker run --rm --platform linux/amd64 \
    -v "$VENDOR:/vendor" \
    -v "$CRSQLITE_REPO:/crsqlite" \
    --entrypoint /bin/bash \
    "$IMAGE" /vendor/vendor-static.sh "$TARGET" /crsqlite/core
fi
ARCHIVE="crsqlite-$TRIPLE.a"

echo ">> building cr-sqlite static for $TRIPLE (this recompiles std via -Zbuild-std)"
( cd "$CORE" && make clean >/dev/null 2>&1 || true )

if [ "$KIND" = ios ]; then
  # C amalgamation cross-compiles fine via the Makefile's --target/-isysroot.
  ( cd "$CORE" && RUSTUP_TOOLCHAIN="$NIGHTLY" IOS_TARGET="$TRIPLE" make static )
elif [ "$KIND" = darwin ]; then
  # macOS build (native arm64 or x86_64 cross from an arm Mac): drive the triple
  # with a make-var override of CI_MAYBE_TARGET — that sets `cargo --target` and
  # the triple-suffixed dist name, and clang takes `--target=<triple>` natively
  # with the default macOS SDK (no sysroot juggling). No -Zbuild-std here: the
  # prebuilt std for the target must be installed on the toolchain
  # (`rustup target add $TRIPLE --toolchain $NIGHTLY`). MACOSX_DEPLOYMENT_TARGET
  # pins the objects' minos to the app's (cargokit exports the same value);
  # without it clang stamps the current SDK and `ld -r` warns on every object.
  ( cd "$CORE" && RUSTUP_TOOLCHAIN="$NIGHTLY" MACOSX_DEPLOYMENT_TARGET="$MACOS_MIN" \
      make CI_MAYBE_TARGET="$TRIPLE" static )
elif [ "$KIND" = linux ]; then
  # Native build inside the AppImage toolchain container (glibc floor comes from
  # the image, Ubuntu 22.04). CI_GCC holds the COMPILER NAME (the Makefile does
  # `CC:=$(CI_GCC)`), and defining it also skips the C_TARGET flag — needed
  # because gcc does not take clang's `--target=`, and a native build needs no
  # cross flag anyway. The pinned nightly is not baked into the image; install
  # it on the fly (minimal profile, no rust-src: this path skips -Zbuild-std).
  rustup toolchain list | grep -q "$NIGHTLY" \
    || rustup toolchain install "$NIGHTLY" --profile minimal
  # Best-effort bitcode reduction for the crate's own objects. NOT sufficient on
  # its own: the PREBUILT std rlibs ship with embedded LLVM-17 `.llvmbc` that
  # these flags cannot touch, which Ubuntu 22.04's binutils (LLVM-14 plugin)
  # abort on — the operative fix is the `.llvmbc` section strip in the ELF
  # relink below.
  ( cd "$CORE" && RUSTUP_TOOLCHAIN="$NIGHTLY" \
      CARGO_PROFILE_RELEASE_LTO=off RUSTFLAGS="-Cembed-bitcode=no" \
      make CI_MAYBE_TARGET="$TRIPLE" CI_GCC=gcc static )
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
  if [ "$KIND" = ios ] || [ "$KIND" = darwin ]; then
    # Mach-O: a single partial link that re-exports only the entry point.
    ARCH="${MACH_ARCH:-arm64}"
    printf '_sqlite3_crsqlite_init\n' > exports.txt
    if [ "$KIND" = darwin ]; then
      # Two macOS-specific twists vs the iOS line below: (1) the plain-C objects
      # built with `--target=<triple>` carry no LC_BUILD_VERSION, so `ld -r`
      # cannot infer the platform and demands `-platform_version` (11.0 matches
      # the app build's MACOSX_DEPLOYMENT_TARGET, exported in the cargokit
      # podspec script phase); (2) `ld -r` loads an ARCHIVE lazily — with no
      # root reference it pulls members arbitrarily and can DROP the entry
      # point (observed: `_sqlite3_crsqlite_init` absent from merged.o) — so
      # extract the objects and link them explicitly.
      ar -x "$BUILT"
      xcrun ld -r -arch "$ARCH" \
        -platform_version macos "$MACOS_MIN" "$(xcrun --sdk macosx --show-sdk-version)" \
        -x ./*.o -exported_symbols_list exports.txt -o merged.o
    else
      xcrun ld -r -arch "$ARCH" -x "$BUILT" -exported_symbols_list exports.txt -o merged.o
    fi
    xcrun ar -rcs "$WORK/out.a" merged.o
  else
    # ELF: merge first (ld -r), THEN localize with objcopy (localizing a
    # multi-object archive directly would strand internal cross-object refs).
    # Android relinks with the NDK llvm tools; linux runs inside the toolchain
    # container where the system GNU binutils do the same job.
    if [ "$KIND" = linux ]; then
      AR=ar; LD=ld; OBJCOPY=objcopy
    else
      NDK="${ANDROID_NDK_HOME:-$(ls -d "$HOME"/Library/Android/sdk/ndk/* 2>/dev/null | sort -V | tail -1)}"
      BIN="$NDK/toolchains/llvm/prebuilt/darwin-x86_64/bin"
      AR="$BIN/llvm-ar"; LD="$BIN/ld.lld"; OBJCOPY="$BIN/llvm-objcopy"
    fi
    "$AR" x "$BUILT"
    "$LD" -r ./*.o -o merged.o
    # Also strip the embedded LLVM bitcode sections: the nightly's rust objects
    # carry LLVM-17 `.llvmbc`/`.llvmcmd` next to the machine code, and any GNU
    # binutils step that probes them with an older plugin (Ubuntu 22.04 `ar s`
    # indexing, or the final app link) aborts with "LLVM ERROR: Invalid
    # encoding". The machine code is all we ship; the bitcode is dead weight.
    "$OBJCOPY" --keep-global-symbol=sqlite3_crsqlite_init \
      --remove-section='.llvmbc' --remove-section='.llvmcmd' \
      merged.o out_obj.o
    "$AR" rcs "$WORK/out.a" out_obj.o
  fi )

# Validate the relink invariant BEFORE replacing the vendored archive: exactly
# one defined global symbol, and it is the extension entry point. This is not
# theoretical: `ld -r` fed the archive directly once lazy-loaded members and
# silently DROPPED `sqlite3_crsqlite_init` (exit 0, unusable artifact).
echo ">> validating relinked archive (single exported global = entry point)"
if ! nm "$WORK/out.a" 2>/dev/null | grep -Eq ' T _?sqlite3_crsqlite_init$'; then
  echo "error: relinked archive lost the entry point sqlite3_crsqlite_init" >&2
  exit 1
fi
GLOBALS="$(nm -g "$WORK/out.a" 2>/dev/null | grep -cE '^[0-9a-f]+ [TSDW] ' || true)"
if [ "$GLOBALS" != 1 ]; then
  echo "error: relinked archive exports $GLOBALS defined globals (expected exactly 1)" >&2
  exit 1
fi

cp "$WORK/out.a" "$VENDOR/$ARCHIVE"

echo ">> updating CHECKSUMS.txt"
# shasum is the macOS spelling; the linux toolchain container has sha256sum.
if command -v shasum >/dev/null 2>&1; then
  NEW="$(shasum -a 256 "$VENDOR/$ARCHIVE" | awk '{print $1}')"
else
  NEW="$(sha256sum "$VENDOR/$ARCHIVE" | awk '{print $1}')"
fi
CS="$VENDOR/CHECKSUMS.txt"
touch "$CS"
grep -v " $ARCHIVE\$" "$CS" > "$CS.tmp" || true
echo "$NEW  $ARCHIVE" >> "$CS.tmp"
sort "$CS.tmp" -o "$CS"
rm -f "$CS.tmp"

echo ">> done: $VENDOR/$ARCHIVE"
echo "   $NEW"
