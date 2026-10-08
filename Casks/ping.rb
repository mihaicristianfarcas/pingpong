# Written by tools/update-casks for each release: change that script, not
# this file.
cask "ping" do
  arch arm: "arm64", intel: "x86_64"

  version "0.9.1"
  sha256 arm:   "a26cdff2186ee115ba807990aa7d0b77399a00bc005bf8486424786fbd5c7563",
         intel: "9bd8bd60b35b193c1890c75ed0665d2e97b806183305ebedd7718239a1fe5c7c"

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
