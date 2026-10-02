# Written by tools/update-casks for each release: change that script, not
# this file.
cask "pong" do
  arch arm: "arm64", intel: "x86_64"

  version "0.7.0"
  sha256 arm:   "22d93e125fe309fbe016d72afac973e86d7261f5417358f72d2d3e111c0412a1",
         intel: "8153df9a0b5fba034a51ea0678763cb8de422f54722faae2f087a077c82e024f"

  url "https://github.com/mihaicristianfarcas/pingpong/releases/download/v#{version}/Pong-#{version}-macos-#{arch}.zip"
  name "Pong"
  desc "Desktop and game streaming host for Ping clients, post-quantum encrypted"
  homepage "https://github.com/mihaicristianfarcas/pingpong"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: :sonoma

  app "Pong.app"
  app "Pong Control.app"

  uninstall launchctl: [
              "dev.pingpong.Pong",
              "dev.pingpong.PongControl",
            ],
            quit:      "dev.pingpong.PongControl"

  zap trash: [
    "~/Library/Application Support/Pong",
    "~/Library/Logs/Pong",
  ]

  caveats <<~EOS
    Open Pong Control and choose Start Pong: the host then runs whenever you
    are logged in, and Pong's icon is in the menu bar. macOS asks for Screen
    Recording and Accessibility the first time.
  EOS
end
