#!/usr/bin/env bash
set -euo pipefail
repo=$(git rev-parse --show-toplevel)
fixtures=$(cd "$(dirname "$0")" && pwd)
evidence=$(realpath -m "${EVIDENCE:?set an evidence directory}")
zlib=$(realpath "${1:?provide the pinned zlib archive}")
product=3a091e22d5cbb00ed3a13e980811fd67ba1d6718
tree=dd476e8053b1cffff6fca70804445ec665e2dcba
old=3deb4a3e215feb16c94a9bdd1b61f8166aef822c
fix=c5be2dfffe8c221e1f2e61c22c63d75362af5384
target=x86_64-unknown-linux-gnu
export EVIDENCE="$evidence" CARGO_BUILD_JOBS=1 WFC_BUILD_ID="$product" PYTHONDONTWRITEBYTECODE=1
mkdir -p "$evidence"

scratch=$(mktemp -d "${RUNNER_TEMP:-/tmp}/wfc-tmux-r11-XXXXXXXX")
cleanup() {
    local code=$?
    trap - EXIT
    if test -e "$scratch/product/.git"; then
        git -C "$repo" worktree remove --force "$scratch/product" || code=1
    fi
    python3 -c 'import shutil,sys; shutil.rmtree(sys.argv[1])' "$scratch" || code=1
    exit "$code"
}
trap cleanup EXIT
git -C "$repo" worktree add --detach "$scratch/product" "$product"
cd "$scratch/product"
export CARGO_TARGET_DIR="$scratch/target" ZIG_GLOBAL_CACHE_DIR="$scratch/zig-cache"
{
    test "$(git rev-parse HEAD)" = "$product"
    test "$(git rev-parse 'HEAD^{tree}')" = "$tree"
    test "$(git rev-parse HEAD^1)" = "$old"
    test "$(git rev-parse HEAD^2)" = "$fix"
    test "$(git diff --name-only "$old" HEAD)" = tools/check_tmux.py
    git show -s --format='%H %T %P %s'
    sha256sum --check --strict vendor/INPUTS.sha256
    printf '%s  %s\n' 17e88863f3600672ab49182f217281b6fc4d3c762bde361935e436a95214d05c "$zlib" |
        sha256sum --check --strict
    python3 tools/prepare.py --zlib-archive "$zlib"
    cargo build --locked -j1 --target "$target" --release --bin wfc
} 2>&1 | tee "$evidence/product-build.log"
bin="$CARGO_TARGET_DIR/$target/release/wfc"
{
    "$bin" --version
    sha256sum "$bin" tools/check_tmux.py
    git show -s --format='%H %T %P %s'
} | tee "$evidence/identity.log"
python3 "$fixtures/reap.py" python3 -Bu "$fixtures/probe.py" "$bin" 2>&1 |
    tee "$evidence/fixture.log"
