#!/usr/bin/env bash
# Launches vpm-login-api (builds it first if the binary is missing).
set -euo pipefail

usage() {
  cat <<'EOF'
Launches vpm-login-api (builds it first if the binary is missing).

Usage:
  ./launch.sh                 # run in foreground (Ctrl-C to stop)
  ./launch.sh --background    # run in background, console logs -> logs/api.log
  ./launch.sh --help

Configuration comes from environment variables or .env
(PORT, USE_TLS, TLS_CERT, TLS_KEY, ACCESS_LOG, RUST_LOG).
EOF
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
# The binary also loads .env itself via dotenvy; sourcing here keeps the
# banner below consistent with what will actually run.
if [[ -f .env ]]; then
  set -a
  # shellcheck disable=SC1091
  source .env
  set +a
fi

BIN="target/release/vpm-login-api"
if [[ ! -x "$BIN" ]]; then
  echo "==> Binary not found, running ./build.sh first ..."
  ./build.sh
fi

USE_TLS="${USE_TLS:-0}"
if [[ "$USE_TLS" == "1" || "$USE_TLS" =~ ^(true|yes)$ ]]; then
  MODE="HTTPS"
else
  MODE="HTTP"
fi
if [[ -z "${PORT:-}" ]]; then
  if [[ "$MODE" == "HTTPS" ]]; then PORT=2096; else PORT=2095; fi
fi

echo "==> Launching vpm-login-api ($MODE on port $PORT)"
mkdir -p logs

if [[ "$BACKGROUND" == "1" ]]; then
  LOG="logs/api.log"
  nohup "$BIN" >>"$LOG" 2>&1 &
  echo "==> Started in background (pid $!) — console logs: $LOG"
  echo "==> Access log (with client IPs): logs/access.log"
else
  exec "$BIN"
fi
