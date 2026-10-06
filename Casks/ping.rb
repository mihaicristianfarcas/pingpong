# Written by tools/update-casks for each release: change that script, not
# this file.
cask "ping" do
  arch arm: "arm64", intel: "x86_64"

  version "0.8.1"
  sha256 arm:   "467aa3f6ce6d0ef9a9edf1ab10b77354201f0d068bba90dc54cca23299ea78cc",
         intel: "3d8af0bb48014b08021ed66f27e5382a7643eb45e7882cfacfc658734462f92d"

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
