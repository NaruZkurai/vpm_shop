#!/usr/bin/env bash
# Builds vpm-upload-api (VPM shop upload API).
set -euo pipefail

usage() {
  cat <<'EOF'
Builds vpm-upload-api.

Usage:
  ./build.sh            # release build (default)
  ./build.sh --debug    # debug build
  ./build.sh --help
