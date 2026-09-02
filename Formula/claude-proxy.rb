class ClaudeProxy < Formula
  desc "Local HTTPS MITM proxy, API translator, and aggregator for Claude, Gemini, and OpenAI clients"
  homepage "https://github.com/aero/claude-proxy"
  version "1.4.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aero/claude-proxy/releases/download/1.4.0/claude-proxy-macos-arm64.zip"
      sha256 "206bf428bf5c9dfff055f8fafa3bca8cbd2bfd71d0cd045478c63b28fa7fc03c"
    end
  end

  def install
    bin.install "claude-proxy"
  end

  test do
    assert_match "claude-proxy #{version}", shell_output("#{bin}/claude-proxy --version")
  end
end
