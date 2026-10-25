#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
target_app="${HOME}/Library/Input Methods/MuuIME.app"

# Stop the running IME and disable its input sources first, so macOS is not
# left with an enabled source whose bundle has gone. The bundle then goes to
# the Trash, not rm: an uninstall can be undone by hand.
"${script_dir}/reload-ime.sh"
xcrun swift "${script_dir}/tis-control.swift" disable dev.muu.inputmethod.Muu

if [[ -e "${target_app}" ]]; then
  trash_dir="$(mktemp -d "${HOME}/.Trash/MuuIME-uninstall.XXXXXX")"
  mv "${target_app}" "${trash_dir}/MuuIME.app"
  echo "Moved MuuIME.app to ${trash_dir}; it can be recovered from Trash."
else
  echo "MuuIME.app is not installed."
fi
