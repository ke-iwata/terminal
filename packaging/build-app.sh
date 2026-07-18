#!/bin/bash
# Builds dist/keterm.app (icon + Info.plist + release binary, ad-hoc
# signed) and a matching dist/keterm-<version>-macos-arm64.zip release
# asset. Run from the repo root: packaging/build-app.sh
#
# Requires: cargo build --release already run (or this script runs it),
# and iconutil/qlmanage (both stock macOS tools).
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
echo "Building keterm $VERSION..."

cargo build --release

rm -rf dist
mkdir -p dist/keterm.app/Contents/MacOS
mkdir -p dist/keterm.app/Contents/Resources
cp target/release/terminal dist/keterm.app/Contents/MacOS/keterm

sed "s/VERSION_PLACEHOLDER/$VERSION/g" packaging/Info.plist > dist/keterm.app/Contents/Info.plist

# Rebuild AppIcon.icns from source SVG rather than trusting a possibly
# stale committed .icns.
ICONSET=$(mktemp -d)/AppIcon.iconset
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  double=$((size * 2))
  qlmanage -t -s "$size" -o "$ICONSET" icon/icon.svg >/dev/null 2>&1
  mv "$ICONSET/icon.svg.png" "$ICONSET/icon_${size}x${size}.png"
  qlmanage -t -s "$double" -o "$ICONSET" icon/icon.svg >/dev/null 2>&1
  mv "$ICONSET/icon.svg.png" "$ICONSET/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$ICONSET" -o dist/keterm.app/Contents/Resources/AppIcon.icns
rm -rf "$(dirname "$ICONSET")"

codesign --force --deep --sign - dist/keterm.app
codesign --verify --verbose dist/keterm.app

cd dist
ditto -c -k --sequesterRsrc --keepParent keterm.app "keterm-${VERSION}-macos-arm64.zip"
shasum -a 256 "keterm-${VERSION}-macos-arm64.zip"
cd ..

echo "Done: dist/keterm.app, dist/keterm-${VERSION}-macos-arm64.zip"
