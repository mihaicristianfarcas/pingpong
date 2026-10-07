# Written by tools/update-casks for each release: change that script, not
# this file.
cask "pong" do
  arch arm: "arm64", intel: "x86_64"

  version "0.9.0"
  sha256 arm:   "5e0b724d24483e6e89b1d9bc83bb80de737ddb9e22537f22ff8216483ca0aa9c",
         intel: "aac96916d1cd1215630247440cfbb781cb5066971915e7683f575549813a1b11"

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
