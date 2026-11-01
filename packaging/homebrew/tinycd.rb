class Tinycd < Formula
  desc "Tiny self-hosted deployments with a dashboard and a GitHub App"
  homepage "https://github.com/maxz411/tinycd"
  version "0.4.1"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/maxz411/tinycd/releases/download/v0.4.1/tinycd-aarch64-apple-darwin.tar.gz"
      sha256 "61675259fb908aa9f8d89275d45c8218eb6d198d78cf7248ce143fe5a1748247"
    end
    on_intel do
      url "https://github.com/maxz411/tinycd/releases/download/v0.4.1/tinycd-x86_64-apple-darwin.tar.gz"
      sha256 "0881a5317c7ec61ccfeb818fe3fff7812371d286b1d05a9e5871ad1aed277bf0"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/maxz411/tinycd/releases/download/v0.4.1/tinycd-aarch64-unknown-linux-musl.tar.gz"
      sha256 "273c09a8e128d9f163a407b7b69c6d26b185b5f0abf6e7f7cdb7350aa6db4b99"
    end
    on_intel do
      url "https://github.com/maxz411/tinycd/releases/download/v0.4.1/tinycd-x86_64-unknown-linux-musl.tar.gz"
      sha256 "82854652604634299a4a337a5d29128496ab9b6ad79d5b2d0c92428eb339e5e4"
    end
  end

  def install
    bin.install "tinycd"
  end

  test do
    assert_match "tinycd", shell_output("#{bin}/tinycd --version")
  end
end
