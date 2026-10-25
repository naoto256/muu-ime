#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 1 ]]; then
  echo "usage: $0 /path/to/MuuIME.app" >&2
  exit 64
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
source_dir="$repo_root/THIRD_PARTY_LICENSES"
bundle_dir="$1/Contents/Resources/THIRD_PARTY_LICENSES"

if [[ ! -d "$source_dir" || -L "$source_dir" ]]; then
  echo "missing regular source license directory: $source_dir" >&2
  exit 1
fi

if [[ ! -d "$bundle_dir" || -L "$bundle_dir" ]]; then
  echo "missing regular bundled license directory: $bundle_dir" >&2
  exit 1
fi

# Files that must exist in both trees. The checks below then require the two
# trees to hold exactly the same regular files with the same bytes, and the
# bundle to hold no symlink at all.
required_files=(
  "LICENSE"
  "NOTICE"
  "models/kanakanji-edge-33m/NOTICE.txt"
  "models/zenz-v3.2-small/NOTICE.txt"
  "rust/manifest.json"
  "code_deps/manifest.json"
  "code_deps/SOURCE-NOTICES.txt"
  "dictionaries/mozc/LICENSE.txt"
  "dictionaries/mozc/README.txt"
  "data/cldr/LICENSE.txt"
  "data/cldr/README.txt"
  "dictionaries/sudachidict/LICENSE-2.0.txt"
  "dictionaries/sudachidict/LEGAL.txt"
)

for relative_path in "${required_files[@]}"; do
  source_file="$source_dir/$relative_path"
  bundle_file="$bundle_dir/$relative_path"
  if [[ ! -f "$source_file" || -L "$source_file" ]]; then
    echo "missing required regular source license: $relative_path" >&2
    exit 1
  fi
  if [[ ! -f "$bundle_file" || -L "$bundle_file" ]]; then
    echo "missing required regular bundled license: $relative_path" >&2
    exit 1
  fi
done

# This checks the reviewed file set and hashes without Cargo or a registry.
# Cargo-to-manifest coverage is checked separately by rust-code-license-check.
python3 "$repo_root/scripts/rust_code_licenses.py" --check-bundle "$1"
python3 "$repo_root/scripts/rust_std_licenses.py" --check-bundle "$1"

source_files="$({
  cd "$source_dir"
  find . -type f -print | LC_ALL=C sort
})"
bundle_files="$({
  cd "$bundle_dir"
  find . -type f -print | LC_ALL=C sort
})"

if [[ "$source_files" != "$bundle_files" ]]; then
  echo "bundled third-party license file set differs from source" >&2
  diff -u <(printf '%s\n' "$source_files") <(printf '%s\n' "$bundle_files") || true
  exit 1
fi

while IFS= read -r relative_path; do
  [[ -n "$relative_path" ]] || continue
  source_file="$source_dir/${relative_path#./}"
  bundle_file="$bundle_dir/${relative_path#./}"
  if [[ ! -f "$bundle_file" || -L "$bundle_file" ]]; then
    echo "bundled license is not a regular file: $bundle_file" >&2
    exit 1
  fi
  if ! cmp -s "$source_file" "$bundle_file"; then
    echo "bundled license differs from source: ${relative_path#./}" >&2
    exit 1
  fi
done <<< "$source_files"

if find "$bundle_dir" -type l -print -quit | grep -q .; then
  echo "bundled third-party license tree contains a symlink" >&2
  exit 1
fi

echo "third-party license bundle verified: $bundle_dir"
