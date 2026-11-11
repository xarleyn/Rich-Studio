#!/usr/bin/env bash
# The Ubuntu runner usually has the libraries needed by the pinned headless browser.
set -euo pipefail

probe() {
  node -e "require('playwright').chromium.launch().then(browser => browser.close())"
}

npx playwright install chromium
if output=$(probe 2>&1); then
  exit 0
fi
printf '%s\n' "$output" >&2
if ! grep -Eiq 'missing dependencies|error while loading shared libraries|cannot open shared object file' <<<"$output"; then
  exit 1
fi

npx playwright install --with-deps chromium
probe
