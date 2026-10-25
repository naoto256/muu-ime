#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
input_methods_dir="${HOME}/Library/Input Methods"
target_app="${input_methods_dir}/MuuIME.app"

usage() {
  cat >&2 <<'USAGE'
usage: install-dev.sh \
  --source-app <reviewed-signed-MuuIME.app> \
  --expected-commit <full-sha> \
  --expected-build <positive-build-number> \
  --expected-variant <standard|dogfood> \
  --expected-executable-sha256-file <path> \
  --expected-codesign-json <path>
USAGE
}

source_app=""
expected_commit=""
expected_build=""
expected_variant=""
expected_executable_sha256_file=""
expected_codesign_json=""
while (($# > 0)); do
  case "$1" in
    --source-app) source_app="${2:-}"; shift 2 ;;
    --expected-commit) expected_commit="${2:-}"; shift 2 ;;
    --expected-build) expected_build="${2:-}"; shift 2 ;;
    --expected-variant) expected_variant="${2:-}"; shift 2 ;;
    --expected-executable-sha256-file)
      expected_executable_sha256_file="${2:-}"
      shift 2
      ;;
    --expected-codesign-json) expected_codesign_json="${2:-}"; shift 2 ;;
    *) usage; exit 64 ;;
  esac
done

if [[ -z "${source_app}" || -z "${expected_commit}" || -z "${expected_build}" ||
      -z "${expected_variant}" || -z "${expected_executable_sha256_file}" ||
      -z "${expected_codesign_json}" ]]; then
  usage
  exit 64
fi

# Reject links before verification, reload or mkdir can change the destination
# or integration state. Moving a relative link into the stage could make it
# dangling and hide the only previous app from a target-existence check.
# Recovery would then skip it, and deleting the stage would delete the link,
# so the user's previous installation would be lost. Every ancestor is checked
# too, which also refuses a HOME that is reached through a symbolic link.
install_path="${target_app}"
if [[ "${install_path}" != /* ]]; then
  echo "Install destination must be an absolute path." >&2
  exit 1
fi
while true; do
  if [[ -L "${install_path}" ]]; then
    echo "Install destination must not contain a symbolic link: ${install_path}" >&2
    exit 1
  fi
  if [[ "${install_path}" == / ]]; then break; fi
  install_path="$(dirname "${install_path}")"
done

if [[ ! -d "${source_app}" || ! -f "${expected_executable_sha256_file}" ||
      ! -f "${expected_codesign_json}" ]]; then
  echo "Reviewed artifact or authority input is missing." >&2
  exit 1
fi
# The operator names the variant; nothing here infers it from the bundle.
# The artifact check below then requires the plist to match it exactly.
if [[ "${expected_variant}" != standard && "${expected_variant}" != dogfood ]]; then
  echo "Unknown reviewed build variant: ${expected_variant}" >&2
  exit 1
fi
expected_executable_sha256="$(
  awk 'NR == 1 { print $1 }' "${expected_executable_sha256_file}"
)"
if [[ ! "${expected_executable_sha256}" =~ ^[0-9a-fA-F]{64}$ ]]; then
  echo "Reviewed executable SHA-256 authority is malformed." >&2
  exit 1
fi

executable_sha256() {
  local app="$1"
  local executable_name
  executable_name="$(plutil -extract CFBundleExecutable raw "${app}/Contents/Info.plist")"
  shasum -a 256 "${app}/Contents/MacOS/${executable_name}" | awk '{print $1}'
}

signature_evidence() {
  local app="$1"
  xcrun swift "${script_dir}/audit-installed-mode-icons.swift" signature "${app}"
}

verify_artifact_authority() {
  local app="$1"
  local output="$2"
  xcrun swift "${script_dir}/audit-installed-mode-icons.swift" \
    verify-artifact \
    "${app}" \
    "${expected_commit}" \
    "${expected_build}" \
    "${expected_variant}" \
    "${expected_executable_sha256}" \
    "${expected_codesign_json}" \
    > "${output}"
}

# The build output is the reviewed signed authority. Installation may copy it
# to a fresh inode, but must never create different signed bytes at the target.
codesign --verify --deep --strict --verbose=2 "${source_app}"
source_executable_sha256="$(executable_sha256 "${source_app}")"
if [[ "${source_executable_sha256}" != "${expected_executable_sha256}" ]]; then
  echo "Source executable differs from the reviewed SHA-256 authority." >&2
  exit 1
fi

"${script_dir}/reload-ime.sh"
mkdir -p "${input_methods_dir}"

# Keep renames on the destination filesystem, including the recovery copy.
# A failed recovery must not leave the only previous app in a disposable /tmp.
# Same-filesystem renames are atomic, so each mv below either moved a whole
# bundle or left it where it was.
#
# What remains if a step fails (cleanup_install runs on exit, HUP, INT, TERM;
# nothing runs on SIGKILL):
# - Any check before the backup mv: the previous app is untouched at the target.
# - Backup mv of the previous app: it is still at the target, and cleanup
#   leaves it there (replacement_started is still 0).
# - mv of the new app into place: the previous app is restored from the stage.
# - A check of the installed copy: that copy is moved aside into the stage and
#   the previous app is restored. With no previous app (a fresh install) the
#   unverified copy is still moved into the stage and deleted with it, so the
#   target returns to absent.
# - TIS registration: the installed copy is already verified and stays; the
#   previous app in the stage is deleted with it.
# - A failed recovery rename: the stage is kept, and stderr names the path of
#   the intact previous app for manual recovery.
stage_dir="$(mktemp -d "${input_methods_dir}/.muu-ime-install.XXXXXX")"
stage_app="${stage_dir}/MuuIME.app"
previous_app="${stage_dir}/previous-MuuIME.app"
replacement_started=0
replacement_verified=0

cleanup_install() {
  local status="$?"
  local keep_stage=0
  trap - EXIT HUP INT TERM
  if ((replacement_verified == 0)); then
    # Never delete either app to make room for restoration. If a rename fails,
    # retain the complete previous bundle and report its exact recovery path.
    # The unverified new copy goes to rejected-MuuIME.app in the stage first,
    # whether or not a previous app exists. Do not quarantine the old target
    # when its backup mv failed before replacement began.
    if ((replacement_started != 0)) && [[ -e "${target_app}" || -L "${target_app}" ]]; then
      if ! mv "${target_app}" "${stage_dir}/rejected-MuuIME.app"; then
        keep_stage=1
        echo "Recovery could not move the unverified app aside from ${target_app}." >&2
      fi
    fi
    if [[ -e "${previous_app}" || -L "${previous_app}" ]]; then
      if ((keep_stage == 0)); then
        if ! mv "${previous_app}" "${target_app}"; then
          keep_stage=1
        fi
      fi
      if ((keep_stage != 0)); then
        echo "Recovery could not restore the previous app; preserved at ${previous_app}." >&2
      fi
    fi
    if ((keep_stage != 0)); then
      if ((status == 0)); then status=1; fi
    fi
  fi
  # On success this also deletes the previous app (and any rejected copy);
  # a kept stage is the only case where the previous bundle survives here.
  if ((keep_stage == 0)); then
    if ! rm -rf "${stage_dir}"; then
      echo "Cleanup failed; staged files remain at ${stage_dir}." >&2
      if ((status == 0)); then status=1; fi
    fi
  fi
  exit "${status}"
}

trap cleanup_install EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
source_authority_json="${stage_dir}/source-authority.json"
stage_authority_json="${stage_dir}/stage-authority.json"
installed_authority_json="${stage_dir}/installed-authority.json"
source_signature_json="${stage_dir}/source-codesign.json"
stage_signature_json="${stage_dir}/stage-codesign.json"
installed_signature_json="${stage_dir}/installed-codesign.json"
verify_artifact_authority "${source_app}" "${source_authority_json}"
signature_evidence "${source_app}" > "${source_signature_json}"
if ! cmp -s "${expected_codesign_json}" "${source_signature_json}"; then
  echo "Source code-sign JSON differs from the reviewed authority." >&2
  exit 1
fi

# A fresh copy avoids replacing files in place while the input method host may
# still have executable mappings from the previous bundle.
ditto "${source_app}" "${stage_app}"
stage_executable_sha256="$(executable_sha256 "${stage_app}")"
if [[ "${expected_executable_sha256}" != "${stage_executable_sha256}" ]]; then
  echo "Staged executable differs from the reviewed source artifact." >&2
  exit 1
fi
codesign --verify --deep --strict --verbose=2 "${stage_app}"
verify_artifact_authority "${stage_app}" "${stage_authority_json}"
signature_evidence "${stage_app}" > "${stage_signature_json}"
if ! cmp -s "${expected_codesign_json}" "${stage_signature_json}"; then
  echo "Staged code-sign evidence differs from the reviewed source artifact." >&2
  exit 1
fi

if [[ -e "${target_app}" || -L "${target_app}" ]]; then
  mv "${target_app}" "${previous_app}"
fi
replacement_started=1
mv "${stage_app}" "${target_app}"

installed_executable_sha256="$(executable_sha256 "${target_app}")"
if [[ "${expected_executable_sha256}" != "${installed_executable_sha256}" ]]; then
  echo "Installed executable differs from the reviewed source artifact." >&2
  exit 1
fi
codesign --verify --deep --strict --verbose=2 "${target_app}"
verify_artifact_authority "${target_app}" "${installed_authority_json}"
signature_evidence "${target_app}" > "${installed_signature_json}"
if ! cmp -s "${expected_codesign_json}" "${installed_signature_json}"; then
  echo "Installed code-sign evidence differs from the reviewed source artifact." >&2
  exit 1
fi

bundle_identifier="$(plutil -extract CFBundleIdentifier raw "${target_app}/Contents/Info.plist")"
# Once the installed copy has passed every authority check, a later TIS failure
# can leave that valid app in place. Before this point, restore the previous one.
replacement_verified=1
materialized=0
max_registration_attempts=10
for ((attempt = 1; attempt <= max_registration_attempts; attempt++)); do
  if xcrun swift "${script_dir}/tis-control.swift" register "${target_app}" && \
    xcrun swift "${script_dir}/tis-control.swift" migrate-two-mode "${bundle_identifier}" && \
    xcrun swift "${script_dir}/tis-control.swift" verify-two-mode "${bundle_identifier}"; then
    materialized=1
    echo "Two-mode input source materialized after registration attempt ${attempt}."
    break
  fi

  if ((attempt < max_registration_attempts)); then
    echo "Input source not materialized; retrying registration in 1 second (${attempt}/${max_registration_attempts})." >&2
    sleep 1
  fi
done

if ((materialized == 0)); then
  echo "TIS registration did not materialize the two-mode input source or retire legacy modes for ${bundle_identifier} after ${max_registration_attempts} attempts." >&2
  exit 1
fi

echo "Installed ${target_app}"
echo "Executable SHA-256: ${installed_executable_sha256}"
cat "${installed_signature_json}"
echo "Build variant: ${expected_variant}"
echo "Preserve the pre-login public snapshot and reviewed authority."
echo "Log out and log back in before accepting the installed input source UI."
echo "After login, verify new System Settings, TextInputMenuAgent, SystemUIServer, and ControlCenter PID/start-time generations."
echo "If ui-consumers collection fails, preserve its public process snapshot/error and stop as a tooling blocker; do not repeat logout/login, reboot, edit private defaults, repair caches, or automate UI."
echo "Capture pre-login with: /tmp/audit-installed-mode-icons ui-consumers /tmp/muu-icon-ui-pre-login.json /tmp/muu-icon-ui-pre-login.partial.json /tmp/muu-icon-ui-pre-login.error"
echo "Capture post-login with: /tmp/audit-installed-mode-icons verify-login-generation /tmp/muu-icon-ui-pre-login.json /tmp/muu-icon-ui-post-login.json /tmp/muu-icon-ui-post-login.partial.json /tmp/muu-icon-ui-post-login.error"
echo "The helper atomically publishes success or preserves filtered partial evidence plus a privacy-safe error; preserve failure files and stop."
echo "Re-run the same build, executable and asset hashes, code signature, and public TIS authority after login."
echo "Verify System Settings, Menu bar, and Input menu separately."
echo "A user-chosen reboot may substitute for logout/login, but Muu does not require or automate a reboot."
echo "Do not edit private defaults, repair caches, or automate UI."
echo "Preserve the post-login public snapshot and stop as stale cache."
