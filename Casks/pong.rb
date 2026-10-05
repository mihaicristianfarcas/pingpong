# Written by tools/update-casks for each release: change that script, not
# this file.
cask "pong" do
  arch arm: "arm64", intel: "x86_64"

  version "0.8.0"
  sha256 arm:   "1a3ae65beff4c24912bafca5aa023d96fbee4853b735ffe8b486fa6c8b320779",
         intel: "6c7280553c16dcc2e8a6660d5416ceca7e6ff996afe0d3a307aec88be9f28705"

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
