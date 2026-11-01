class Tinycd < Formula
  desc "Tiny self-hosted deployments with a dashboard and a GitHub App"
  homepage "https://github.com/maxz411/tinycd"
  version "0.5.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/maxz411/tinycd/releases/download/v0.5.0/tinycd-aarch64-apple-darwin.tar.gz"
      sha256 "ea343724ffddac8794fb3a14804b3fb1113cae64c5749113c589e557ff038f00"
    end
    on_intel do
      url "https://github.com/maxz411/tinycd/releases/download/v0.5.0/tinycd-x86_64-apple-darwin.tar.gz"
      sha256 "386b840d7d84169b907b77a822d7f6fd52087bab1d0e1542952adce019c02bb4"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/maxz411/tinycd/releases/download/v0.5.0/tinycd-aarch64-unknown-linux-musl.tar.gz"
      sha256 "4e1f931bd20515ec2ff234fc1f9fb0f262ace1d36678a53ff3284d1c625bbf1a"
    end
    on_intel do
      url "https://github.com/maxz411/tinycd/releases/download/v0.5.0/tinycd-x86_64-unknown-linux-musl.tar.gz"
      sha256 "e219e250538b19c8d5ca9714950d01803f22e379574925a0235b935cbb3b0a83"
    end
  end

  def install
    bin.install "tinycd"
  end

  test do
    assert_match "tinycd", shell_output("#{bin}/tinycd --version")
  end
end
