class Gitrust < Formula
  desc "Agentic Git CLI — natural language interface for Git"
  homepage "https://github.com/YOUR_ORG/gitrust"
  version "0.1.0"

  depends_on "ollama"

  on_macos do
    on_arm do
      url "https://github.com/YOUR_ORG/gitrust/releases/download/v#{version}/gitrust-macos-aarch64"
      sha256 "PLACEHOLDER_SHA256_AARCH64"
    end
    on_intel do
      url "https://github.com/YOUR_ORG/gitrust/releases/download/v#{version}/gitrust-macos-x86_64"
      sha256 "PLACEHOLDER_SHA256_X86_64"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/YOUR_ORG/gitrust/releases/download/v#{version}/gitrust-linux-aarch64"
      sha256 "PLACEHOLDER_SHA256_LINUX_AARCH64"
    end
    on_intel do
      url "https://github.com/YOUR_ORG/gitrust/releases/download/v#{version}/gitrust-linux-x86_64"
      sha256 "PLACEHOLDER_SHA256_LINUX_X86_64"
    end
  end

  def install
    bin.install stable.url.split("/").last => "gitrust"
  end

  def caveats
    <<~EOS
      One-time setup required (downloads ~2GB model):
        gitrust setup

      This will:
        • Verify Ollama is running  (brew services start ollama)
        • Pull qwen2.5:3b from the Ollama registry
    EOS
  end

  test do
    system "#{bin}/gitrust", "--version"
  end
end
