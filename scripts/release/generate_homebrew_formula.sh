#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/release/generate_homebrew_formula.sh \
    --version <version> \
    --checksums <SHA256SUMS path> \
    --output <output formula path>

Example:
  scripts/release/generate_homebrew_formula.sh \
    --version 0.1.0 \
    --checksums dist/SHA256SUMS \
    --output dist/krust.rb
EOF
}

VERSION=""
CHECKSUMS=""
OUTPUT=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      VERSION="${2:-}"
      shift 2
      ;;
    --checksums)
      CHECKSUMS="${2:-}"
      shift 2
      ;;
    --output)
      OUTPUT="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ -z "${VERSION}" || -z "${CHECKSUMS}" || -z "${OUTPUT}" ]]; then
  echo "missing required arguments" >&2
  usage >&2
  exit 1
fi

if [[ ! -f "${CHECKSUMS}" ]]; then
  echo "checksums file not found: ${CHECKSUMS}" >&2
  exit 1
fi

sum_for() {
  local filename="$1"
  local sum
  sum="$(awk -v f="${filename}" '$2 == f {print $1}' "${CHECKSUMS}" | head -n1)"
  if [[ -z "${sum}" ]]; then
    echo "missing checksum for ${filename}" >&2
    exit 1
  fi
  echo "${sum}"
}

DARWIN_ARM64_TGZ="krust-aarch64-apple-darwin.tar.gz"
DARWIN_AMD64_TGZ="krust-x86_64-apple-darwin.tar.gz"
LINUX_AMD64_TGZ="krust-x86_64-unknown-linux-gnu.tar.gz"

DARWIN_ARM64_SHA="$(sum_for "${DARWIN_ARM64_TGZ}")"
DARWIN_AMD64_SHA="$(sum_for "${DARWIN_AMD64_TGZ}")"
LINUX_AMD64_SHA="$(sum_for "${LINUX_AMD64_TGZ}")"

mkdir -p "$(dirname "${OUTPUT}")"
cat > "${OUTPUT}" <<EOF
class Krust < Formula
  desc "Latency-first Kubernetes terminal navigator"
  homepage "https://github.com/ErfanY/krust"
  version "${VERSION}"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/ErfanY/krust/releases/download/v#{version}/${DARWIN_ARM64_TGZ}"
      sha256 "${DARWIN_ARM64_SHA}"
    else
      url "https://github.com/ErfanY/krust/releases/download/v#{version}/${DARWIN_AMD64_TGZ}"
      sha256 "${DARWIN_AMD64_SHA}"
    end
  end

  on_linux do
    url "https://github.com/ErfanY/krust/releases/download/v#{version}/${LINUX_AMD64_TGZ}"
    sha256 "${LINUX_AMD64_SHA}"
  end

  def install
    bin.install "krust"
  end

  test do
    assert_match "krust", shell_output("#{bin}/krust --help")
  end
end
EOF
