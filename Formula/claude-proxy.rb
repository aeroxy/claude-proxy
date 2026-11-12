class ClaudeProxy < Formula
  desc "Local HTTPS MITM proxy, API translator, and aggregator for Claude, Gemini, and OpenAI clients"
  homepage "https://github.com/aeroxy/claude-proxy"
  version "1.5.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aeroxy/claude-proxy/releases/download/1.5.0/claude-proxy-macos-arm64.zip"
      sha256 "1cf9e0082dfcb21cfc723de2d0fe4a1ec04890145e022665c979716d41d59ef7"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/aeroxy/claude-proxy/releases/download/1.5.0/claude-proxy-linux-x86_64.zip"
      sha256 ""
    end
  end

  def install
    bin.install "claude-proxy"
  end

  test do
    assert_match "claude-proxy #{version}", shell_output("#{bin}/claude-proxy --version")
  end
end
