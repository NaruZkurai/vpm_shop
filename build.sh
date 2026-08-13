#!/usr/bin/env bash
# Builds vpm-login-api.
set -euo pipefail

usage() {
  cat <<'EOF'
Builds vpm-login-api.

Usage:
  ./build.sh            # release build (default)
  ./build.sh --debug    # debug build
  ./build.sh --help
EOF
}

cd "$(dirname "$0")"

MODE="release"
case "${1:-}" in
  "" ) ;;
  --debug ) MODE="debug" ;;
  -h|--help ) usage; exit 0 ;;
  * )
    echo "Unknown option: $1" >&2
    usage >&2
    exit 1
    ;;
esac

echo "==> Building vpm-login-api ($MODE) ..."
if [[ "$MODE" == "debug" ]]; then
  cargo build
else
  cargo build --release
fi

echo "==> Build complete: target/${MODE}/vpm-login-api"
