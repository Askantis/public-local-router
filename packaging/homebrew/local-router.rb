# Copy to Askantis/homebrew-tap as Casks/local-router.rb. The release workflow keeps version and sha256 current.
cask "local-router" do
  version "0.1.0"
  sha256 "REPLACE_WITH_SHA256_OF_ZIP" # shasum -a 256 Local-Router.zip

  url "https://github.com/Askantis/public-local-router/releases/download/v#{version}/Local-Router.zip"
  name "Local Router"
  desc "Menu bar app for managing and routing local Ollama models"
  homepage "https://github.com/Askantis/public-local-router"

  livecheck do
    url :url
    strategy :github_latest
  end

  # Uncomment once Sparkle in-app updates ship (brew then leaves upgrades to the app):
  # auto_updates true
  depends_on macos: :sequoia # current syntax: minimum version, i.e. 15+

  app "Local Router.app"

  # Stop the running app before uninstall/upgrade.
  uninstall quit: "ch.askantis.localrouter"

  # Only our own files. Do NOT touch ~/.ollama (user's models / the symlink the app manages).
  zap trash: [
    "~/Library/Application Support/ch.askantis.localrouter",
    "~/Library/Caches/ch.askantis.localrouter",
    "~/Library/HTTPStorages/ch.askantis.localrouter",
    "~/Library/Preferences/ch.askantis.localrouter.plist",
    "~/Library/Saved Application State/ch.askantis.localrouter.savedState",
  ]

  # Only while the app is NOT notarized. Remove once releases are notarized.
  caveats <<~EOS
    Local Router is not yet notarized by Apple. On first launch macOS will block it.
    Open System Settings > Privacy & Security, click "Open Anyway" next to
    "Local Router", and confirm with Touch ID or your password.
  EOS
end
