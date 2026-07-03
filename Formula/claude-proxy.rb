class ClaudeProxy < Formula
  desc "Local HTTPS MITM proxy, API translator, and aggregator for Claude, Gemini, and OpenAI clients"
  homepage "https://github.com/aero/claude-proxy"
  version "1.0.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aero/claude-proxy/releases/download/1.0.0/claude-proxy-macos-arm64.zip"
      sha256 "08eef2a9aacdcfb6b29c212aa88e53203d7119630cf6fdc6ffd0379b825302db"
    end
  end

  def install
    bin.install "claude-proxy"
  end

  test do
    assert_match "claude-proxy #{version}", shell_output("#{bin}/claude-proxy --version")
  end
end
