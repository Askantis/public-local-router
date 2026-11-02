#!/bin/sh
# Builds "build/Local Router.app" from source: the menu-bar app (Swift) and its
# background service, localrouterd (Rust).
#
#   scripts/build-app.sh                       # ad-hoc signed, for local use
#   SIGN_IDENTITY="Developer ID Application: …" scripts/build-app.sh
#                                              # signed with hardened runtime, ready to notarize
set -e
cd "$(dirname "$0")/.."

swift build -c release

# An ad-hoc signed development build can't prove it's Local Router by its
# Developer ID, so its service accepts it by identifier (dev-admin). Release
# builds never include that.
if [ -n "$SIGN_IDENTITY" ]; then
    cargo build --release -p localrouterd
else
    cargo build --release -p localrouterd --features dev-admin
fi

APP="build/Local Router.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Library/LaunchAgents"
cp "$(swift build -c release --show-bin-path)/LocalRouter" "$APP/Contents/MacOS/LocalRouter"
cp target/release/localrouterd "$APP/Contents/MacOS/localrouterd"
cp Support/ch.askantis.localrouter.daemon.plist "$APP/Contents/Library/LaunchAgents/"
cp Support/Info.plist "$APP/Contents/Info.plist"
cp Support/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
cp -R Resources/ "$APP/Contents/Resources/"

# Inside-out: the nested service first, then the app around it.
if [ -n "$SIGN_IDENTITY" ]; then
    codesign --force --options runtime --timestamp --identifier ch.askantis.localrouter.daemon \
        --sign "$SIGN_IDENTITY" "$APP/Contents/MacOS/localrouterd"
    codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$APP"
else
    codesign --force --identifier ch.askantis.localrouter.daemon --sign - "$APP/Contents/MacOS/localrouterd"
    codesign --force --sign - "$APP"
fi
echo "Built $APP"
