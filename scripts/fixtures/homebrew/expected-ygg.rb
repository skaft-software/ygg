# Generated from verified immutable Ygg release metadata.
# Release tag: v0.6.6
# Release source commit: 0123456789abcdef0123456789abcdef01234567
# Release workflow commit: abcdef0123456789abcdef0123456789abcdef01
# Release workflow ref: skaft-software/ygg/.github/workflows/release-ygg.yml@refs/tags/ygg-binaries-v0.6.6
# YGG_SHA256SUMS SHA-256: a78cb3e2d30a54022e80e282c87222377650242a01d0c5516d119ba08b1d9796
class Ygg < Formula
  desc "High-performance coding agent"
  homepage "https://github.com/skaft-software/ygg"
  version "0.6.6"
  depends_on :macos
  depends_on "ripgrep"

  on_arm do
    url "https://github.com/skaft-software/ygg/releases/download/v0.6.6/ygg-0.6.6-aarch64-apple-darwin.tar.gz"
    sha256 "09be15cc7ace2cb887232033c8c443ae2b6dfae778b6213f7c313d79084745d5"
  end

  on_intel do
    url "https://github.com/skaft-software/ygg/releases/download/v0.6.6/ygg-0.6.6-x86_64-apple-darwin.tar.gz"
    sha256 "6aa6d23a60f0fbca903ee03349967eab0267f6991e98e895732e72cac15f140d"
  end

  def install
    root = Dir["ygg-*/"].find { |candidate| File.executable?(File.join(candidate, "ygg")) }
    odie "Ygg release archive has no executable ygg binary" unless root
    bin.install File.join(root, "ygg")
    bin.install File.join(root, "ygg-host")
  end

  test do
    assert_match "ygg #{version}", shell_output("#{bin}/ygg --version")
  end
end
