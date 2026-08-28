#!/usr/bin/env bash
# One-command bring-up of the knowledge demo, delegating every step to the Gleaph CLI.
#
# This is thin orchestration, not a bootstrap: each line is the CLI's own idempotent
# surface (network start reuses a running network, migration apply replays applied
# migrations, load resumes its durable job, grants apply upserts), so the script is safe
# to re-run and stops at the first failure. What each step does is documented in the
# README Quickstart.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

echo "== identity =="
gleaph identity new dev 2>/dev/null || echo "identity dev already exists; reusing"
gleaph login

echo "== network =="
gleaph network start

echo "== migrations =="
gleaph migration apply

echo "== data =="
gleaph load seeds/vertices.jsonl seeds/edges.jsonl --graph knowledge
node scripts/gen-embeddings.mjs
gleaph embed ingest \
  --vertices seeds/vertices.jsonl \
  --embeddings seeds/embeddings.jsonl \
  --graph knowledge

echo "== prepared ops + grant policy =="
gleaph prepared apply
gleaph grants apply

echo "== client =="
gleaph codegen

echo
echo "bring-up complete — start the browser host with:  pnpm dev"
echo "(then open http://localhost:3000/ — the page runs all four scenarios as an anonymous visitor)"