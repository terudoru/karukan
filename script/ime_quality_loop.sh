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
    cmp -s \
      "$REPO_ROOT/karukan-macos/out/Karukan.app/Contents/MacOS/KarukanIME" \
      "$HOME/Library/Input Methods/Karukan.app/Contents/MacOS/KarukanIME"
    cmp -s \
      "$REPO_ROOT/karukan-macos/out/Karukan.app/Contents/MacOS/karukan-imserver" \
      "$HOME/Library/Input Methods/Karukan.app/Contents/MacOS/karukan-imserver"
    codesign --verify --deep --strict \
      "$HOME/Library/Input Methods/Karukan.app"
    plutil -lint \
      "$HOME/Library/Input Methods/Karukan.app/Contents/Info.plist"
    open -n "$HOME/Library/Input Methods/Karukan.app"
    sleep 1
    pgrep -x KarukanIME >/dev/null
    pgrep -x karukan-imserver >/dev/null

    INPUT_SOURCE_SCRIPT="$REPO_ROOT/karukan-macos/scripts/select_input_source.swift"
    PREVIOUS_INPUT_SOURCE="$("$INPUT_SOURCE_SCRIPT" --current)"
    restore_input_source() {
      if [[ -n "$PREVIOUS_INPUT_SOURCE" ]]; then
        "$INPUT_SOURCE_SCRIPT" "$PREVIOUS_INPUT_SOURCE" >/dev/null || true
      fi
    }
    trap restore_input_source EXIT

    "$INPUT_SOURCE_SCRIPT" >/dev/null
    SELECTED_INPUT_SOURCE="$("$INPUT_SOURCE_SCRIPT" --current)"
    [[ "$SELECTED_INPUT_SOURCE" == "dev.togatoga.inputmethod.Karukan.Japanese" ]]

    restore_input_source
    trap - EXIT
    ;;
  *)
    echo "usage: $0 [--check|--install]" >&2
    exit 2
    ;;
esac
