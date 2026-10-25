#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-license-bundle-test.XXXXXX")"
trap 'rm -rf "$scratch_dir"' EXIT

make_fixture() {
  local fixture_root="$1"
  mkdir -p \
    "$fixture_root/scripts" \
    "$fixture_root/app/MuuIME.app/Contents/Resources"
  cp "$repo_root/scripts/verify-bundled-third-party-licenses.sh" \
    "$fixture_root/scripts/verify-bundled-third-party-licenses.sh"
  cp "$repo_root/scripts/rust_code_licenses.py" "$fixture_root/scripts/rust_code_licenses.py"
  cp "$repo_root/scripts/rust_std_licenses.py" "$fixture_root/scripts/rust_std_licenses.py"
  cp "$repo_root/rust-toolchain" "$fixture_root/rust-toolchain"
  cp -R "$repo_root/THIRD_PARTY_LICENSES" \
    "$fixture_root/THIRD_PARTY_LICENSES"
  cp -R "$repo_root/THIRD_PARTY_LICENSES" \
    "$fixture_root/app/MuuIME.app/Contents/Resources/THIRD_PARTY_LICENSES"
}

positive_root="$scratch_dir/positive"
make_fixture "$positive_root"
"$positive_root/scripts/verify-bundled-third-party-licenses.sh" \
  "$positive_root/app/MuuIME.app" >/dev/null

missing_root="$scratch_dir/missing-required"
make_fixture "$missing_root"
rm \
  "$missing_root/THIRD_PARTY_LICENSES/dictionaries/sudachidict/LEGAL.txt" \
  "$missing_root/app/MuuIME.app/Contents/Resources/THIRD_PARTY_LICENSES/dictionaries/sudachidict/LEGAL.txt"
if "$missing_root/scripts/verify-bundled-third-party-licenses.sh" \
  "$missing_root/app/MuuIME.app" >/dev/null 2>&1
then
  echo "verifier accepted source and bundle with required LEGAL.txt deleted" >&2
  exit 1
fi

empty_root="$scratch_dir/empty"
mkdir -p \
  "$empty_root/scripts" \
  "$empty_root/THIRD_PARTY_LICENSES" \
  "$empty_root/app/MuuIME.app/Contents/Resources/THIRD_PARTY_LICENSES"
cp "$repo_root/scripts/verify-bundled-third-party-licenses.sh" \
  "$empty_root/scripts/verify-bundled-third-party-licenses.sh"
if "$empty_root/scripts/verify-bundled-third-party-licenses.sh" \
  "$empty_root/app/MuuIME.app" >/dev/null 2>&1
then
  echo "verifier accepted empty source and bundle license trees" >&2
  exit 1
fi

postal_root="$scratch_dir/postal-license-missing"
make_fixture "$postal_root"
rm "$postal_root/THIRD_PARTY_LICENSES/code_deps/miniz_oxide-0.8.9/LICENSE-MIT.md" \
  "$postal_root/app/MuuIME.app/Contents/Resources/THIRD_PARTY_LICENSES/code_deps/miniz_oxide-0.8.9/LICENSE-MIT.md"
if "$postal_root/scripts/verify-bundled-third-party-licenses.sh" "$postal_root/app/MuuIME.app" >/dev/null 2>&1; then
  echo "verifier accepted missing Postal decompressor license" >&2
  exit 1
fi

model_root="$scratch_dir/model-notice-missing"
make_fixture "$model_root"
rm "$model_root/THIRD_PARTY_LICENSES/models/kanakanji-edge-33m/NOTICE.txt" \
  "$model_root/app/MuuIME.app/Contents/Resources/THIRD_PARTY_LICENSES/models/kanakanji-edge-33m/NOTICE.txt"
if "$model_root/scripts/verify-bundled-third-party-licenses.sh" "$model_root/app/MuuIME.app" >/dev/null 2>&1; then
  echo "verifier accepted missing model attribution" >&2
  exit 1
fi

zenz_root="$scratch_dir/zenz-notice-missing"
make_fixture "$zenz_root"
rm "$zenz_root/THIRD_PARTY_LICENSES/models/zenz-v3.2-small/NOTICE.txt" \
  "$zenz_root/app/MuuIME.app/Contents/Resources/THIRD_PARTY_LICENSES/models/zenz-v3.2-small/NOTICE.txt"
if "$zenz_root/scripts/verify-bundled-third-party-licenses.sh" "$zenz_root/app/MuuIME.app" >/dev/null 2>&1; then
  echo "verifier accepted missing zenz / GGUF attribution" >&2
  exit 1
fi

# A negative passes only on exit 1 with the expected reason, so a fixture that
# fails for some unrelated error cannot count as the intended rejection.
expect_rejection() {
  local fixture_root="$1" reason="$2" status=0
  "$fixture_root/scripts/verify-bundled-third-party-licenses.sh" \
    "$fixture_root/app/MuuIME.app" >"$fixture_root/rejection.log" 2>&1 || status=$?
  if [[ "$status" -ne 1 ]] || ! grep -Fq "$reason" "$fixture_root/rejection.log"; then
    cat "$fixture_root/rejection.log" >&2
    echo "verifier did not reject the fixture for the expected reason: $reason" >&2
    exit 1
  fi
}

# Both sides missing the same Unicode exception must still fail the manifest.
stdlib_root="$scratch_dir/stdlib-unicode-missing"
make_fixture "$stdlib_root"
rm "$stdlib_root/THIRD_PARTY_LICENSES/rust/1.96.0/licenses/Unicode-3.0.txt" \
  "$stdlib_root/app/MuuIME.app/Contents/Resources/THIRD_PARTY_LICENSES/rust/1.96.0/licenses/Unicode-3.0.txt"
expect_rejection "$stdlib_root" "Rust standard library license check failed"

# Changed bytes in a required file: the set of names still matches.
changed_root="$scratch_dir/changed-bytes"
make_fixture "$changed_root"
printf '\nfixture altered bytes\n' >> \
  "$changed_root/app/MuuIME.app/Contents/Resources/THIRD_PARTY_LICENSES/NOTICE"
expect_rejection "$changed_root" "bundled license differs from source: NOTICE"

# An extra regular file: every required file is present and unchanged.
extra_root="$scratch_dir/extra-file"
make_fixture "$extra_root"
printf 'fixture extra file\n' > \
  "$extra_root/app/MuuIME.app/Contents/Resources/THIRD_PARTY_LICENSES/EXTRA.txt"
expect_rejection "$extra_root" "bundled third-party license file set differs from source"

# These links do not change find -type f's set. The final tree scan must
# reject them even though required files and all regular bytes still match.
for link_target in LICENSE models; do
  linked_root="$scratch_dir/linked-$link_target"
  make_fixture "$linked_root"
  ln -s "$link_target" \
    "$linked_root/app/MuuIME.app/Contents/Resources/THIRD_PARTY_LICENSES/extra-link"
  expect_rejection "$linked_root" "bundled third-party license tree contains a symlink"
done

echo "third-party license bundle verifier tests passed"
