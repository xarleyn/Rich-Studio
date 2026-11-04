#!/usr/bin/env bash
# Browser checks on a stand of their own: the C# fake Telegram and a Rust studio with an empty DATA_DIR
# on free ports, so a working studio on 7424 keeps running and never sees the test posts.
# Usage: make e2e-local [ONLY=<part of a check file name>]
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
cargo build --locked --bin rich-studio
E2E_ONLY="${1:-}" node scripts/check-rust-browser.mjs
