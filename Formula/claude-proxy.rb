class ClaudeProxy < Formula
  desc "Local HTTPS MITM proxy, API translator, and aggregator for Claude, Gemini, and OpenAI clients"
  homepage "https://github.com/aero/claude-proxy"
  version "1.1.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aero/claude-proxy/releases/download/1.1.0/claude-proxy-macos-arm64.zip"
      sha256 "11028c36df5fc40ccb680325c2bbaa1a3465d51be337228a43938d4957ad907e"
    end
  end

  def install
    bin.install "claude-proxy"
  end

  test do
    assert_match "claude-proxy #{version}", shell_output("#{bin}/claude-proxy --version")
  end
end
