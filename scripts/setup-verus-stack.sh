#!/bin/sh
# Provision the pinned Verus release used by the Gleaph verification leaf.
#
# The pin mirrors verus-ic v0.1.0 (release 0.2026.10.18.7d82994). Unlike the
# verus-ic script, this copy carries the Linux x86_64 asset as well, because the
# Gleaph CI job runs on ubuntu-latest. Update both files together when the pin
# moves; the release assets, sizes and SHA-256 values are the source of truth
# here (GitHub release metadata).
set -eu
root=$(CDPATH= cd -P -- "$(dirname -- "$0")/.." && pwd)
rel="0.2026.10.18.7d82994"
base="https://github.com/verus-lang/verus/releases/download/release/${rel}"
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)
    asset="verus-${rel}-arm64-macos.zip"
    size="451327093"
    sha256="3fc56c58277b5f0999368f7b3a2b07ba4eb9ec2965454ce7a9f2769c0464d32b"
    dirname="verus-arm64-macos"
    ;;
  Linux-x86_64)
    asset="verus-${rel}-x86-linux.zip"
    size="486844228"
    sha256="e9af03732d4bdd32b93a8fcf471706739c24c1473fa5086618d96c47237845dd"
    dirname="verus-x86-linux"
    ;;
  *)
    echo "setup-verus-stack: no pinned asset for host $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

stack="$root/.tools/verus-${rel}"
downloads="$root/.tools/downloads"

if [ -x "$stack/verus" ] && [ -x "$stack/cargo-verus" ]; then
  echo "pinned Verus stack already provisioned: $stack"
else
  mkdir -p "$downloads"
  archive="$downloads/$asset"
  if [ ! -f "$archive" ]; then
    echo "downloading $base/$asset"
    curl -L --fail --retry 3 --retry-delay 5 -o "$archive" "$base/$asset"
  fi
  actual_size=$(wc -c < "$archive" | tr -d ' ')
  [ "$actual_size" = "$size" ] || { echo "size mismatch: $actual_size != $size" >&2; exit 1; }
  if command -v sha256sum >/dev/null 2>&1; then
    actual_sha=$(sha256sum "$archive" | cut -d' ' -f1)
  else
    actual_sha=$(shasum -a 256 "$archive" | cut -d' ' -f1)
  fi
  [ "$actual_sha" = "$sha256" ] || { echo "sha256 mismatch: $actual_sha != $sha256" >&2; exit 1; }
  tmp="$root/.tools/.extract-verus-$$"
  trap 'rm -rf "$tmp"' EXIT INT TERM
  mkdir -p "$tmp"
  unzip -q "$archive" -d "$tmp"
  [ -x "$tmp/$dirname/verus" ] || { echo "unexpected archive layout" >&2; exit 1; }
  rm -rf "$stack"
  mv "$tmp/$dirname" "$stack"
  echo "provisioned $stack"
fi

# The release is built against Rust 1.99.1; cargo-verus runs the verifier with
# that toolchain selected by scripts/verus-0.2026.10.18.
if command -v rustup >/dev/null 2>&1; then
  if ! rustup toolchain list 2>/dev/null | grep -q '^1\.99\.1'; then
    rustup toolchain install 1.99.1 --profile minimal
  fi
fi
