class Gitrust < Formula
  desc "Safety layer for Git: plain-English operations, guarded by a local decision model"
  homepage "https://github.com/Aniketdalvi385/gitrust"
  version "0.4.1"

  depends_on "uv" # runs the local Kev decision model (gitrust setup installs it)

  on_macos do
    on_arm do
      url "https://github.com/Aniketdalvi385/gitrust/releases/download/v#{version}/gitrust-macos-aarch64"
      sha256 "PLACEHOLDER_SHA256_AARCH64"
    end
    on_intel do
      url "https://github.com/Aniketdalvi385/gitrust/releases/download/v#{version}/gitrust-macos-x86_64"
      sha256 "PLACEHOLDER_SHA256_X86_64"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Aniketdalvi385/gitrust/releases/download/v#{version}/gitrust-linux-aarch64"
      sha256 "PLACEHOLDER_SHA256_LINUX_AARCH64"
    end
    on_intel do
      url "https://github.com/Aniketdalvi385/gitrust/releases/download/v#{version}/gitrust-linux-x86_64"
      sha256 "PLACEHOLDER_SHA256_LINUX_X86_64"
    end
  end

  def install
    bin.install stable.url.split("/").last => "gitrust"
  end

  def caveats
    <<~EOS
      One-time setup required (~1 GB environment + ~1.5 GB model):
        gitrust setup

      This will:
        • Clone Kev (github.com/jaredpalmer/kev, pinned) into ~/.gitrust/kev
        • Install its environment with uv and download the GiTrust model
          (huggingface.co/AniketDalvi385/gitrust-kev-0.8b)
        • Start the model server in the background and smoke-test it

      The server uses ~3 GB while running and stops itself after 15 idle
      minutes (kev_idle_minutes in ~/.gitrust.toml).
    EOS
  end

  test do
    system "#{bin}/gitrust", "--version"
  end
end
