#!/usr/bin/env bash
# Build Herse and package it as a macOS application bundle.
#
#   packaging/macos/bundle.sh             # -> target/release/Herse.app
#   packaging/macos/bundle.sh --dmg       # also -> target/release/Herse-<version>-<arch>.dmg
#   packaging/macos/bundle.sh --no-build  # reuse an existing target/release/herse
#
# Idempotent: Herse.app (and the DMG) are rebuilt from scratch on every run.
# Does not install anything and does not touch Launch Services.
#
# Environment:
#   CARGO_TARGET_DIR    cargo's target dir (default: <workspace>/target)
#   NICE_LEVEL          nice(1) level for the cargo build (default: 10)
#   PYTHON              python3 with Pillow, used only if AppIcon.icns must be regenerated
#   CODESIGN_IDENTITY   codesign identity (default: "-", ad-hoc)
set -euo pipefail

die() { echo "bundle.sh: error: $*" >&2; exit 1; }
log() { echo "==> $*"; }

DMG=0
BUILD=1
for arg in "$@"; do
  case "$arg" in
    --dmg) DMG=1 ;;
    --no-build) BUILD=0 ;;
    -h | --help) sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $arg (try --help)" ;;
  esac
done

[[ "$(uname -s)" == "Darwin" ]] || die "this script only runs on macOS"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "$HERE/../.." && pwd)"   # packaging/macos -> native/
TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE/target}"
RELEASE_DIR="$TARGET_DIR/release"
APP="$RELEASE_DIR/Herse.app"
NICE_LEVEL="${NICE_LEVEL:-10}"
IDENTITY="${CODESIGN_IDENTITY:--}"

[[ -f "$WORKSPACE/Cargo.toml" ]] || die "no Cargo.toml in $WORKSPACE"
[[ -f "$HERE/Info.plist" ]] || die "missing $HERE/Info.plist"

# rustup's cargo may not be on PATH in a minimal shell.
if ! command -v cargo >/dev/null 2>&1 && [[ -x "$HOME/.cargo/bin/cargo" ]]; then
  PATH="$HOME/.cargo/bin:$PATH"
fi
for tool in cargo codesign plutil xattr ditto; do
  command -v "$tool" >/dev/null 2>&1 || die "required tool not found: $tool"
done
if [[ $DMG -eq 1 ]]; then
  command -v hdiutil >/dev/null 2>&1 || die "required tool not found: hdiutil"
fi

# ---------------------------------------------------------------- build
cd "$WORKSPACE"
if [[ $BUILD -eq 1 ]]; then
  log "cargo build --release -p herse -p herse-cli -p herse-quicklook (nice -n $NICE_LEVEL)"
  nice -n "$NICE_LEVEL" cargo build --release -p herse -p herse-cli -p herse-quicklook
else
  log "skipping cargo build (--no-build)"
fi
BIN="$RELEASE_DIR/herse"
[[ -x "$BIN" ]] || die "release binary not found at $BIN"

# Version of the `herse` crate: `cargo pkgid` prints ...#herse@0.1.0 (or ...#0.1.0).
PKGID="$(cargo pkgid -p herse)"
VERSION="${PKGID##*#}"
VERSION="${VERSION##*@}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+.][0-9A-Za-z.-]+)?$ ]] || die "could not determine crate version from '$PKGID'"
log "herse $VERSION"

# ----------------------------------------------------------------- icon
ICNS="$HERE/AppIcon.icns"
if [[ ! -f "$ICNS" || "$HERE/make_icon.py" -nt "$ICNS" ]]; then
  log "generating AppIcon.icns"
  PY=""
  for cand in "${PYTHON:-}" /opt/anaconda3/bin/python3 python3; do
    [[ -n "$cand" ]] || continue
    if command -v "$cand" >/dev/null 2>&1 && "$cand" -c 'import PIL' >/dev/null 2>&1; then PY="$cand"; break; fi
  done
  [[ -n "$PY" ]] || die "AppIcon.icns is missing/stale and no python3 with Pillow was found (set PYTHON=...)"
  command -v iconutil >/dev/null 2>&1 || die "required tool not found: iconutil"
  "$PY" "$HERE/make_icon.py" --out "$HERE"
fi
[[ -f "$ICNS" ]] || die "missing $ICNS"

# ------------------------------------------------------------- assemble
log "assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

install -m 755 "$BIN" "$APP/Contents/MacOS/herse"
# The `herse` command (crates/cli): Herse ▸ Install Command Line Tool links
# it onto the PATH.
CLI="$RELEASE_DIR/herse-cli"
[[ -x "$CLI" ]] || die "command-line tool not found at $CLI"
install -m 755 "$CLI" "$APP/Contents/MacOS/herse-cli"
sed "s/@VERSION@/$VERSION/g" "$HERE/Info.plist" > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist" >/dev/null || die "generated Info.plist is invalid"
grep -q '@VERSION@' "$APP/Contents/Info.plist" && die "unsubstituted placeholder left in Info.plist"
install -m 644 "$ICNS" "$APP/Contents/Resources/AppIcon.icns"
printf 'APPL????' > "$APP/Contents/PkgInfo"

# ----------------------------------------------------------- quick look
# Finder's space bar on a notebook: a Quick Look preview extension, a small
# Swift shim over the Rust renderer (crates/quicklook). Skipped, with a
# note, where there is no Swift compiler.
QL_SRC="$HERE/QuickLook"
QL_LIB="$RELEASE_DIR/libherse_quicklook.a"
APPEX="$APP/Contents/PlugIns/HerseQuickLook.appex"
if xcrun --find swiftc >/dev/null 2>&1 && [[ -f "$QL_LIB" ]]; then
  log "building the Quick Look extension"
  mkdir -p "$APPEX/Contents/MacOS"
  xcrun swiftc -O -module-name HerseQuickLook -target "$(uname -m)-apple-macos13.0" \
    -application-extension -parse-as-library \
    -import-objc-header "$QL_SRC/herse_ql.h" "$QL_SRC/PreviewProvider.swift" "$QL_LIB" \
    -framework QuickLookUI -framework UniformTypeIdentifiers -framework Cocoa \
    -Xlinker -e -Xlinker _NSExtensionMain -Xlinker -dead_strip \
    -o "$APPEX/Contents/MacOS/HerseQuickLook"
  strip -x "$APPEX/Contents/MacOS/HerseQuickLook"
  sed "s/@VERSION@/$VERSION/g" "$QL_SRC/Info.plist" > "$APPEX/Contents/Info.plist"
  plutil -lint "$APPEX/Contents/Info.plist" >/dev/null || die "generated Quick Look Info.plist is invalid"
else
  log "skipping the Quick Look extension (no Swift compiler, or no $QL_LIB)"
fi

# ----------------------------------------------------------------- sign
# Strip xattrs (resource forks / Finder info make codesign refuse), then sign.
xattr -cr "$APP"
log "codesign ($([[ "$IDENTITY" == "-" ]] && echo ad-hoc || echo "$IDENTITY"))"
# Inside out: the extension first, with its sandbox entitlement (Quick Look
# only loads sandboxed extensions), then the app around it. No --deep, which
# would re-sign the extension without its entitlement.
if [[ -d "$APPEX" ]]; then
  codesign --force -s "$IDENTITY" --entitlements "$QL_SRC/QuickLook.entitlements" "$APPEX"
fi
codesign --force -s "$IDENTITY" "$APP/Contents/MacOS/herse-cli"
codesign --force -s "$IDENTITY" "$APP"
codesign --verify --deep --strict "$APP"

# ------------------------------------------------------------------ dmg
if [[ $DMG -eq 1 ]]; then
  ARCH="$(uname -m)"
  DMG_PATH="$RELEASE_DIR/Herse-$VERSION-$ARCH.dmg"
  STAGE="$(mktemp -d "${TMPDIR:-/tmp}/herse-dmg.XXXXXX")"
  trap 'rm -rf "$STAGE"' EXIT
  log "creating $DMG_PATH"
  ditto "$APP" "$STAGE/Herse.app"
  ln -s /Applications "$STAGE/Applications"
  rm -f "$DMG_PATH"
  hdiutil create -quiet -volname "Herse" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$DMG_PATH"
  hdiutil verify -quiet "$DMG_PATH"
fi

echo
log "done"
echo "  app: $APP"
[[ $DMG -eq 1 ]] && echo "  dmg: $DMG_PATH"
exit 0
