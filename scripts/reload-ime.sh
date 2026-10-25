#!/usr/bin/env bash
set -euo pipefail

# Scope termination to the current user; another user's input session is never touched.
killall -u "$(id -un)" MuuIME 2>/dev/null || true
