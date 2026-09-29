#!/bin/sh
# Packages build/Local Router.app into dist/Local-Router.zip and dist/Local-Router.dmg.
# Run scripts/build-app.sh first. With SIGN_IDENTITY set, the DMG is signed too.
set -e
cd "$(dirname "$0")/.."

APP="build/Local Router.app"
[ -d "$APP" ] || { echo "Missing $APP. Run scripts/build-app.sh first."; exit 1; }

rm -rf dist build/dmg
mkdir -p dist build/dmg

# ZIP for Homebrew; ditto keeps the bundle's structure and attributes.
ditto -c -k --sequesterRsrc --keepParent "$APP" dist/Local-Router.zip

# DMG for people: the app plus a shortcut to Applications to drag it onto.
cp -R "$APP" build/dmg/
ln -s /Applications build/dmg/Applications
hdiutil create -volname "Local Router" -srcfolder build/dmg -ov -format UDZO dist/Local-Router.dmg
if [ -n "$SIGN_IDENTITY" ]; then
    codesign --force --timestamp --sign "$SIGN_IDENTITY" dist/Local-Router.dmg
fi

( cd dist && shasum -a 256 Local-Router.zip Local-Router.dmg > SHA256SUMS.txt )
cat dist/SHA256SUMS.txt
