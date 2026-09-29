#!/bin/sh
# Builds, signs, notarizes and packages a release on this Mac.
#
#   SIGN_IDENTITY="Developer ID Application: …" \
#   NOTARY_KEY=~/.appstoreconnect/private_keys/AuthKey_XXXX.p8 NOTARY_KEY_ID=XXXX NOTARY_ISSUER=… \
#   scripts/release-local.sh 1.0.0
#
# Produces dist/Local-Router.dmg and dist/Local-Router.zip, both notarized and stapled.
set -e
cd "$(dirname "$0")/.."
VERSION="${1:?usage: scripts/release-local.sh <version>}"
: "${SIGN_IDENTITY:?set SIGN_IDENTITY}" "${NOTARY_KEY:?set NOTARY_KEY}" "${NOTARY_KEY_ID:?set NOTARY_KEY_ID}" "${NOTARY_ISSUER:?set NOTARY_ISSUER}"
NOTARY="--key $NOTARY_KEY --key-id $NOTARY_KEY_ID --issuer $NOTARY_ISSUER"

/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" Support/Info.plist
SIGN_IDENTITY="$SIGN_IDENTITY" scripts/build-app.sh
codesign --verify --strict --verbose=2 "build/Local Router.app"

echo "Notarizing the app…"
ditto -c -k --sequesterRsrc --keepParent "build/Local Router.app" build/notarize.zip
xcrun notarytool submit build/notarize.zip $NOTARY --wait
xcrun stapler staple "build/Local Router.app"

SIGN_IDENTITY="$SIGN_IDENTITY" scripts/package.sh

echo "Notarizing the DMG…"
xcrun notarytool submit dist/Local-Router.dmg $NOTARY --wait
xcrun stapler staple dist/Local-Router.dmg
( cd dist && shasum -a 256 Local-Router.zip Local-Router.dmg > SHA256SUMS.txt )

spctl --assess --type execute -vv "build/Local Router.app"
spctl --assess --type open --context context:primary-signature -vv dist/Local-Router.dmg
cat dist/SHA256SUMS.txt
