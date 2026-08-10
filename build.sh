#!/usr/bin/env bash
# ponytail: minimal build, with an optional local TUI launch
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT_DIR"

# Parse flags. We support three build modes:
#   --with-web   force building the `native-browser` feature (requires
#                WebKitGTK system headers on Linux).
#   --no-web     skip `native-browser`; build the TUI-only core. This is the
#                right mode on headless servers and CI.
#   (none)       auto-detect: enable on macOS/Windows (system WKWebView /
#                WebView2); on Linux probe pkg-config for gio-2.0.
# Plus --run [args] to launch the freshly-built TUI when the build succeeds.
WITH_WEB="auto"
RUN_TUI=false
RUN_ARGS=()
print_help() {
  cat <<EOF
usage: $(basename "$0") [--with-web | --no-web] [--run [TUI_ARGS...]]

Builds the release core and Go TUI, then replaces the current catcode
installation when one is on PATH.

  --with-web   build the \`native-browser\` feature (Linux: requires
               libgtk-3-dev, libwebkit2gtk-4.1-dev, libgio-2.0-dev)
  --no-web     skip \`native-browser\`; build the TUI-only core
  (default)    auto-detect: macOS/Windows always; Linux via
               \`pkg-config --exists gio-2.0\`
  --run [...]  after building, exec the freshly-built TUI
EOF
}
while [[ $# -gt 0 ]]; do
  case "$1" in
    --with-web) WITH_WEB="yes" ;;
    --no-web)   WITH_WEB="no"  ;;
    --run)      RUN_TUI=true; shift; RUN_ARGS=("$@"); break ;;
    -h|--help)  print_help; exit 0 ;;
    *)
      printf 'error: unknown option %s\n' "$1" >&2
      print_help >&2
      exit 2
      ;;
  esac
  shift
done

# Resolve "auto" in a platform-aware way:
#   - Darwin / Windows (MINGW/MSYS/CYGWIN): system WKWebView / WebView2 — no
#     extra packages, so enable native-browser by default.
#   - Linux (and anything else): probe for WebKitGTK via gio-2.0 on pkg-config;
#     headless hosts without GTK skip with a one-line notice.
if [[ "$WITH_WEB" == "auto" ]]; then
  case "$(uname -s 2>/dev/null || echo unknown)" in
    Darwin|MINGW*|MSYS*|CYGWIN*)
      WITH_WEB="yes"
      ;;
    *)
      if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists gio-2.0; then
        WITH_WEB="yes"
      else
        WITH_WEB="no"
        echo "notice: WebKitGTK system headers not found via pkg-config; skipping native-browser (pass --with-web once you've installed them)"
      fi
      ;;
  esac
fi

if [[ "$WITH_WEB" == "yes" ]]; then
  echo "[1/3] building core (cargo, native-browser, -j$(nproc))..."
  cargo build --release -j"$(nproc)" --features native-browser --manifest-path core/Cargo.toml
else
  echo "[1/3] building core (cargo, TUI-only, -j$(nproc); native-browser skipped)..."
  cargo build --release -j"$(nproc)" --manifest-path core/Cargo.toml
fi

echo "[2/3] building tui (go)..."
( cd tui && go build -o tui . )

LOCAL_CORE="$ROOT_DIR/core/target/release/core"
INSTALLED_TUI="$(type -P catcode || true)"

# Replace the binaries used by the current installation, rather than leaving
# the freshly-built artifacts stranded in the repository. The TUI and core
# must be updated together: a source-built TUI can speak protocol changes that
# an older installed core may not understand.
install_binary() {
  local source="$1" destination="$2"
  local destination_dir
  destination_dir="$(dirname "$destination")"
  if [[ $EUID -eq 0 || -w "$destination_dir" ]]; then
    install -m 0755 "$source" "$destination"
  elif command -v sudo >/dev/null 2>&1; then
    sudo install -m 0755 "$source" "$destination"
  else
    echo "error: cannot replace $destination (directory is not writable and sudo is unavailable)" >&2
    return 1
  fi
}

if [[ -n "$INSTALLED_TUI" ]]; then
  echo "[3/3] replacing installed catcode"
  INSTALLED_DIR="$(dirname "$INSTALLED_TUI")"
  case "$INSTALLED_TUI" in
    *.exe|*.EXE) INSTALLED_CORE="$INSTALLED_DIR/catcode-core.exe" ;;
    *)           INSTALLED_CORE="$INSTALLED_DIR/catcode-core"     ;;
  esac
  echo "installing tui -> $INSTALLED_TUI"
  install_binary "$ROOT_DIR/tui/tui" "$INSTALLED_TUI"
  echo "installing core -> $INSTALLED_CORE"
  install_binary "$LOCAL_CORE" "$INSTALLED_CORE"
else
  echo "[3/3] installed catcode not found on PATH; skipping installation"
fi

echo "done: core -> core/target/release/core, tui -> tui/tui"

if [[ -n "${CATCODE_CORE:-}" && "$CATCODE_CORE" != "$LOCAL_CORE" ]]; then
  echo "warning: CATCODE_CORE=$CATCODE_CORE overrides this source build"
  echo "         run locally with: CATCODE_CORE=$LOCAL_CORE $ROOT_DIR/tui/tui"
fi

if $RUN_TUI; then
  echo "starting local TUI (core=$LOCAL_CORE)"
  exec env CATCODE_CORE="$LOCAL_CORE" "$ROOT_DIR/tui/tui" "${RUN_ARGS[@]}"
fi
