class ClaudeProxy < Formula
  desc "Local HTTPS MITM proxy, API translator, and aggregator for Claude, Gemini, and OpenAI clients"
  homepage "https://github.com/aeroxy/claude-proxy"
  version "1.6.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aeroxy/claude-proxy/releases/download/1.6.0/claude-proxy-macos-arm64.zip"
      sha256 "59c4b73fe08567db9d816e2bdc3a529ba54421d1b8d69a1b151b0aa54f6d793f"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/aeroxy/claude-proxy/releases/download/1.6.0/claude-proxy-linux-x86_64.zip"
      sha256 "90ce46826f6f74a35804ffb8dae9191ee09ea290b194a4b3939451b49b103499"
    end
  end

  def install
    bin.install "claude-proxy"
  end

  test do
    assert_match "claude-proxy #{version}", shell_output("#{bin}/claude-proxy --version")
  end
end
