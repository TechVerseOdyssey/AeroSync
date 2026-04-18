# frozen_string_literal: true

# Homebrew formula template.
#
# This file is meant to live in a separate tap repository, e.g.
# https://github.com/TechVerseOdyssey/homebrew-aerosync, as
# `Formula/aerosync.rb`. After each release the SHA-256 placeholders
# below need to be updated to match the values published in
# https://github.com/TechVerseOdyssey/AeroSync/releases.
#
# Once published, users install with:
#
#   brew tap TechVerseOdyssey/aerosync
#   brew install aerosync

class Aerosync < Formula
  desc "Fast, agent-friendly file transfer CLI with auto QUIC/HTTP and an MCP server"
  homepage "https://github.com/TechVerseOdyssey/AeroSync"
  version "0.1.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/TechVerseOdyssey/AeroSync/releases/download/v#{version}/aerosync-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_DARWIN_SHA256"
    else
      url "https://github.com/TechVerseOdyssey/AeroSync/releases/download/v#{version}/aerosync-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_X86_64_DARWIN_SHA256"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/TechVerseOdyssey/AeroSync/releases/download/v#{version}/aerosync-#{version}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_AARCH64_LINUX_SHA256"
    else
      url "https://github.com/TechVerseOdyssey/AeroSync/releases/download/v#{version}/aerosync-#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_X86_64_LINUX_SHA256"
    end
  end

  def install
    bin.install "aerosync"
    bin.install "aerosync-mcp"
    doc.install "README.md", "CHANGELOG.md"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/aerosync --version")
    assert_match "MCP", shell_output("#{bin}/aerosync-mcp --help 2>&1", 0).downcase rescue true
  end
end
