#!/usr/bin/env bash
# Plan 0330 spike: download the three dictionaries into resources/ (gitignored).
# Every URL and license-file path is recorded in plans/0330-text-analyzer-spike.md.
#
# Usage: ./fetch_resources.sh
set -euo pipefail
cd "$(dirname "$0")/resources"
mkdir -p vibrato sudachi lindera

# ── Candidate B: vibrato 0.5.2 + ipadic-mecab 2.7.0 (compiled system.dic.zst) ────────
# URL: https://github.com/daac-tools/vibrato/releases/download/v0.5.0/ipadic-mecab-2_7_0.tar.xz
# License files inside: COPYING (mecab-ipadic BSD + acknowledgment), NOTICE.
# (The ipadic asset lives on the v0.5.0 release; v0.5.2 has no assets.)
if [ ! -f vibrato/ipadic-mecab-2_7_0/system.dic.zst ]; then
  curl -sL https://github.com/daac-tools/vibrato/releases/download/v0.5.0/ipadic-mecab-2_7_0.tar.xz \
    -o /tmp/ipadic-mecab-2_7_0.tar.xz
  tar xf /tmp/ipadic-mecab-2_7_0.tar.xz -C vibrato --strip-components=0
  rm -f /tmp/ipadic-mecab-2_7_0.tar.xz
fi

# ── Candidate C: SudachiDict small (raw binary system_small.dic) ─────────────────────
# URL: https://github.com/WorksApplications/SudachiDict/releases/download/v20260428/sudachi-dictionary-20260428-small.zip
# License files inside: LEGAL, LICENSE-2.0.txt (Apache-2.0).
if [ ! -f sudachi/system_small.dic ]; then
  curl -sL https://github.com/WorksApplications/SudachiDict/releases/download/v20260428/sudachi-dictionary-20260428-small.zip \
    -o /tmp/sudachi-small.zip
  unzip -o -q /tmp/sudachi-small.zip -d /tmp/sudachi-dict
  cp /tmp/sudachi-dict/sudachi-dictionary-20260428/system_small.dic sudachi/
  cp /tmp/sudachi-dict/sudachi-dictionary-20260428/LEGAL sudachi/
  cp /tmp/sudachi-dict/sudachi-dictionary-20260428/LICENSE-2.0.txt sudachi/
  rm -rf /tmp/sudachi-small.zip /tmp/sudachi-dict
fi

# ── Candidate D: lindera 6.0 + ipadic prebuilt dictionary directory ──────────────────
# URL: https://github.com/lindera/lindera/releases/download/v6.0.0/lindera-ipadic-6.0.0.zip
# License file inside: NOTICE.txt. (The wasm build additionally fetches
# https://Lindera.dev/mecab-ipadic-2.7.0-20250920.tar.gz at BUILD time via
# lindera-ipadic's build.rs — md5 a95c409f12f1023fce8ef91f991ef042.)
if [ ! -d lindera/lindera-ipadic ]; then
  curl -sL https://github.com/lindera/lindera/releases/download/v6.0.0/lindera-ipadic-6.0.0.zip \
    -o /tmp/lindera-ipadic.zip
  unzip -o -q /tmp/lindera-ipadic.zip -d lindera
  rm -f /tmp/lindera-ipadic.zip
fi

echo "resources ready:"; du -sh vibrato sudachi lindera