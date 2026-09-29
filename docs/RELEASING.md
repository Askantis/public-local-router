# Releasing

Releases are built by [`.github/workflows/release.yml`](../.github/workflows/release.yml) when a version tag is pushed.

```bash
git tag v0.1.0
git push origin v0.1.0
```

The workflow sets the version in `Support/Info.plist` from the tag, builds the app, packages `Local-Router.dmg` and `Local-Router.zip`, and publishes a GitHub release. The file names don't change between versions, so the README's download link (`releases/latest/download/Local-Router.dmg`) always points at the newest release.

## Build a release locally

```bash
scripts/build-app.sh
scripts/package.sh        # dist/Local-Router.zip, dist/Local-Router.dmg, SHA256SUMS.txt
```

Signed and notarized, in one go (needs the Developer ID certificate and an App Store Connect API key):

```bash
SIGN_IDENTITY="Developer ID Application: …" NOTARY_KEY=path/to/AuthKey.p8 NOTARY_KEY_ID=… NOTARY_ISSUER=… \
  scripts/release-local.sh 1.0.0
gh release create v1.0.0 dist/* --title "Local Router 1.0" --notes-file notes.md
```

The release workflow skips tags whose release already exists, so a locally made release isn't built twice.

## Signing and notarization

Without signing secrets, releases are ad-hoc signed, and users confirm the first launch in System Settings. To sign and notarize, you need an [Apple Developer Program](https://developer.apple.com/programs/) membership, then:

1. Create a **Developer ID Application** certificate and export it with its key as a `.p12`.
2. Create an **App Store Connect API key** (Users and Access → Integrations) for `notarytool`.
3. Add the repository secrets listed at the top of the workflow.

The workflow then signs with the hardened runtime, notarizes and staples both the app and the DMG. Locally, the same signing works with `SIGN_IDENTITY="Developer ID Application: …" scripts/build-app.sh`.

## Homebrew

The cask lives in [`Askantis/homebrew-tap`](https://github.com/Askantis/homebrew-tap), so users install with `brew install --cask askantis/tap/local-router`.

1. Create the tap: `brew tap-new askantis/tap`, then push it as `Askantis/homebrew-tap`.
2. Copy [`packaging/homebrew/local-router.rb`](../packaging/homebrew/local-router.rb) to `Casks/local-router.rb` in the tap, with the sha256 of the first release's ZIP.
3. Add a `TAP_GITHUB_TOKEN` secret (a fine-grained token with write access to the tap). From then on, each release updates the cask's version and sha256.
4. Check it with `brew audit --cask askantis/tap/local-router`.

Remove the cask's `caveats` block once releases are notarized.

The official Homebrew cask repository needs notarized apps and a notable project (about 225 stars, and at least 30 days old), so the tap comes first.

## Refresh the model catalog

```bash
scripts/update-catalog.py
```

It rebuilds `Resources/catalog.json` and the makers' icons from the Ollama library. Commit the result.
