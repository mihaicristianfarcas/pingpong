# Written by tools/update-casks for each release: change that script, not
# this file.
cask "ping" do
  arch arm: "arm64", intel: "x86_64"

  version "0.6.1"
  sha256 arm:   "97176974379de23fc615db8849ee6a461346b94b32d4672349e227cba9bce80d",
         intel: "dfaff0c44198fa3458b5dcadb4ded173e461fca5258c1068770d3b4be28c2c43"

  url "https://github.com/mihaicristianfarcas/pingpong/releases/download/v#{version}/Ping-#{version}-macos-#{arch}.zip"
  name "Ping"
  desc "Desktop and game streaming client for Pong hosts, post-quantum encrypted"
  homepage "https://github.com/mihaicristianfarcas/pingpong"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on macos: :sonoma

  app "Ping.app"

  zap trash: [
    "~/Library/Application Support/Ping",
    "~/Library/Logs/Ping",
  ]
end
