#!/usr/bin/env bash
# Launches vpm-upload-api (builds it first if the binary is missing).
set -euo pipefail

usage() {
  cat <<'HELP'
Launches vpm-upload-api (builds it first if the binary is missing).

Usage:
  ./launch.sh                 # run in foreground (Ctrl-C to stop)
  ./launch.sh --background    # run in background, console logs -> logs/api.log
  ./launch.sh --help

Configuration comes from vpm-shop.conf (copy vpm-shop.conf.example first)
or environment variables (VPM_SHOP, VALIDATOR, MASTER_HOST, BIND,
API_USER, API_PASS, PKG_AUTHOR, RUST_LOG).
HELP
}

cd "$(dirname "$0")"

# --- parse args -----------------------------------------------------------
BACKGROUND=0
case "${1:-}" in
  "" ) ;;
  -b|--background ) BACKGROUND=1 ;;
  -h|--help ) usage; exit 0 ;;
  * )
    echo "Unknown option: $1" >&2
    usage >&2
    exit 1
    ;;
esac

# --- config ----------------------------------------------------------------
if [[ -f vpm-shop.conf ]]; then
  set -a
  # shellcheck disable=SC1091
  source vpm-shop.conf
  set +a
fi

BIN="target/release/vpm-upload-api"
if [[ ! -x "$BIN" ]]; then
  echo "==> Binary not found, running ./build.sh first ..."
  ./build.sh
fi

BIND="${BIND:-0.0.0.0:55555}"
echo "==> Launching vpm-upload-api (listening on $BIND)"
mkdir -p logs

if [[ "$BACKGROUND" == "1" ]]; then
  LOG="logs/api.log"
  nohup "$BIN" >>"$LOG" 2>&1 &
  echo "==> Started in background (pid $!) — console logs: $LOG"
else
  exec "$BIN"
fi
