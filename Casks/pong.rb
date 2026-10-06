# Written by tools/update-casks for each release: change that script, not
# this file.
cask "pong" do
  arch arm: "arm64", intel: "x86_64"

  version "0.8.2"
  sha256 arm:   "3823ea1580723f89fa223840de9afdc2a5e4924869bc0acdb5af131f61250875",
         intel: "52d53ac9ade921ca1e46c421ccb82429facc5dea6e039993ec763226f2dff066"

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
