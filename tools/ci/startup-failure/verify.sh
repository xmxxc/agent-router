#!/bin/bash
# From the frozen Linux product checkout, with the candidate already built.
set -euo pipefail
selection=${1:-all}
case "$selection" in
    all) groups='controlled hosted-tmux scope-relay workspace' ;;
    hosted-tmux) groups='hosted-tmux' ;;
    startup-review) groups='controlled hosted-tmux startup-unit' ;;
    runtime-review) groups='controlled hosted-tmux startup-unit runtime-tmux scope-relay' ;;
    runtime-herdr) groups='runtime-herdr' ;;
    *) printf 'Unsupported test group: %s\n' "$selection" >&2; exit 2 ;;
esac
fixtures=$(cd "$(dirname "$0")" && pwd)
binary=$(realpath "target/$TARGET/release/wfc")
reap="$fixtures/reap.py"
version=$("$binary" --version)
[[ "$version" == *"(Rust, $TARGET, release, build $PRODUCT_SHA)" ]]
test "$(git rev-parse HEAD)" = "$PRODUCT_SHA"
test "$(git rev-parse 'HEAD^{tree}')" = "$PRODUCT_TREE"

failed=0
run_group() {
    local group=$1 status=0
    shift
    if [[ " $groups " != *" $group "* ]]; then
        return 0
    fi
    {
        printf 'selector:'
        printf ' %q' "$@"
        printf '\n'
        printf 'binary=%s\nversion=%s\n' "$binary" "$version"
        sha256sum "$binary"
        python3 "$reap" "$@"
    } 2>&1 | tee "$EVIDENCE/$group.log" || status=$?
    printf '%s exit=%s\n' "$group" "$status" | tee -a "$EVIDENCE/results.log"
    if (( status != 0 )); then
        failed=1
    fi
}

run_group controlled python3 -Bu tests/startup_failure.py "$binary" -v \
    StartupFailure.test_controlled_failures_and_completion \
    StartupFailure.test_close_distinguishes_natural_exit_and_cancellation
run_group hosted-tmux python3 -Bu tests/startup_failure.py "$binary" -v \
    StartupFailure.test_hosted_failures \
    StartupFailure.test_tmux_zero_follows_the_create_confirmation \
    StartupFailure.test_script_lookup_permission_failure
run_group startup-unit cargo test --locked --release --target "$TARGET" --bin wfc \
    startup -- --test-threads=1
run_group runtime-tmux python3 -Bu tests/startup_failure.py "$binary" -v \
    StartupFailure.test_host_runtime_observations
run_group scope-relay python3 -Bu tests/relay.py "$binary" -v \
    Relay.test_startup_failure_uses_the_existing_scope_and_ack \
    Relay.test_runtime_notice_through_the_private_queue
if [[ "$selection" == runtime-herdr ]]; then
    test -x "${HERDR_BIN:?verified Herdr binary required}"
    run_group runtime-herdr python3 -Bu tests/herdr_public.py "$HERDR_BIN" "$binary" -v \
        PublicLifecycle.test_runtime_notice_reads_the_bound_private_pane \
        PublicLifecycle.test_dispatch_current_execution_done_and_close
fi

# Includes the existing dispatch, receipt and relay regressions and the
# startup publication test for concurrent/repeated observations of one attempt.
run_group workspace cargo test --locked --release --workspace --target "$TARGET" -- --test-threads=1
exit "$failed"
