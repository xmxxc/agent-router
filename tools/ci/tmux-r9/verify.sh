#!/usr/bin/env bash
# Run on the Linux CI runner, starting from the checked-out CI branch.
set -euo pipefail
repo=$(git rev-parse --show-toplevel)
fixtures=$(cd "$(dirname "$0")" && pwd)
zlib=$(realpath "${1:?provide the verified zlib archive}")
evidence=$(realpath -m "${EVIDENCE:?define an evidence directory}")
target=x86_64-unknown-linux-gnu
product=3deb4a3e215feb16c94a9bdd1b61f8166aef822c
integration=addfd51b5c0457e3d178ad1341c71899c9d437a5
engine=b201965e19eb33c255bfc3bd4de9d22f0086df4d
public=7144bf3fb138d7c7ee79f56f3f5f439b6e633323
herdr_source=1861bbc896e5be668bf60c4179613e8e3c2c564b
r5=2aa82de2f2f34053e778465a3d7ac88a635929db
r7=1a665c491c939022fb511dc73351e888508b5a88

mkdir -p "$evidence"
scratch=$(mktemp -d "${RUNNER_TEMP:-/tmp}/wfc-tmux-r9-XXXXXXXX")
cleanup() {
    local status=$?
    trap - EXIT
    for name in product r5 r7; do
        if test -e "$scratch/$name/.git"; then
            git -C "$repo" worktree remove --force "$scratch/$name" || status=1
        fi
    done
    rm -rf -- "$scratch"
    exit "$status"
}
trap cleanup EXIT
export PYTHONDONTWRITEBYTECODE=1
export ZIG_GLOBAL_CACHE_DIR="$scratch/zig-cache"

{
    for source in "$integration" "$engine" "$public" "$herdr_source" "$r5" "$r7"; do
        git -C "$repo" merge-base --is-ancestor "$source" "$product"
        git -C "$repo" show -s --format='%H %T %P %s' "$source"
    done
    test "$(git -C "$repo" rev-parse "$product^1")" = "$integration"
    test "$(git -C "$repo" rev-parse "$product^2")" = "$engine"
    git -C "$repo" show -s --format='%H %T %P %s' "$product"
    git -C "$repo" merge-base --is-ancestor "$product" HEAD
    git -C "$repo" diff --exit-code "$integration" "$product" -- src/workflow.rs src/workflow
    printf '%s  %s\n' 17e88863f3600672ab49182f217281b6fc4d3c762bde361935e436a95214d05c "$zlib" |
        sha256sum --check --strict
} 2>&1 | tee "$evidence/sources.log"

build() {
    local name=$1 revision=$2 profile=$3 dir=$4
    (
        git -C "$repo" worktree add --detach "$scratch/$name" "$revision"
        cd "$scratch/$name"
        test "$(git rev-parse HEAD)" = "$revision"
        test -z "$(git status --porcelain)"
        git show -s --format='%H %T %P %s'
        sha256sum vendor/INPUTS.sha256
        sha256sum --check --strict vendor/INPUTS.sha256
        cargo fmt --all -- --check
        python3 tools/prepare.py --zlib-archive "$zlib"
        export WFC_BUILD_ID="$revision" CARGO_TARGET_DIR="$scratch/target/$name"
        cargo build --locked --target "$target" --profile "$profile" --bin wfc
        binary="$CARGO_TARGET_DIR/$target/$dir/wfc"
        version=$("$binary" --version)
        printf '%s\n' "$version"
        [[ "$version" == *"(Rust, $target, $dir, build $revision)" ]]
        sha256sum "$binary"
    ) 2>&1 | tee "$evidence/$name-build.log"
}

# Old binaries generate compatibility fixtures from the fixed sources.
build product "$product" release release
build r5 "$r5" release release
build r7 "$r7" dev debug
binary="$scratch/target/product/$target/release/wfc"
old_r5="$scratch/target/r5/$target/release/wfc"
old_r7="$scratch/target/r7/$target/debug/wfc"
{
    for path in "$binary" "$old_r5" "$old_r7"; do
        "$path" --version
        sha256sum "$path"
    done
} | tee "$evidence/binaries.log"

cd "$scratch/product"
failed=0
run_case() {
    local label=$1
    shift
    if python3 "$fixtures/reap.py" python3 -Bu "$@" 2>&1 | tee "$evidence/$label.log"; then
        printf 'PASS %s\n' "$label" | tee -a "$evidence/results.log"
    else
        printf 'FAIL %s\n' "$label" | tee -a "$evidence/results.log"
        failed=1
    fi
}

run_case tmux-r9 tools/check_tmux.py "$binary" -v \
    Backend.test_native_socket_alias_preserves_server_identity \
    Backend.test_legacy_socket_alias_can_identify_original_pane \
    Backend.test_legacy_cancelled_reservation_requires_empty_original_endpoint \
    Backend.test_keyed_close_can_confirm_prepared_launch_without_claiming_revocation \
    Backend.test_retry_same_dispatch_after_launcher_could_not_start \
    Backend.test_revoke_and_close_a_retained_session_after_prestart_cancellation \
    Backend.test_plain_absence_rejects_conflicting_cancellation
run_case tmux-r7-upgrade "$fixtures/upgrade-r9.py" "$old_r7" "$binary"
cc -shared -fPIC -o "$scratch/park.so" "$fixtures/park.c" -ldl
export WFC_R9_CUT_LIB="$scratch/park.so"
run_case tmux-r9-pre-submission "$fixtures/cut.py" "$binary"
run_case tmux-public-r6 "$fixtures/public-resources.py" "$binary" "$old_r5" -v
herdr="$scratch/herdr"
if (
    curl --fail --show-error --silent --location --retry 3 --max-time 120 \
        https://github.com/herdrdev/herdr/releases/download/v0.9.5/herdr-linux-x86_64 -o "$herdr" || exit 1
    printf '%s  %s\n' 14d9e8bc8b4a9e0118d77c32266f76d461f09b40342eb23752960bd89f59929d "$herdr" |
        sha256sum --check --strict || exit 1
    chmod 700 "$herdr" || exit 1
    version=$("$herdr" --version) || exit 1
    printf '%s\n' "$version"
    test "$version" = 'herdr 0.9.5' || exit 1
    sha256sum "$herdr"
) 2>&1 | tee "$evidence/herdr-input.log"; then
    run_case herdr-public tests/herdr_public.py "$herdr" "$binary" -v \
        PublicLifecycle.test_dispatch_current_execution_done_and_close \
        PublicLifecycle.test_named_status_during_done_then_exit \
        PublicLifecycle.test_conflicting_stop_evidence_cannot_close_live_writers \
        PublicLifecycle.test_old_worker_data_does_not_override_verified_resource_close
else
    printf '%s\n' 'FAIL herdr-input' | tee -a "$evidence/results.log"
    failed=1
fi
exit "$failed"
