#!/usr/bin/env bash
set -euo pipefail
repo=$(git rev-parse --show-toplevel)
fixtures=$(cd "$(dirname "$0")" && pwd)
evidence=$(realpath -m "${EVIDENCE:?set an evidence directory}")
export EVIDENCE="$evidence"
mkdir -p "$evidence"

product=3deb4a3e215feb16c94a9bdd1b61f8166aef822c
prior_elf=9b19e0d750cdb1a7612cb1cd96a249c9f9dfad89ef85cf919654ddb0928ca5c1
target=x86_64-unknown-linux-gnu
scratch=$(mktemp -d "${RUNNER_TEMP:-/tmp}/wfc-tmux-r10-XXXXXXXX")
cleanup() {
    local code=$?
    trap - EXIT
    if test -e "$scratch/product/.git"; then
        git -C "$repo" worktree remove --force "$scratch/product" || code=1
    fi
    rm -rf -- "$scratch"
    exit "$code"
}
trap cleanup EXIT
git -C "$repo" worktree add --detach "$scratch/product" "$product"
cd "$scratch/product"
if test -n "${R10_ELF:-}"; then
    bin=$(realpath "$R10_ELF")
    printf '%s  %s\n' "$prior_elf" "$bin" | sha256sum --check --strict
    printf 'Reusing original runner ELF\n' | tee "$evidence/product-build.log"
else
    zlib=$(realpath "${1:?provide pinned zlib for a remote rebuild}")
    printf '%s  %s\n' 17e88863f3600672ab49182f217281b6fc4d3c762bde361935e436a95214d05c "$zlib" |
        sha256sum --check --strict
    export WFC_BUILD_ID="$product" CARGO_TARGET_DIR="$scratch/target" CARGO_BUILD_JOBS=1
    export ZIG_GLOBAL_CACHE_DIR="$scratch/zig-cache"
    {
        printf 'Rebuilding only the fixed product\n'
        sha256sum --check --strict vendor/INPUTS.sha256
        python3 tools/prepare.py --zlib-archive "$zlib"
        cargo build --locked -j1 --target "$target" --release --bin wfc
    } 2>&1 | tee "$evidence/product-build.log"
    bin="$CARGO_TARGET_DIR/$target/release/wfc"
fi
{
    "$bin" --version
    sha256sum "$bin"
    git show -s --format='%H %T %P %s'
} | tee "$evidence/identity.log"
python3 "$fixtures/reap.py" python3 -Bu "$fixtures/failed-case-repro.py" "$bin" 2>&1 |
    tee "$evidence/diagnostic.log"
