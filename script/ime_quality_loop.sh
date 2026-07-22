#!/usr/bin/env bash
set -euo pipefail

MODE="${1:---check}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$REPO_ROOT"
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

cd "$REPO_ROOT/karukan-macos"
swift test
swift test --sanitize=thread --filter TransportTests

case "$MODE" in
  --check|check)
    ;;
  --install|install)
    make install
    codesign --verify --deep --strict \
      "$HOME/Library/Input Methods/Karukan.app"
    plutil -lint \
      "$HOME/Library/Input Methods/Karukan.app/Contents/Info.plist"
    open -n "$HOME/Library/Input Methods/Karukan.app"
    sleep 1
    pgrep -x KarukanIME >/dev/null
    pgrep -x karukan-imserver >/dev/null
    ;;
  *)
    echo "usage: $0 [--check|--install]" >&2
    exit 2
    ;;
esac
