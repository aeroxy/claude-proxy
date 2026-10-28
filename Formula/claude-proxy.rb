class ClaudeProxy < Formula
  desc "Local HTTPS MITM proxy, API translator, and aggregator for Claude, Gemini, and OpenAI clients"
  homepage "https://github.com/aero/claude-proxy"
  version "1.5.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aero/claude-proxy/releases/download/1.5.0/claude-proxy-macos-arm64.zip"
      sha256 "1cf9e0082dfcb21cfc723de2d0fe4a1ec04890145e022665c979716d41d59ef7"
    end
  end

  def install
    bin.install "claude-proxy"
  end

  test do
    assert_match "claude-proxy #{version}", shell_output("#{bin}/claude-proxy --version")
  end
end
