#!/usr/bin/env bash
set -euo pipefail
# Build the release binary for the current platform, stage it under
# npm/vendor/<platform>-<arch>/, and pack the npm bundle.
cd "$(dirname "${BASH_SOURCE[0]}")/.."

cargo build --release

PLATFORM="$(node -p 'process.platform')"
ARCH="$(node -p 'process.arch')"
BINARY="target/release/dsh-whale-tui"
if [[ "$PLATFORM" == "win32" ]]; then
  BINARY="${BINARY}.exe"
fi

node scripts/package-native.mjs stage \
  --source "$BINARY" \
  --platform "$PLATFORM" \
  --arch "$ARCH" \
  --vendor-root npm/vendor

mkdir -p dist
(cd npm && npm pack --pack-destination ../dist)
echo "tgz written to: $(ls -1 dist/*.tgz | tail -n 1)"
