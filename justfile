set shell := ["bash", "-euo", "pipefail", "-c"]

# Minimum supported OS: keep native Rust and standalone Swift/ObjC tests aligned.
export MACOSX_DEPLOYMENT_TARGET := "15.0"
swift_target := "arm64-apple-macosx15.0"

default: build

rust-targets:
    rustup target add --toolchain "$(cat rust-toolchain)" aarch64-apple-darwin

# Regenerate include/muu_ffi.h with cbindgen. header-check diffs a fresh
# generation against the committed file. An explicit CBINDGEN takes priority,
# including an invalid empty value; only an unset variable permits fallback.
# Otherwise use PATH, then Cargo's bin directory.
header:
    #!/usr/bin/env bash
    set -euo pipefail
    # `${CBINDGEN-...}` (no colon) uses the default only when CBINDGEN is
    # unset, and `${CBINDGEN+x}` is empty only then. An empty CBINDGEN is
    # therefore kept as the path, and `test -x` rejects it.
    cbindgen_bin="${CBINDGEN-${CARGO_HOME:-${HOME}/.cargo}/bin/cbindgen}"
    if [[ -z "${CBINDGEN+x}" ]] && command -v cbindgen >/dev/null 2>&1; then
      cbindgen_bin="$(command -v cbindgen)"
    fi
    test -x "$cbindgen_bin" || { echo "cbindgen is required: cargo install cbindgen --locked" >&2; exit 1; }
    mkdir -p include
    "$cbindgen_bin" --config cbindgen.toml --crate muu-ffi --output include/muu_ffi.h

# The Release staticlib for Xcode and the Swift tests. The source commit is
# stamped into it so verify-rust-source-commit.py can tie the app to HEAD;
# lipo confirms the archive is arm64 only.
rust-arm64: rust-targets
    #!/usr/bin/env bash
    set -euo pipefail
    export MUU_RUST_SOURCE_COMMIT="$(git rev-parse HEAD)"
    rust_target_dir="${CARGO_TARGET_DIR:-target}"
    python3 scripts/rust_toolchain.py cargo build -p muu-ffi --release --target aarch64-apple-darwin
    mkdir -p build
    cp "$rust_target_dir/aarch64-apple-darwin/release/libmuu_ffi.a" build/libmuu_ffi.a
    test "$(lipo -archs build/libmuu_ffi.a)" = arm64

# Print the pinned tool versions, then run the real recipes against fake
# tools with a hostile PATH (scripts/test-build-toolchain.py).
rust-toolchain-absolute-test:
    python3 scripts/rust_toolchain.py --versions
    python3 scripts/test-build-toolchain.py "{{just_executable()}}"

# Build and inspection must use the same overrides, including their source and
# version stamps. Keep this invocation as their single authority.
# The variant sets both the plist value and the Swift condition, and the two
# variants build into separate DerivedData, so a dogfood app is never left at
# the path the standard checks read.
# release-bundle-check reads `settings` to verify the flags of what `build`
# made; an override added only to one action would void that check.
[private]
_xcode-release action variant="standard":
    #!/usr/bin/env bash
    set -euo pipefail
    case '{{ action }}' in
      build) action_args=(build) ;;
      settings) action_args=(-showBuildSettings -json) ;;
      *) echo 'unknown Release Xcode action' >&2; exit 1 ;;
    esac
    case '{{ variant }}' in
      standard) build_dir=build/xcode; conditions= ;;
      dogfood) build_dir=build/xcode-dogfood; conditions=MUU_DOGFOOD ;;
      *) echo 'unknown build variant' >&2; exit 1 ;;
    esac
    xcodebuild -quiet \
      -project apple/MuuIME.xcodeproj \
      -scheme MuuIME \
      -configuration Release \
      -derivedDataPath "$build_dir" \
      CODE_SIGNING_ALLOWED=NO \
      ARCHS="arm64" \
      ONLY_ACTIVE_ARCH=NO \
      CURRENT_PROJECT_VERSION="$(git rev-list --count HEAD)" \
      MUU_SOURCE_COMMIT="$(git rev-parse HEAD)" \
      MUU_BUILD_VARIANT={{ variant }} \
      SWIFT_ACTIVE_COMPILATION_CONDITIONS="$conditions" \
      "${action_args[@]}"

# The unsigned Release app, then its source-commit and license checks. A
# shallow clone is refused: the build number is the commit count, which a
# shallow history would give wrongly. The standard-library check runs again
# with --archive once rust-arm64 has built the staticlib, so the reviewed
# sysroot components are compared with the objects actually linked.
xcode-ci: rust-code-license-check rust-stdlib-license-check header rust-arm64 dict-build
    just _xcode-ci standard

[private]
_xcode-ci variant:
    #!/usr/bin/env bash
    set -euo pipefail
    case '{{ variant }}' in
      standard) app=build/xcode/Build/Products/Release/MuuIME.app ;;
      dogfood) app=build/xcode-dogfood/Build/Products/Release/MuuIME.app ;;
      *) echo 'unknown build variant' >&2; exit 1 ;;
    esac
    test "$(git rev-parse --is-shallow-repository)" = "false"
    python3 scripts/rust_std_licenses.py --check --archive build/libmuu_ffi.a
    just _xcode-release build '{{ variant }}'
    python3 scripts/verify-rust-source-commit.py "$app" "$(git rev-parse HEAD)"
    scripts/verify-bundled-third-party-licenses.sh "$app"

# Explicit local recording-capable Release. Never selected by the release gate.
# This only builds, signs and verifies an artifact; it does not install it.
xcode-dogfood: rust-code-license-check rust-stdlib-license-check header rust-arm64 dict-build
    #!/usr/bin/env bash
    set -euo pipefail
    just _xcode-ci dogfood
    app=build/xcode-dogfood/Build/Products/Release/MuuIME.app
    codesign --force --sign - --options runtime "$app"
    codesign --verify --deep --strict --verbose=2 "$app"
    mkdir -p build/reviewed-dogfood
    just _xcode-release settings dogfood > build/reviewed-dogfood/build-settings.json
    python3 scripts/verify-release-resources.py --app "$app" --repository . \
      --build-settings build/reviewed-dogfood/build-settings.json --expected-variant dogfood
    shasum -a 256 "$app/Contents/MacOS/MuuIME" > build/reviewed-dogfood/MuuIME-executable.sha256
    xcrun swift -target "{{ swift_target }}" scripts/audit-installed-mode-icons.swift \
      signature "$app" > build/reviewed-dogfood/MuuIME-codesign.json
    xcrun swift -target "{{ swift_target }}" scripts/audit-installed-mode-icons.swift \
      verify-artifact "$app" "$(git rev-parse HEAD)" "$(git rev-list --count HEAD)" dogfood \
      "$(cut -d ' ' -f 1 build/reviewed-dogfood/MuuIME-executable.sha256)" \
      build/reviewed-dogfood/MuuIME-codesign.json > build/reviewed-dogfood/MuuIME-artifact.json

# Ad-hoc signature with the hardened runtime, then a strict verify. No
# certificate is involved.
xcode: xcode-ci
    codesign --force --sign - --options runtime build/xcode/Build/Products/Release/MuuIME.app
    codesign --verify --strict --verbose=2 build/xcode/Build/Products/Release/MuuIME.app

# Record what was verified: the executable's SHA-256 and its signature
# report, then an artifact report built from both. install passes these to
# install-dev.sh, which installs only an app that matches them.
reviewed-executable-authority: xcode
    #!/usr/bin/env bash
    set -euo pipefail
    python3 scripts/verify-rust-source-commit.py build/xcode/Build/Products/Release/MuuIME.app "$(git rev-parse HEAD)"
    scripts/verify-bundled-third-party-licenses.sh \
      build/xcode/Build/Products/Release/MuuIME.app
    mkdir -p build/reviewed
    shasum -a 256 \
      build/xcode/Build/Products/Release/MuuIME.app/Contents/MacOS/MuuIME \
      > build/reviewed/MuuIME-executable.sha256
    xcrun swift -target "{{ swift_target }}" scripts/audit-installed-mode-icons.swift \
      signature \
      build/xcode/Build/Products/Release/MuuIME.app \
      > build/reviewed/MuuIME-codesign.json
    test "$(cut -d ' ' -f 1 build/reviewed/MuuIME-executable.sha256)" \
      = "$(shasum -a 256 build/xcode/Build/Products/Release/MuuIME.app/Contents/MacOS/MuuIME | cut -d ' ' -f 1)"
    test -s build/reviewed/MuuIME-codesign.json
    xcrun swift -target "{{ swift_target }}" scripts/audit-installed-mode-icons.swift \
      verify-artifact \
      build/xcode/Build/Products/Release/MuuIME.app \
      "$(git rev-parse HEAD)" \
      "$(git rev-list --count HEAD)" \
      standard \
      "$(cut -d ' ' -f 1 build/reviewed/MuuIME-executable.sha256)" \
      build/reviewed/MuuIME-codesign.json \
      > build/reviewed/MuuIME-artifact.json

build: xcode

fmt:
    python3 scripts/rust_toolchain.py cargo fmt --all

fmt-check:
    python3 scripts/rust_toolchain.py cargo fmt --all -- --check

# All Rust tests except the release-only scale group: fast, then assets.
test: rust-fast-test rust-assets-test

# Rust's fast group from scripts/run-rust-test-group.py, plus doc-tests.
# The asset and scale exceptions are in test-support/rust-test-groups.json.
# The runner strips asset paths and forces required-asset mode, so a test
# that reaches for a dictionary or model fails here instead of skipping.
rust-fast-test: rust-test-groups-test
    python3 scripts/run-rust-test-group.py fast
    python3 scripts/rust_toolchain.py cargo test --workspace --doc

rust-test-groups-test:
    python3 scripts/run-rust-test-group.py --self-test

# The 64 Rust tests that need the public dictionary, edge, zenz or the
# bundled seed, with MUU_REQUIRE_REAL_ASSETS=1. Fails, rather than skips,
# when an asset is missing; the models are never fetched here.
rust-assets-test: dict-build
    #!/usr/bin/env bash
    set -euo pipefail
    export MUU_REAL_DICT_PATH="${MUU_REAL_DICT_PATH:-$PWD/build/mozc-oss.muudict}"
    export MUU_LM_MODEL_DIR="${MUU_LM_MODEL_DIR:-$HOME/Library/Application Support/MuuIME/Models/kanakanji-edge-33m}"
    export MUU_ZENZ_MODEL_DIR="${MUU_ZENZ_MODEL_DIR:-$HOME/Library/Application Support/MuuIME/Models/zenz-v3.2-small}"
    export MUU_REQUIRE_REAL_ASSETS=1
    for model_file in model.safetensors tokenizer.json; do
      if [ ! -f "$MUU_LM_MODEL_DIR/$model_file" ]; then
        echo "Missing $MUU_LM_MODEL_DIR/$model_file. Run scripts/fetch-conversion-lm.sh --model edge before just test." >&2
        exit 1
      fi
    done
    if [ ! -f "$MUU_ZENZ_MODEL_DIR/ggml-model-Q5_K_M.gguf" ]; then
      echo "Missing zenz GGUF. Run scripts/fetch-conversion-lm.sh --model zenz before just test." >&2
      exit 1
    fi
    python3 scripts/run-rust-test-group.py assets

clippy:
    python3 scripts/rust_toolchain.py cargo clippy --workspace --all-targets -- -D warnings

# Swift tests compile production sources directly with the test file, outside
# Xcode. Each list below names exactly what one test links; entries are one
# per line, and _swift-test-compile drops blank lines.
swift_test_flags := '''
-D
MUU_TEST
'''

candidate_apply_flags := swift_test_flags + '''

-import-objc-header
apple/MuuIME/MuuIME-Bridging-Header.h
-Xcc
-Iinclude
'''

# Headless store tests also use the C header as the sole source of result values.
swift_abi_import_flags := '''
-import-objc-header
include/muu_ffi.h
'''
swift_abi_flags := swift_test_flags + swift_abi_import_flags
candidate_surface_flags := candidate_apply_flags

candidate_ui_sources := '''
apple/MuuIME/CandidateSurfaceModels.swift
apple/MuuIME/CandidateActionPolicy.swift
apple/MuuIME/CandidateGeometryPolicy.swift
apple/MuuIME/ClientGeometryReader.swift
apple/MuuIME/CandidateAttributeGeometry.swift
apple/MuuIME/CandidatePanelController.swift
apple/MuuIME/CandidatePanelView.swift
apple/MuuIME/ModeBadgeController.swift
apple/MuuIME/ModeBadgeScheduler.swift
apple/MuuIME/BadgePlacement.swift
apple/MuuIME/InputUICleanup.swift
'''

candidate_surface_policy_sources := candidate_ui_sources + '''

apple/MuuIME/ProtocolModels.swift
scripts/test-candidate-surface-policy.swift
'''

candidate_apply_sources := candidate_ui_sources + '''

apple/MuuIME/ApplicationLifecycle.swift
apple/MuuIME/ProtocolModels.swift
scripts/LegacyCoreFixtureModels.swift
scripts/AboutStatusTestSupport.swift
scripts/ModelDownloadUITestSupport.swift
scripts/LifecycleTestSupport.swift
scripts/CommitStreamTestClock.swift
apple/MuuIME/FileSafety.swift
apple/MuuIME/DictionaryWindowSupport.swift
apple/MuuIME/UserDictionaryStore.swift
apple/MuuIME/UserDictionaryWindowController.swift
apple/MuuIME/DictionaryDisplayModels.swift
apple/MuuIME/ImportedDictionaryStore.swift
apple/MuuIME/ImportedDictionaryWindowController.swift
apple/MuuIME/PostalCodeStore.swift
apple/MuuIME/PostalCodeWindowController.swift
apple/MuuIME/CommitStreamDelivery.swift
apple/MuuIME/CommitStreamFileWriter.swift
apple/MuuIME/CommitStreamRecorder.swift
apple/MuuIME/BuildCapabilities.swift
apple/MuuIME/RustEngine.swift
apple/MuuIME/PersonalFrequencyStore.swift
apple/MuuIME/InputController.swift
apple/MuuIME/InputPresentation.swift
apple/MuuIME/InputSession.swift
apple/MuuIME/InputSessionCoordinator.swift
apple/MuuIME/InputMenuActions.swift
apple/MuuIME/AboutStatus.swift
apple/MuuIME/ConversionModelProfile.swift
apple/MuuIME/ModelDownload.swift
apple/MuuIME/AboutPresenter.swift
scripts/test-input-controller-apply.swift
'''

commit_stream_recorder_library_sources := '''
apple/MuuIME/ApplicationLifecycle.swift
apple/MuuIME/FileSafety.swift
apple/MuuIME/ProtocolModels.swift
apple/MuuIME/CommitStreamDelivery.swift
apple/MuuIME/CommitStreamFileWriter.swift
apple/MuuIME/CommitStreamRecorder.swift
apple/MuuIME/BuildCapabilities.swift
'''

commit_stream_recorder_sources := commit_stream_recorder_library_sources + '''

scripts/CommitStreamTestClock.swift
scripts/test-commit-stream-recorder.swift
'''

recorder_factory_visibility_sources := commit_stream_recorder_library_sources + '''

scripts/RecorderFactoryVisibility.swift
'''

dictionary_codec_sources := '''
apple/MuuIME/ApplicationLifecycle.swift
apple/MuuIME/ProtocolModels.swift
apple/MuuIME/FileSafety.swift
apple/MuuIME/UserDictionaryStore.swift
apple/MuuIME/DictionaryDisplayModels.swift
'''

user_dictionary_sources := dictionary_codec_sources + '''

scripts/test-user-dictionary.swift
'''

imported_dictionary_sources := dictionary_codec_sources + '''
apple/MuuIME/DictionaryWindowSupport.swift
apple/MuuIME/ImportedDictionaryStore.swift
apple/MuuIME/ImportedDictionaryWindowController.swift
scripts/test-imported-dictionary.swift
'''

postal_code_sources := '''
apple/MuuIME/ApplicationLifecycle.swift
apple/MuuIME/ProtocolModels.swift
apple/MuuIME/FileSafety.swift
apple/MuuIME/PostalCodeStore.swift
apple/MuuIME/PostalCodeWindowController.swift
scripts/test-postal-code.swift
'''

store_abi_sources := '''
apple/MuuIME/ApplicationLifecycle.swift
apple/MuuIME/FileSafety.swift
apple/MuuIME/ProtocolModels.swift
apple/MuuIME/DictionaryDisplayModels.swift
apple/MuuIME/DictionaryWindowSupport.swift
apple/MuuIME/UserDictionaryStore.swift
apple/MuuIME/ImportedDictionaryStore.swift
apple/MuuIME/PostalCodeStore.swift
apple/MuuIME/CommitStreamDelivery.swift
apple/MuuIME/CommitStreamFileWriter.swift
apple/MuuIME/CommitStreamRecorder.swift
apple/MuuIME/BuildCapabilities.swift
apple/MuuIME/RustEngine.swift
apple/MuuIME/PersonalFrequencyStore.swift
apple/MuuIME/AboutStatus.swift
apple/MuuIME/ConversionModelProfile.swift
apple/MuuIME/ModelDownload.swift
apple/MuuIME/AboutPresenter.swift
scripts/LegacyCoreFixtureModels.swift
scripts/test-store-abi.swift
'''

candidate_surface_link_args := '''
-framework
InputMethodKit
'''

candidate_apply_link_args := '''
-L
build
-lmuu_ffi
-framework
Accelerate
-framework
AppKit
-framework
InputMethodKit
'''

# Each list is read line by line into an array, so every path stays one
# argument. `extra_condition` adds one -D, which the recorder visibility
# check uses to inject an unrelated error.
[private]
_swift-test-compile output compile_args sources link_args extra_condition="":
    #!/usr/bin/env bash
    set -euo pipefail
    compiler_args=()
    while IFS= read -r arg; do
      test -z "$arg" || compiler_args+=("$arg")
    done <<'MUU_COMPILER_ARGS'
    {{ compile_args }}
    MUU_COMPILER_ARGS
    if test -n '{{ extra_condition }}'; then
      compiler_args+=(-D '{{ extra_condition }}')
    fi
    source_args=()
    while IFS= read -r source; do
      test -z "$source" || source_args+=("$source")
    done <<'MUU_SOURCE_ARGS'
    {{ sources }}
    MUU_SOURCE_ARGS
    linker_args=()
    while IFS= read -r arg; do
      test -z "$arg" || linker_args+=("$arg")
    done <<'MUU_LINKER_ARGS'
    {{ link_args }}
    MUU_LINKER_ARGS
    # Candidate fallback needs the ObjC exception boundary.
    if [[ '{{ sources }}' == *ClientGeometryReader.swift* ]]; then
      compiler_args+=(-Xcc -DMUU_TEST)
      xcrun clang -fobjc-arc -DMUU_TEST \
        -c apple/MuuIME/ClientGeometryBridge.m -o '{{ output }}-bridge.o'
      linker_args+=('{{ output }}-bridge.o')
    fi
    # `${a[@]+"${a[@]}"}` expands an empty array to nothing under `set -u`;
    # the macOS bash 3.2 treats a plain "${a[@]}" of an empty array as unset.
    xcrun swiftc -target "{{ swift_target }}" \
      ${compiler_args[@]+"${compiler_args[@]}"} \
      ${source_args[@]+"${source_args[@]}"} \
      ${linker_args[@]+"${linker_args[@]}"} \
      -o '{{ output }}'

# Candidate panel and badge policy, with no Core and no dictionary.
candidate-surface-policy-test:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-candidate-surface.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    just _swift-test-compile \
      "$scratch_dir/test-candidate-surface-policy" \
      "{{ candidate_surface_flags }}" \
      "{{ candidate_surface_policy_sources }}" \
      "{{ candidate_surface_link_args }}"
    "$scratch_dir/test-candidate-surface-policy"

# Every controller mode, both asset groups.
candidate-apply-characterization-test: header rust-arm64 dict-build
    just _candidate-apply-test all

# Controller cases in scripts/input_path_cases.py's fast group (no dictionary).
candidate-apply-fast-test: header rust-arm64
    just _candidate-apply-test fast

# Controller cases in scripts/input_path_cases.py's assets group, restart pairs included.
candidate-apply-assets-test: header rust-arm64 dict-build
    just _candidate-apply-test assets

[private]
_candidate-apply-test group:
    #!/usr/bin/env bash
    set -euo pipefail
    # Ancestor-link tests must not inherit macOS's /var or /tmp aliases.
    mkdir -p target/swift-controller-fixtures
    scratch_dir="$(mktemp -d "$PWD/target/swift-controller-fixtures/run.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    just _swift-test-compile \
      "$scratch_dir/test-input-controller-apply" \
      "{{ candidate_apply_flags }}" \
      "{{ candidate_apply_sources }}" \
      "{{ candidate_apply_link_args }}"
    # Each invocation fixes one Core startup data and resource configuration.
    test_cases="$(python3 scripts/input_path_cases.py --group '{{ group }}')"
    dictionary="${MUU_REAL_DICT_PATH:-$PWD/build/mozc-oss.muudict}"
    if [[ '{{ group }}' == fast ]]; then
      dictionary="$scratch_dir/absent.muudict"
    else
      test -s "$dictionary"
    fi
    while IFS= read -r test_case; do
      MUU_TEST_LEARNING_RESTART_DIR="$scratch_dir/restart" "$scratch_dir/test-input-controller-apply" "$dictionary" "$test_case" | tee "$scratch_dir/$test_case.log"
      # Exactly one completion marker: a process that exits 0 before its end,
      # or runs a case twice, fails even though pipefail saw success.
      test "$(grep -Fxc "candidate apply characterization: $test_case passed" "$scratch_dir/$test_case.log")" = 1
    done <<< "$test_cases"

# Compare public effects and FFI requests across the input-path rewrite.
input-path-contract-trace: header rust-arm64 dict-build
    #!/usr/bin/env bash
    set -euo pipefail
    # Explicit artifact directory only; never select installed/user data.
    trace_dir="${MUU_INPUT_PATH_TRACE_DIR:?set an explicit output directory}"
    mkdir -p "$trace_dir" target/swift-controller-fixtures
    scratch_dir="$(mktemp -d "$PWD/target/swift-controller-fixtures/trace.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    just _swift-test-compile \
      "$scratch_dir/controller" \
      "{{ candidate_apply_flags }}" \
      "{{ candidate_apply_sources }}" \
      "{{ candidate_apply_link_args }}"
    test_cases="$(python3 scripts/input_path_cases.py --trace)"
    while IFS= read -r test_case; do
      test ! -e "$trace_dir/$test_case.jsonl"
      MUU_INPUT_PATH_TRACE="$trace_dir/$test_case.jsonl" \
        MUU_TEST_LEARNING_RESTART_DIR="$scratch_dir/restart" \
        "$scratch_dir/controller" "$PWD/build/mozc-oss.muudict" "$test_case" | tee "$scratch_dir/$test_case.log"
      test "$(grep -Fxc "candidate apply characterization: $test_case passed" "$scratch_dir/$test_case.log")" = 1
    done <<< "$test_cases"

input-path-trace-test:
    python3 scripts/compare-input-path-traces.py --self-test

# The termination lifecycle in its two modes, one process each. The isolated
# bundle inherits production termination flags so AppKit cannot silently skip
# the delegate: if the shipped plist allowed sudden termination, this test
# would see the same skipped flush. The timeout only guards against a hang.
application-lifecycle-test:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-lifecycle.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    app="$scratch_dir/Lifecycle.app"
    mkdir -p "$app/Contents/MacOS"
    python3 -c 'import plistlib, sys; source = plistlib.load(open(sys.argv[1], "rb")); info = {"CFBundleIdentifier": "dev.muu.test.Lifecycle", "CFBundleExecutable": "Lifecycle", "LSUIElement": True}; info.update({key: source[key] for key in ("NSSupportsSuddenTermination", "NSSupportsAutomaticTermination") if key in source}); plistlib.dump(info, open(sys.argv[2], "wb"))' apple/MuuIME/Info.plist "$app/Contents/Info.plist"
    just _swift-test-compile "$app/Contents/MacOS/Lifecycle" "{{ swift_test_flags }}" \
      $'apple/MuuIME/ApplicationLifecycle.swift\nscripts/test-application-lifecycle.swift' ""
    for mode in early deadline; do
      python3 -c 'import subprocess, sys; subprocess.run(sys.argv[1:], check=True, timeout=180)' \
        "$app/Contents/MacOS/Lifecycle" "$mode" > "$scratch_dir/$mode.log" 2>&1
      cat "$scratch_dir/$mode.log"
      grep -F "AppKit termination: $mode passed" "$scratch_dir/$mode.log" >/dev/null
    done

# Manual real-network smoke. Run explicitly; never a check/CI dependency.
# It builds the production downloader without MUU_TEST (see the private
# recipe below) and downloads the pinned model into `destination`, which must
# be an empty `muu-model-live.*` directory directly under this checkout's build/.
[positional-arguments]
model-download-live-smoke destination:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d /tmp/muu-model-live-build.XXXXXX)"
    trap 'rm -rf "$scratch_dir"' EXIT
    just _model-download-live-smoke-build "$scratch_dir/live-smoke"
    "$scratch_dir/live-smoke" "$1"

# The smoke entry is appended to ModelDownload.swift in one file, so it can
# use the file's private downloader and configuration without a test flag or
# an API added only for it.
[private]
[positional-arguments]
_model-download-live-smoke-build output:
    #!/usr/bin/env bash
    set -euo pipefail
    cat apple/MuuIME/ModelDownload.swift scripts/model-download-live-smoke.swift > "$1.swift"
    xcrun swiftc -swift-version 5 -target arm64-apple-macosx15.0 -parse-as-library -O \
      apple/MuuIME/ApplicationLifecycle.swift apple/MuuIME/FileSafety.swift apple/MuuIME/ConversionModelProfile.swift "$1.swift" -o "$1"

# Fixed-profile model acquisition: synthetic bytes and URLProtocol only.
model-download-test:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-model-download.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    just _swift-test-compile "$scratch_dir/test-model-download" "{{ swift_test_flags }}" \
      $'apple/MuuIME/ApplicationLifecycle.swift\napple/MuuIME/FileSafety.swift\napple/MuuIME/ConversionModelProfile.swift\napple/MuuIME/ModelDownload.swift\nscripts/test-model-download.swift' ""
    python3 -c 'import subprocess, sys; subprocess.run(sys.argv[1:], check=True, timeout=60)' "$scratch_dir/test-model-download"
    # As for the recorder: the test-only factory must compile with MUU_TEST,
    # fail without it for that missing member (and same-call lost type context).
    # An unrelated error must not pass as that result.
    visibility_sources=$'apple/MuuIME/ApplicationLifecycle.swift\napple/MuuIME/FileSafety.swift\napple/MuuIME/ConversionModelProfile.swift\napple/MuuIME/ModelDownload.swift\nscripts/ModelDownloadFactoryVisibility.swift'
    just _swift-test-compile "$scratch_dir/factory-test" "{{ swift_test_flags }}" "$visibility_sources" ""
    "$scratch_dir/factory-test"
    if just _swift-test-compile "$scratch_dir/factory-standard" "" "$visibility_sources" "" > "$scratch_dir/standard.log" 2>&1; then
      echo 'model acquisition injection leaked into the standard build' >&2
      exit 1
    fi
    python3 scripts/check-recorder-visibility-diagnostics.py --model-download "$scratch_dir/standard.log"
    if just _swift-test-compile "$scratch_dir/factory-mixed" "" "$visibility_sources" "" MUU_UNRELATED_DIAGNOSTIC_FIXTURE > "$scratch_dir/mixed.log" 2>&1; then
      echo 'mixed-error fixture unexpectedly compiled' >&2
      exit 1
    fi
    if python3 scripts/check-recorder-visibility-diagnostics.py --model-download "$scratch_dir/mixed.log"; then
      echo 'unrelated error was accepted as an absent model factory' >&2
      exit 1
    fi

# This source is compiled without MUU_TEST too, so the standard policy is
# observed directly rather than inferred from a test-enabled build.
session-create-policy-test:
    #!/usr/bin/env bash
    set -euo pipefail
    destination=build/test-evidence/session-create-policy
    mkdir -p "$destination"
    for variant in standard dogfood test; do
      flags=""
      case "$variant" in
        dogfood) flags=$'-D\nMUU_DOGFOOD' ;;
        test) flags=$'-D\nMUU_TEST' ;;
      esac
      just _swift-test-compile "$destination/$variant" "$flags" $'apple/MuuIME/BuildCapabilities.swift\nscripts/test-force-create-failure.swift' ""
      "$destination/$variant"
    done

# Runs the real AppDelegate against same-module doubles (scripts/
# AppStartupTestSupport.swift), one process per mode, because the doubles are
# process-wide singletons and the lifecycle shuts down only once. The bundle
# is needed because MuuConstants reads Bundle.main's identifier.
application-startup-test:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-startup.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    app="$scratch_dir/Startup.app"
    mkdir -p "$app/Contents/MacOS"
    python3 -c 'import plistlib, sys; plistlib.dump({"CFBundleIdentifier": "dev.muu.test.Startup", "CFBundleExecutable": "Startup"}, open(sys.argv[1], "wb"))' "$app/Contents/Info.plist"
    just _swift-test-compile "$app/Contents/MacOS/Startup" "{{ swift_test_flags }}" \
      $'apple/MuuIME/ApplicationLifecycle.swift\napple/MuuIME/AppDelegate.swift\nscripts/AppStartupTestSupport.swift\nscripts/test-application-startup.swift' ""
    for mode in abi-mismatch initialization-failure degraded-success normal-success; do
      python3 -c 'import subprocess, sys; subprocess.run(sys.argv[1:], check=True, timeout=60)' \
        "$app/Contents/MacOS/Startup" "$mode"
    done

# The real stores and Core through the C ABI, across process restarts and
# hard exits. Scratch lives under build/, because the stores refuse the
# /var symlink that the system temporary directory sits behind.
store-abi-test: header rust-arm64 dict-build
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-store-abi.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    just _swift-test-compile "$scratch_dir/test-store-abi" "{{ candidate_apply_flags }}" \
      "{{ store_abi_sources }}" "{{ candidate_apply_link_args }}"
    python3 scripts/test-store-abi.py --consumer "$scratch_dir/test-store-abi" \
      --dictionary "$PWD/build/mozc-oss.muudict" --scratch-parent "$PWD/build"

commit-stream-delivery-test:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-commit-delivery.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    just _swift-test-compile "$scratch_dir/test-delivery" "{{ swift_abi_flags }}" \
      $'apple/MuuIME/ApplicationLifecycle.swift\napple/MuuIME/CommitStreamDelivery.swift\nscripts/test-commit-stream-delivery.swift' ""
    "$scratch_dir/test-delivery"

commit-stream-file-writer-test:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-commit-files.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    just _swift-test-compile "$scratch_dir/test-files" "{{ swift_abi_flags }}" \
      $'apple/MuuIME/ApplicationLifecycle.swift\napple/MuuIME/FileSafety.swift\napple/MuuIME/CommitStreamDelivery.swift\napple/MuuIME/CommitStreamFileWriter.swift\nscripts/test-commit-stream-files.swift' ""
    "$scratch_dir/test-files"

# The recorder tests, then a visibility check: the test-only factory entry
# must exist in a MUU_TEST build and be absent from a standard one.
commit-stream-recorder-test:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-commit-stream-recorder.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    just _swift-test-compile \
      "$scratch_dir/test-commit-stream-recorder" \
      "{{ swift_abi_flags }}" \
      "{{ commit_stream_recorder_sources }}" \
      ""
    "$scratch_dir/test-commit-stream-recorder"
    just _swift-test-compile "$scratch_dir/test-recorder-dogfood" "{{ swift_abi_flags }}" \
      "{{ commit_stream_recorder_sources }}" "" MUU_DOGFOOD
    "$scratch_dir/test-recorder-dogfood"
    just _swift-test-compile \
      "$scratch_dir/factory-test-entry" \
      "{{ swift_abi_flags }}" \
      "{{ recorder_factory_visibility_sources }}" \
      ""
    "$scratch_dir/factory-test-entry"
    # Standard compilation must reject the same consumer, specifically because
    # its injection entry is absent, rather than for an unrelated compile error.
    if just _swift-test-compile \
      "$scratch_dir/factory-standard-entry" \
      "{{ swift_abi_import_flags }}" \
      "{{ recorder_factory_visibility_sources }}" \
      "" > "$scratch_dir/factory-standard.log" 2>&1; then
      echo "recorder injection entry leaked into the standard build" >&2
      exit 1
    fi
    python3 scripts/check-recorder-visibility-diagnostics.py --self-test
    python3 scripts/check-recorder-visibility-diagnostics.py "$scratch_dir/factory-standard.log"
    # A standard-only type error must not be hidden by the expected error.
    if just _swift-test-compile \
      "$scratch_dir/factory-mixed-errors" \
      "{{ swift_abi_import_flags }}" \
      "{{ recorder_factory_visibility_sources }}" \
      "" MUU_UNRELATED_DIAGNOSTIC_FIXTURE > "$scratch_dir/factory-mixed.log" 2>&1; then
      echo "mixed-error fixture unexpectedly compiled" >&2
      exit 1
    fi
    if python3 scripts/check-recorder-visibility-diagnostics.py "$scratch_dir/factory-mixed.log"; then
      echo "unrelated compiler error was accepted as a visibility result" >&2
      exit 1
    else
      test "$?" -eq 1
    fi
    echo "recorder factory visibility: standard entry absent"

user-dictionary-test:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-user-dictionary.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    just _swift-test-compile \
      "$scratch_dir/test-user-dictionary" \
      "{{ swift_abi_flags }}" \
      "{{ user_dictionary_sources }}" \
      ""
    "$scratch_dir/test-user-dictionary"

imported-dictionary-test:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-imported-dictionary.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    just _swift-test-compile \
      "$scratch_dir/test-imported-dictionary" \
      "{{ swift_abi_flags }}" \
      "{{ imported_dictionary_sources }}" \
      ""
    "$scratch_dir/test-imported-dictionary"

# Release-only scale group in test-support/rust-test-groups.json: synthetic
# catalog/payload bounds and BLAS shape guards; no real assets.
rust-scale-test: rust-test-groups-test
    python3 scripts/run-rust-test-group.py scale --release

postal-code-test:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-postal-code.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    just _swift-test-compile \
      "$scratch_dir/test-postal-code" \
      "{{ swift_abi_flags }}" \
      "{{ postal_code_sources }}" \
      ""
    "$scratch_dir/test-postal-code"

# Generate the icons twice, under different locales and time zones. Both
# runs must match each other and the committed assets byte for byte; the
# .icns must expand to every size, and the previews must be written.
mode-icon-test:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-mode-icons.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    xcrun swift -target "{{ swift_target }}" scripts/generate-mode-icons.swift --self-test
    env LC_ALL=C TZ=UTC \
      xcrun swift -target "{{ swift_target }}" scripts/generate-mode-icons.swift \
        apple/MuuIME/ModeIconArtwork \
        "$scratch_dir/generated-a"
    env LC_ALL=ja_JP.UTF-8 TZ=Pacific/Honolulu \
      xcrun swift -target "{{ swift_target }}" scripts/generate-mode-icons.swift \
        apple/MuuIME/ModeIconArtwork \
        "$scratch_dir/generated-b"
    xcrun swiftc -target "{{ swift_target }}" \
      scripts/test-mode-icons.swift \
      -o "$scratch_dir/test-mode-icons"
    "$scratch_dir/test-mode-icons" --generated-asset-sets \
      "$scratch_dir/generated-a" "$scratch_dir/generated-b"
    for asset in apple/MuuIME/ModeIcons/*; do
      cmp "$asset" "$scratch_dir/generated-a/$(basename "$asset")" || exit 1
      cmp "$asset" "$scratch_dir/generated-b/$(basename "$asset")" || exit 1
      cmp \
        "$scratch_dir/generated-a/$(basename "$asset")" \
        "$scratch_dir/generated-b/$(basename "$asset")" || exit 1
    done
    iconutil \
      -c iconset \
      "$scratch_dir/generated-a/MuuIME.icns" \
      -o "$scratch_dir/MuuIME.iconset"
    for iconset_asset in \
      icon_16x16.png icon_16x16@2x.png \
      icon_32x32.png icon_32x32@2x.png \
      icon_128x128.png icon_128x128@2x.png \
      icon_256x256.png icon_256x256@2x.png; do
      test -s "$scratch_dir/MuuIME.iconset/$iconset_asset"
    done
    MUU_MODE_ICON_PREVIEW_DIR="$scratch_dir/previews" \
      "$scratch_dir/test-mode-icons" \
      apple/MuuIME/ModeIcons \
      apple/MuuIME/ModeIconArtwork \
      apple/MuuIME/Info.plist \
      apple/MuuIME.xcodeproj/project.pbxproj
    test -s "$scratch_dir/previews/mode-icons-light-dark-1x-2x.png"
    test -s "$scratch_dir/previews/mode-template-adaptive-1x-2x.png"
    test -s "$scratch_dir/previews/component-icon-light-dark-1x-2x.png"

# The icons and provenance inside the built Release app.
mode-icon-bundle-test: xcode-ci
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-mode-icon-bundle.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    xcrun swiftc -target "{{ swift_target }}" \
      scripts/test-mode-icons.swift \
      -o "$scratch_dir/test-mode-icons"
    MUU_EXPECTED_SOURCE_COMMIT="$(git rev-parse HEAD)" \
      MUU_EXPECTED_BUILD_NUMBER="$(git rev-list --count HEAD)" \
      MUU_EXPECTED_BUILD_VARIANT=standard \
      "$scratch_dir/test-mode-icons" \
      apple/MuuIME/ModeIcons \
      apple/MuuIME/ModeIconArtwork \
      apple/MuuIME/Info.plist \
      apple/MuuIME.xcodeproj/project.pbxproj \
      build/xcode/Build/Products/Release/MuuIME.app

# The audit tool's self-test only; nothing installed is read.
installed-mode-icon-audit-test:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-installed-mode-icon-audit.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    xcrun swiftc -target "{{ swift_target }}" \
      scripts/audit-installed-mode-icons.swift \
      -o "$scratch_dir/audit-installed-mode-icons"
    "$scratch_dir/audit-installed-mode-icons" --self-test

# Installer fault matrix with a temporary HOME and fake tools; never touches
# the installed IME. A timeout keeps its evidence under build/test-evidence.
installer-test:
    python3 scripts/test-install-dev.py
    python3 scripts/test-release-install.py

# Keep source-level tool dependencies within the macOS runner and CI setup.
ci-command-check:
    python3 scripts/test-ci-commands.py
    python3 scripts/check-ci-commands.py

candidate-contract-check:
    #!/usr/bin/env bash
    set -euo pipefail
    # Static guard only: navigation, action authority and grapheme behavior are
    # exercised by the Session, controller, candidate-surface and romaji tests.
    forbidden_apis='IMKCandidates|CandidateSyncPolicy|selectCandidateWithIdentifier|delegates_to_candidate_panel|candidateSelectionChanged|candidateSelected'
    # grep exits 1 for "no match", which is the only success; a match or any
    # scan error (exit 2) fails. The fixtures below check all three outcomes.
    check_candidate_apis() {
      local status
      if grep -r -n -E -I "$forbidden_apis" "$@"; then
        echo "obsolete IMK candidate production path remains" >&2
        return 1
      else
        status=$?
        if [[ "$status" -eq 1 ]]; then return 0; fi
        return "$status"
      fi
    }
    check_candidate_apis apple crates scripts
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-candidate-api-guard.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    # Positive control, then each retired name as a negative, then a scan
    # error on a missing path, which must not read as "nothing found".
    printf '%s\n' 'safe fixture' > "$scratch_dir/fixture.swift"
    check_candidate_apis "$scratch_dir/fixture.swift"
    for identifier in IMKCandidates CandidateSyncPolicy selectCandidateWithIdentifier \
      delegates_to_candidate_panel candidateSelectionChanged candidateSelected; do
      printf '%s\n' "$identifier" > "$scratch_dir/fixture.swift"
      if check_candidate_apis "$scratch_dir/fixture.swift" > "$scratch_dir/rejected.log" 2>&1; then
        echo "retired API fixture escaped the static guard: $identifier" >&2
        exit 1
      fi
      grep -F -q "$identifier" "$scratch_dir/rejected.log"
    done
    if check_candidate_apis "$scratch_dir/missing" > "$scratch_dir/error.log" 2>&1; then
      echo "static guard treated a scan error as success" >&2
      exit 1
    fi
    # Binary artwork and directory symlinks are outside this source scan.
    mkdir "$scratch_dir/tree" "$scratch_dir/outside"
    printf '\0%s\n' "$identifier" > "$scratch_dir/tree/artwork.icns"
    printf '%s\n' "$identifier" > "$scratch_dir/outside/fixture.swift"
    ln -s "$scratch_dir/outside" "$scratch_dir/tree/link"
    check_candidate_apis "$scratch_dir/tree"

# The committed header must equal a fresh cbindgen output, from a cbindgen
# chosen the same way as in `header`.
header-check:
    #!/usr/bin/env bash
    set -euo pipefail
    cbindgen_bin="${CBINDGEN-${CARGO_HOME:-${HOME}/.cargo}/bin/cbindgen}"
    if [[ -z "${CBINDGEN+x}" ]] && command -v cbindgen >/dev/null 2>&1; then
      cbindgen_bin="$(command -v cbindgen)"
    fi
    test -x "$cbindgen_bin" || { echo "cbindgen is required" >&2; exit 1; }
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-cbindgen.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    "$cbindgen_bin" --config cbindgen.toml --crate muu-ffi --output "$scratch_dir/muu_ffi.h"
    diff -u include/muu_ffi.h "$scratch_dir/muu_ffi.h"

# Both header recipes against fake generators: an explicit CBINDGEN wins over
# PATH, PATH wins over Cargo's bin, and a missing or empty explicit path fails
# rather than falling back.
cbindgen-selection-test:
    python3 scripts/test-cbindgen-selection.py

# The exported C surface and its declared ABI epoch advance together. Any
# additive/removal/signature change alters the normalized header tail hash.
ffi-abi-contract-check:
    #!/usr/bin/env bash
    set -euo pipefail
    expected_abi=13
    expected_surface_sha=b2a1ccf18b1314bdf138738c449548519441f8467ca82f46a0abb0e9e840fc1b
    actual_abi="$(sed -nE 's/^const ABI_VERSION: u32 = ([0-9]+);$/\1/p' crates/muu-ffi/src/lib.rs)"
    test "$actual_abi" = "$expected_abi"
    for pair in OK:0 NULL_HANDLE:1 BAD_ARG:2 CREATE_FAILED:3 DECODE_ERR:4 SESSION_INVALID:5 INTERNAL:6 CORE_INVALID:7 REVISION_MISMATCH:8 BUSY:9; do
      grep -E -q "^pub const MUU_RESULT_${pair%:*}: i32 = ${pair#*:};$" crates/muu-ffi/src/lib.rs
    done
    grep -F -q "static let expectedABIVersion: UInt32 = $expected_abi" apple/MuuIME/AppDelegate.swift
    actual_surface_sha="$(awk '/^uint32_t muu_abi_version/{emit=1} emit{print}' include/muu_ffi.h | shasum -a 256 | awk '{print $1}')"
    test "$actual_surface_sha" = "$expected_surface_sha" || {
      echo "C ABI surface changed without advancing the ABI contract" >&2
      exit 1
    }

third-party-license-bundle-test:
    scripts/test-third-party-license-bundle.sh

# The Cargo graph and registry archives are inspected offline. New license
# choices require review of the manifest; checks never regenerate it.
rust-code-license-test:
    python3 scripts/test-rust-code-licenses.py

rust-code-license-check: rust-code-license-test
    python3 scripts/rust_code_licenses.py --check

# Sysroot licenses are separate from Cargo's graph. The direct Xcode path
# additionally checks actual staticlib members after the Rust build.
rust-stdlib-license-test:
    python3 scripts/test-rust-std-licenses.py

rust-stdlib-license-check: rust-stdlib-license-test
    python3 scripts/rust_std_licenses.py --check

conversion-model-fetch-test:
    python3 scripts/test-fetch-conversion-lm.py

# Compile a C consumer (with NDEBUG) against the real header, link it to the
# Release staticlib that rust-arm64 builds, check its output, and run the
# checker's own negatives. The script owns compilation and scratch files.
c-abi-consumer-test: rust-arm64
    #!/usr/bin/env bash
    set -euo pipefail
    python3 scripts/test-c-abi-consumer.py run \
      --library build/libmuu_ffi.a --target "{{ swift_target }}"

release-resources-test:
    python3 scripts/test-release-resources.py

# Checks the built Release app. Each step rejects a different defect:
# - settings and resources: wrong configuration, flags, variant or data
#   (verify-release-resources.py; settings from the _xcode-release query);
# - Rust source commit: an app built from another commit, or not arm64-only;
# - licenses: missing or changed NOTICE and LEGAL files;
# - codesign --strict: bytes changed after the ad-hoc signature;
# - vtool minos: a Mach-O whose minimum macOS is not 15.0, whatever the plist says;
# - nm: test-only entries compiled in. This relies on symbols being present;
#   `xcodebuild build` does not strip them.
# Not a substitute for the distribution certificate or notarization.
release-bundle-check: xcode mode-icon-bundle-test c-abi-consumer-test
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p build/reviewed-release
    just _xcode-release settings > build/reviewed-release/build-settings.json
    just _release-app-check build/xcode/Build/Products/Release/MuuIME.app \
      build/reviewed-release/build-settings.json "$(git rev-parse HEAD)"

# Shared by the built-app gate and the extracted release ZIP. Keep the same
# checks on both paths; this recipe never builds or signs its input.
[private]
[positional-arguments]
_release-app-check app settings commit:
    #!/usr/bin/env bash
    set -euo pipefail
    app="$1"
    executable="$app/Contents/MacOS/MuuIME"
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-release-app.XXXXXX")"
    trap 'rm -r "$scratch_dir"' EXIT
    python3 scripts/verify-release-resources.py --app "$app" --repository . \
      --build-settings "$2"
    python3 scripts/verify-rust-source-commit.py "$app" "$3"
    scripts/verify-bundled-third-party-licenses.sh "$app"
    codesign --verify --deep --strict --verbose=2 "$app"
    xcrun vtool -show-build "$executable" > "$scratch_dir/build-version"
    awk '$1 == "minos" { if ($2 != "15.0") exit 1; count++ } END { if (count != 1) exit 1 }' "$scratch_dir/build-version"
    symbols="$(nm -j "$executable")"
    if [[ "$symbols" == *ForTesting* || "$symbols" == *MUU_TEST* ]]; then
      echo "Release executable contains a test-only entry" >&2
      exit 1
    fi

# Explicit local packaging only: no upload, tag creation or network request.
# Produces build/release/v<version>/ with the ZIP, its one-line SHA256SUMS,
# and the matching source installer for a version-pinned installation.
release-package:
    nice -n 19 env VECLIB_MAXIMUM_THREADS=2 CARGO_PROFILE_RELEASE_STRIP=false CARGO_BUILD_JOBS=1 python3 scripts/package-release.py --just "{{just_executable()}}"

release-package-test:
    python3 scripts/test-release-package.py

# The changelog extraction the release workflow uses for notes and title.
# No build, network or release service is involved.
release-notes-test:
    python3 scripts/test-release-notes.py

named-rust-gate-test:
    python3 scripts/run-named-rust-test.py --self-test

rust-source-commit-test:
    python3 scripts/test-rust-source-commit.py

# Full developer gate: the fast subset alone is never the complete check.
# Run before handing a change over. Contains the four layers below; not the
# signed bundle, dictionary reproducibility, AJIMEE quality or timing.
check: check-fast check-assets check-scale check-build

# When: every change. Contains synthetic Rust tests (asset paths removed),
# fmt, clippy, the Swift unit and controller tests that need no dictionary,
# stores, recorder, About, header/ABI checks, and the verifier and
# public-gate self-tests. Builds the Rust staticlib for the Swift tests.
# Not included: the public dictionary or models, release-mode scale tests,
# the Xcode app, the C consumer, the installer matrix, icon generation.
check-fast: ci-command-check public-conversion-gate-test rust-toolchain-absolute-test input-path-trace-test named-rust-gate-test rust-source-commit-test fmt-check rust-fast-test clippy application-lifecycle-test model-download-test application-startup-test session-create-policy-test candidate-surface-policy-test candidate-apply-fast-test commit-stream-delivery-test commit-stream-file-writer-test commit-stream-recorder-test user-dictionary-test imported-dictionary-test postal-code-test installed-mode-icon-audit-test candidate-contract-check cbindgen-selection-test header-check ffi-abi-contract-check rust-code-license-check rust-stdlib-license-check third-party-license-bundle-test conversion-model-fetch-test release-resources-test release-package-test release-notes-test

# When: changes to conversion, formats, the dictionary, models, learning or
# the ABI. Contains the asset Rust tests, the dictionary controller modes and
# the real-store ABI restarts. Needs `just dict-build` and both models from
# scripts/fetch-conversion-lm.sh. Not included: scale, the Xcode app,
# AJIMEE quality, timing.
check-assets: rust-assets-test candidate-apply-assets-test store-abi-test

# When: changes to limits, memory use or catalogs. Runs the release-mode
# scale group in test-support/rust-test-groups.json, each test in its own
# process: synthetic catalog/payload bounds and BLAS shape guards; no real assets.
check-scale: rust-scale-test

# When: Swift, FFI, bundle or installer changes. Contains the Xcode CI build
# (with source-commit, arm64 and NOTICE checks), icon generation, the C
# consumer against the Release staticlib, and the installer matrix. It also
# builds and verifies the dogfood artifact, so both variants keep compiling;
# release-bundle-check still inspects only the standard app. Not
# included: the strict signed-bundle inspection, dictionary reproducibility,
# AJIMEE quality.
check-build: mode-icon-test mode-icon-bundle-test c-abi-consumer-test installer-test xcode-dogfood

# When: on the final distribution candidate, and in CI. Contains `check`,
# the signed bundle inspection, dictionary reproducibility and the public
# 200-case zenz quality (173/200). Run fetch-public-evaluation-data first; this
# does not fetch. Not included: timing (see performance), a distribution
# certificate or notarization, real IMK/TIS behavior.
check-release: check release-bundle-check dict-reproducibility public-conversion-quality

# Run separately after coordinating a quiet measurement window.
# Contains the handle-event 8 ms p95 gate and three alternating edge/zenz
# rounds over the public 200 cases, written to a new MUU_PERFORMANCE_DIR.
# Not in `check` or CI.
performance: handle-latency public-conversion-performance

# Re-run after Rust syntax or dependency changes, separately from pinned 1.96.
# Uses rustup directly, not rust_toolchain.py, because it needs the other
# toolchain. Not in `check` or CI.
msrv-check:
    rustup run 1.85.0 cargo check --workspace --all-targets --locked

# Build the public system dictionary from the vendored Mozc source and the
# dict-src inputs.
dict-build output="build/mozc-oss.muudict":
    python3 scripts/rust_toolchain.py cargo run --release -p muu-dict-cli --bin muu-dict -- build-mozc \
      --source-dir third_party/mozc-oss/src/data/dictionary_oss \
      --sudachi-supplement dict-src/sudachidict-core-supplement.tsv \
      --pos-map dict-src/mozc-pos-map.toml \
      --unknown dict-src/muu-unknown.toml \
      --provenance dict-src/provenance.toml \
      --output "{{ output }}"

# Two builds must be identical and match the pinned SHA-256, and each golden
# reading's baseline view must keep its hash.
dict-reproducibility:
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-dict-repro.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    for output in "$scratch_dir/first.muudict" "$scratch_dir/second.muudict"; do
      python3 scripts/rust_toolchain.py cargo run --release -p muu-dict-cli --bin muu-dict -- build-mozc \
        --source-dir third_party/mozc-oss/src/data/dictionary_oss \
        --sudachi-supplement dict-src/sudachidict-core-supplement.tsv \
        --pos-map dict-src/mozc-pos-map.toml \
        --unknown dict-src/muu-unknown.toml \
        --provenance dict-src/provenance.toml \
        --output "$output"
    done
    cmp "$scratch_dir/first.muudict" "$scratch_dir/second.muudict"
    expected="$(cut -d ' ' -f 1 dict-src/mozc-oss.sha256)"
    actual="$(shasum -a 256 "$scratch_dir/first.muudict" | cut -d ' ' -f 1)"
    test "$actual" = "$expected" || {
      echo "dictionary SHA-256 regression: expected $expected, got $actual" >&2
      exit 1
    }
    while IFS=$'\t' read -r expected_view reading; do
      [[ -z "$expected_view" || "$expected_view" == \#* ]] && continue
      actual_view="$(
        "${CARGO_TARGET_DIR:-target}/release/muu-dict" baseline \
          --dictionary "$scratch_dir/first.muudict" \
          --reading "$reading" |
          xxd -r -p |
          shasum -a 256 |
          cut -d ' ' -f 1
      )"
      test "$actual_view" = "$expected_view" || {
        echo "baseline golden regression for $reading: expected $expected_view, got $actual_view" >&2
        exit 1
      }
    done < dict-src/mozc-baseline-golden.sha256
    echo "dictionary reproducibility verified: $actual"

# The handle_event latency gate (key and conversion p95 under 8 ms). Raw
# samples go to build/measurements and their hash is printed. Part of
# `performance`, so run it only in a quiet window.
handle-latency: rust-arm64 dict-build
    #!/usr/bin/env bash
    set -euo pipefail
    scratch_dir="$(mktemp -d "${TMPDIR:-/tmp}/muu-handle-latency.XXXXXX")"
    trap 'rm -rf "$scratch_dir"' EXIT
    artifact_dir="build/measurements"
    mkdir -p "$artifact_dir"
    raw_timing="$artifact_dir/handle-event-timing.jsonl"
    xcrun swiftc -target "{{ swift_target }}" -O \
      -import-objc-header include/muu_ffi.h \
      apple/MuuIME/ProtocolModels.swift \
      scripts/measure-session-event.swift \
      -L build -lmuu_ffi -framework Accelerate \
      -o "$scratch_dir/measure-session-event"
    MUU_TIMING_RAW_PATH="$raw_timing" \
      "$scratch_dir/measure-session-event" build/mozc-oss.muudict
    shasum -a 256 "$raw_timing"

# GitHub Actions runs this after fetching the models and the AJIMEE input.
ci: check-release

# Install only the reviewed build, checked against its commit, build number,
# variant, executable hash and signature report.
install: reviewed-executable-authority
    scripts/install-dev.sh \
      --source-app build/xcode/Build/Products/Release/MuuIME.app \
      --expected-commit "$(git rev-parse HEAD)" \
      --expected-build "$(git rev-list --count HEAD)" \
      --expected-variant standard \
      --expected-executable-sha256-file build/reviewed/MuuIME-executable.sha256 \
      --expected-codesign-json build/reviewed/MuuIME-codesign.json

uninstall:
    scripts/uninstall-dev.sh

reload:
    scripts/reload-ime.sh

# The driver's own negatives, with synthetic rows; no data or model. In check-fast.
public-conversion-gate-test:
    python3 scripts/test-public-conversion.py self-test

# Network access is explicit; quality/performance never fetch their own input.
fetch-public-evaluation-data:
    python3 scripts/test-public-conversion.py fetch --output build/test-assets/ajimee-evaluation-items.json

# The Session example that the public conversion runs drive over stdin.
public-conversion-consumer:
    python3 scripts/rust_toolchain.py cargo build --release -p muu-session --example public_conversion

# zenz on all 200 public cases must keep exactly 173 correct (NFKC match).
# Each run writes a new evidence directory; its timings are not measurements.
public-conversion-quality: public-conversion-gate-test public-conversion-consumer dict-build
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p build/test-evidence/public-conversion
    # mktemp only picks a unique name: the driver refuses an existing output
    # directory, so the new one is removed again before the run.
    output="$(mktemp -d "$PWD/build/test-evidence/public-conversion/quality.XXXXXX")"
    rmdir "$output"
    echo "Public quality evidence: $output"
    python3 scripts/test-public-conversion.py run \
      --consumer "${CARGO_TARGET_DIR:-target}/release/examples/public_conversion" \
      --input build/test-assets/ajimee-evaluation-items.json --output "$output" \
      --dictionary "${MUU_REAL_DICT_PATH:-$PWD/build/mozc-oss.muudict}" \
      --model-dir "${MUU_ZENZ_MODEL_DIR:-$HOME/Library/Application Support/MuuIME/Models/zenz-v3.2-small}" \
      --model zenz --purpose quality

# Quiet-window timing only; the directory must be new so rounds never mix.
public-conversion-performance: public-conversion-consumer dict-build
    #!/usr/bin/env bash
    set -euo pipefail
    output="${MUU_PERFORMANCE_DIR:?set a new explicit output directory after coordinating a quiet window}"
    test ! -e "$output"
    mkdir -p "$output"
    for round in 1 2 3; do
      for model in edge zenz; do
        if [[ "$model" == edge ]]; then
          model_dir="${MUU_LM_MODEL_DIR:-$HOME/Library/Application Support/MuuIME/Models/kanakanji-edge-33m}"
        else
          model_dir="${MUU_ZENZ_MODEL_DIR:-$HOME/Library/Application Support/MuuIME/Models/zenz-v3.2-small}"
        fi
        python3 scripts/test-public-conversion.py run \
          --consumer "${CARGO_TARGET_DIR:-target}/release/examples/public_conversion" \
          --input build/test-assets/ajimee-evaluation-items.json --output "$output/$model-$round" \
          --dictionary "${MUU_REAL_DICT_PATH:-$PWD/build/mozc-oss.muudict}" \
          --model-dir "$model_dir" --model "$model" --purpose performance
      done
    done
