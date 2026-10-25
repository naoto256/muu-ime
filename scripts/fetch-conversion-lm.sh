#!/usr/bin/env bash
# Downloads one fixed conversion-model profile; neither model is bundled.
# Edge's tokenizer/data licenses and zenz's unverified attribution are
# described separately in THIRD_PARTY_LICENSES/models. This does not enable
# the model: MuuConversionLmEnabled remains an explicit opt-in preference.
set -euo pipefail

model="zenz"
# Preserve the old one-positional-argument edge destination invocation.
if [ "${1:-}" = "--model" ]; then
    [ "$#" -ge 2 ] || { echo "--model requires zenz or edge" >&2; exit 2; }
    model="$2"
    shift 2
elif [ "$#" -gt 0 ]; then
    model="edge"
fi
[ "$#" -le 1 ] || { echo "usage: $0 [--model zenz|edge] [destination]" >&2; exit 2; }
case "$model" in
    zenz)
        repo="Miwa-Keita/zenz-v3.2-small-gguf"
        revision="c67e03e07d215c869f591b274c1631170d3e11fe"
        profile="zenz-v3.2-small"
        ;;
    edge)
        repo="yuuki14202028/kanakanji-edge-33m"
        revision="da588df2db17e5ab69316986823806ec5f911450"
        profile="kanakanji-edge-33m"
        ;;
    *) echo "unknown model: $model" >&2; exit 2 ;;
esac
destination="${1:-$HOME/Library/Application Support/MuuIME/Models/$profile}"

# Each file is downloaded to `<name>.partial` and renamed over the final name
# only after its SHA-256 matches the pin, so an existing good file is never
# replaced by unverified bytes. A file that already matches is not fetched.
# A failed download leaves the partial for the next run to overwrite; a hash
# mismatch removes it. Pinned revisions and hashes change only by review.
fetch() {
    local name="$1" expected="$2" target="$destination/$1"
    if [ -f "$target" ] && [ "$(shasum -a 256 "$target" | awk '{print $1}')" = "$expected" ]; then
        return
    fi
    curl --fail --location --silent --show-error \
        "https://huggingface.co/$repo/resolve/$revision/$name" --output "$target.partial"
    if [ "$(shasum -a 256 "$target.partial" | awk '{print $1}')" != "$expected" ]; then
        rm -f "$target.partial"
        echo "checksum mismatch for $name" >&2
        exit 1
    fi
    mv "$target.partial" "$target"
}

mkdir -p "$destination"
if [ "$model" = "zenz" ]; then
    fetch ggml-model-Q5_K_M.gguf 29c223d4c23327b80fd13ebb5ab2555057a46317997d5da391584ffbef0db673
else
    fetch model.safetensors 038af5f10e8b289c647c5a2d0dccf2efcf3f05c954c4e561c126a550fc2f2e2f
    fetch tokenizer.json 75318016885462c2f56a41deb19caa8eebf70c70fee466d8ce06a783d5df4c82
fi
echo "$destination"
