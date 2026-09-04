class Pixel < Formula
  desc "Local control layer for coding agents — deterministic retrieval + git engine"
  homepage "https://github.com/LivioGama/pixel"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/LivioGama/pixel/releases/download/v#{version}/pixel-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "__ARM_MAC_SHA256__"
    end
    on_intel do
      url "https://github.com/LivioGama/pixel/releases/download/v#{version}/pixel-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "__INTEL_MAC_SHA256__"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/LivioGama/pixel/releases/download/v#{version}/pixel-v#{version}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "__ARM_LINUX_SHA256__"
    end
    on_intel do
      url "https://github.com/LivioGama/pixel/releases/download/v#{version}/pixel-v#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "__INTEL_LINUX_SHA256__"
    end
  end

  def install
    bin.install "bin/pixel"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/pixel --version")
  end
end
