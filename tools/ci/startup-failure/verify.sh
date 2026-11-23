#!/bin/bash
# From the frozen Linux product checkout, with the candidate already built.
set -euo pipefail
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
    python3 "$reap" "$@" 2>&1 | tee "$EVIDENCE/$group.log" || status=$?
    printf '%s exit=%s\n' "$group" "$status" | tee -a "$EVIDENCE/results.log"
    if (( status != 0 )); then
        failed=1
    fi
}

run_group controlled python3 -Bu tests/startup_failure.py "$binary" -v \
    StartupFailure.test_controlled_failures_and_completion
run_group hosted-tmux python3 -Bu tests/startup_failure.py "$binary" -v \
    StartupFailure.test_hosted_failures
run_group scope-relay python3 -Bu tests/relay.py "$binary" -v \
    Relay.test_startup_failure_uses_the_existing_scope_and_ack

# Includes the existing dispatch, receipt and relay regressions and the
# startup publication test for concurrent/repeated observations of one attempt.
run_group workspace cargo test --locked --release --workspace --target "$TARGET" -- --test-threads=1
exit "$failed"
