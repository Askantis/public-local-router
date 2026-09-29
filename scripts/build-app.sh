#!/bin/sh
# Builds "build/Local Router.app" from source.
#
#   scripts/build-app.sh                       # ad-hoc signed, for local use
#   SIGN_IDENTITY="Developer ID Application: …" scripts/build-app.sh
#                                              # signed with hardened runtime, ready to notarize
set -e
cd "$(dirname "$0")/.."

swift build -c release

APP="build/Local Router.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$(swift build -c release --show-bin-path)/LocalRouter" "$APP/Contents/MacOS/LocalRouter"
cp Support/Info.plist "$APP/Contents/Info.plist"
cp Support/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
cp -R Resources/ "$APP/Contents/Resources/"

if [ -n "$SIGN_IDENTITY" ]; then
    codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$APP"
else
    codesign --force --sign - "$APP"
fi
echo "Built $APP"
