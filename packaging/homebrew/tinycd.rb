# Homebrew formula for tinycd. Lives at Formula/tinycd.rb in the
# maxz411/homebrew-tap repository; see packaging/README.md for the release
# checklist that fills in the sha256 values.
class Tinycd < Formula
  desc "Tiny polling and webhook based deployments"
  homepage "https://github.com/maxz411/tinycd"
  version "0.2.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/maxz411/tinycd/releases/download/v0.2.0/tinycd-aarch64-apple-darwin.tar.gz"
      sha256 "AARCH64_APPLE_DARWIN_SHA256"
    end
    on_intel do
      url "https://github.com/maxz411/tinycd/releases/download/v0.2.0/tinycd-x86_64-apple-darwin.tar.gz"
      sha256 "X86_64_APPLE_DARWIN_SHA256"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/maxz411/tinycd/releases/download/v0.2.0/tinycd-aarch64-unknown-linux-musl.tar.gz"
      sha256 "AARCH64_UNKNOWN_LINUX_MUSL_SHA256"
    end
    on_intel do
      url "https://github.com/maxz411/tinycd/releases/download/v0.2.0/tinycd-x86_64-unknown-linux-musl.tar.gz"
      sha256 "X86_64_UNKNOWN_LINUX_MUSL_SHA256"
    end
  end

  def install
    bin.install "tinycd"
  end

  test do
    assert_match "tinycd", shell_output("#{bin}/tinycd --version")
  end
end
