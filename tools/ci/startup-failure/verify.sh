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

case "${1:-}" in
    classification-tmux)
        run_group classification-codex python3 -Bu tests/startup_failure.py "$binary" -v \
            StartupFailure.test_codex_unicode_appended_activity
        run_group lifecycle-tmux python3 -Bu tests/relay.py "$binary" -v \
            Relay.test_unverified_tmux_timeout_and_abandoned_relay
        ;;
    classification-herdr)
        test -x "${HERDR_BIN:?verified Herdr binary required}"
        run_group classification-herdr python3 -Bu tests/herdr_public.py "$HERDR_BIN" "$binary" -v \
            PublicLifecycle.test_runtime_read_evidence_and_independent_recovery
        ;;
    *) printf 'Unsupported classification test group: %s\n' "${1:-}" >&2; exit 2 ;;
esac
exit "$failed"
