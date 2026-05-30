#!/bin/sh
# Build the cross-compiled spritebox-fsd Linux binary that gets
# embedded into the spritebox CLI by build.rs.
#
# - Linux host: builds natively for x86_64-unknown-linux-gnu (the sprite
#   architecture). Requires `rustup target add x86_64-unknown-linux-gnu`
#   the first time.
# - macOS host: builds inside a Linux Docker container (rust:1, amd64).
#   Requires Docker Desktop or OrbStack running.
# - Other hosts: not supported. Use a release build of spritebox or
#   build on Linux.
#
# Output lands at:
#   target/x86_64-unknown-linux-gnu/release/spritebox-fsd
#
# After running this script, `cargo build` (or cargo run) on the
# spritebox CLI will pick up the daemon and embed it.

set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

OS="$(uname -s)"
TARGET="x86_64-unknown-linux-gnu"

case "$OS" in
  Linux)
    if ! rustup target list --installed 2>/dev/null | grep -q "^$TARGET\$"; then
      echo "==> installing rust target $TARGET"
      rustup target add "$TARGET"
    fi
    echo "==> building spritebox-fsd for $TARGET (native)"
    cargo build --release -p spritebox-fsd --target "$TARGET"
    ;;
  Darwin)
    if ! command -v docker >/dev/null 2>&1; then
      echo "error: docker not found." >&2
      echo "Install Docker Desktop or OrbStack and try again." >&2
      exit 1
    fi
    if ! docker info >/dev/null 2>&1; then
      echo "error: docker daemon not running." >&2
      echo "Start Docker Desktop or OrbStack and try again." >&2
      exit 1
    fi
    echo "==> building spritebox-fsd for $TARGET (via Docker)"
    docker run --rm --platform linux/amd64 \
      -v "$ROOT":/work -w /work \
      rust:1 cargo build --release -p spritebox-fsd --target "$TARGET"
    ;;
  *)
    echo "error: unsupported host OS: $OS" >&2
    echo "the daemon must be built for x86_64-linux." >&2
    echo "use the prebuilt release of spritebox or build on Linux." >&2
    exit 1
    ;;
esac

OUTPUT="target/$TARGET/release/spritebox-fsd"
if [ ! -f "$OUTPUT" ]; then
  echo "error: build succeeded but $OUTPUT not found" >&2
  exit 1
fi

SIZE="$(wc -c < "$OUTPUT" | tr -d ' ')"
echo
echo "==> built: $OUTPUT ($SIZE bytes)"
echo "==> next: cargo build (build.rs will embed the daemon)"
