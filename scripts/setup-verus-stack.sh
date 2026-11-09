#!/bin/sh
# Provision the pinned Verus release used by the Verus intake harness (plan 0370).
# Reproducible: pinned release, pinned asset, pinned size and SHA-256.
set -eu
root=$(CDPATH= cd -P -- "$(dirname -- "$0")/.." && pwd)
rel="0.2026.10.18.7d82994"
asset="verus-${rel}-arm64-macos.zip"
url="https://github.com/verus-lang/verus/releases/download/release/${rel}/${asset}"
size="451327093"
sha256="3fc56c58277b5f0999368f7b3a2b07ba4eb9ec2965454ce7a9f2769c0464d32b"
stack="$root/.tools/verus-${rel}"
downloads="$root/.tools/downloads"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) ;;
  *)
    echo "setup-verus-stack: pinned asset is arm64 macOS; host is $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

if [ -x "$stack/verus" ] && [ -x "$stack/cargo-verus" ]; then
  echo "pinned Verus stack already provisioned: $stack"
  exit 0
fi

mkdir -p "$downloads"
archive="$downloads/$asset"
if [ ! -f "$archive" ]; then
  echo "downloading $url"
  curl -L --fail --retry 3 --retry-delay 5 -o "$archive" "$url"
fi

actual_size=$(wc -c < "$archive" | tr -d ' ')
[ "$actual_size" = "$size" ] || { echo "size mismatch: $actual_size != $size" >&2; exit 1; }
actual_sha=$(shasum -a 256 "$archive" | cut -d' ' -f1)
[ "$actual_sha" = "$sha256" ] || { echo "sha256 mismatch: $actual_sha" >&2; exit 1; }

tmp="$root/.tools/.extract-verus-$$"
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir -p "$tmp"
unzip -q "$archive" -d "$tmp"
[ -x "$tmp/verus-arm64-macos/verus" ] || { echo "unexpected archive layout" >&2; exit 1; }
rm -rf "$stack"
mv "$tmp/verus-arm64-macos" "$stack"
echo "provisioned $stack"
