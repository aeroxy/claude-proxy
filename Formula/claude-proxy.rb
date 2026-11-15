class ClaudeProxy < Formula
  desc "Local HTTPS MITM proxy, API translator, and aggregator for Claude, Gemini, and OpenAI clients"
  homepage "https://github.com/aeroxy/claude-proxy"
  version "1.6.1"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aeroxy/claude-proxy/releases/download/1.6.1/claude-proxy-macos-arm64.zip"
      sha256 "76474b37e54d8f40ea4d530b0c84ea5005de1794ad7006d34749fe46c2029aa8"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/aeroxy/claude-proxy/releases/download/1.6.1/claude-proxy-linux-x86_64.zip"
      sha256 "aa8780d35b36a4deb908da963253bc8e8467dfda395b3b508ea7bf4ec632a434"
    end
  end

  def install
    bin.install "claude-proxy"
  end

  test do
    assert_match "claude-proxy #{version}", shell_output("#{bin}/claude-proxy --version")
  end
end
