#!/usr/bin/env bash
# Build a binary that will run on the primary.
#
# The obvious approach - build on the ThinkPad and copy - does not work: its glibc is 2.41 and the
# target's is 2.39, so the binary links symbols the target does not have and dies on exec. Building
# inside Debian bookworm (glibc 2.36) produces one that runs on both, because glibc is forward
# compatible and not backward.
set -euo pipefail
IMAGE=${IMAGE:-rust:1-bookworm}
# The primary is x86_64; on an Apple Silicon Mac Docker would otherwise build arm64.
PLATFORM=${PLATFORM:-linux/amd64}
docker run --rm --platform "$PLATFORM" -v "$PWD":/src -w /src -e CARGO_TERM_COLOR=never "$IMAGE" \
  bash -c "cargo build --release --bin kittiwake"
ls -la target/release/kittiwake
echo
echo "glibc it needs (must be <= the target's):"
docker run --rm --platform "$PLATFORM" -v "$PWD":/src -w /src "$IMAGE" \
  bash -c "objdump -T target/release/kittiwake | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -3"
