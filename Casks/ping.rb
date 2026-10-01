# Written by tools/update-casks for each release: change that script, not
# this file.
cask "ping" do
  arch arm: "arm64", intel: "x86_64"

  version "0.6.0"
  sha256 arm:   "9efdc81566b70e41f5c27d03e913434df64dcbb92809648c78cff7ee9719bdbf",
         intel: "4997039ed5672429776195cf470997160fc2782118d1d5559cd3f33c798b0f7d"

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

  # The apps are signed but not notarized: without this, macOS refuses to
  # open them until they are allowed in System Settings.
  postflight_steps do
    run "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "{{appdir}}/Ping.app"]
  end

  zap trash: [
    "~/Library/Application Support/Ping",
    "~/Library/Logs/Ping",
  ]
end
