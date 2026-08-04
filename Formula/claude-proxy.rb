class ClaudeProxy < Formula
  desc "Local HTTPS MITM proxy, API translator, and aggregator for Claude, Gemini, and OpenAI clients"
  homepage "https://github.com/aero/claude-proxy"
  version "1.1.1"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aero/claude-proxy/releases/download/1.1.1/claude-proxy-macos-arm64.zip"
      sha256 "6a8398b04abf380c0c604becd3298f4c7e2dd980f3b77a15f0650298dd519a30"
    end
  end

  def install
    bin.install "claude-proxy"
  end

  test do
    assert_match "claude-proxy #{version}", shell_output("#{bin}/claude-proxy --version")
  end
end
