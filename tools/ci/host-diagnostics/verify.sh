#!/bin/bash
# Run in the frozen product checkout after the one release build.
set -euo pipefail
fixtures=$(cd "$(dirname "$0")" && pwd)
binary=$(realpath "target/$TARGET/release/wfc")
version=$("$binary" --version)
[[ "$version" == *"(Rust, $TARGET, release, build $PRODUCT_SHA)" ]]
test "$(git rev-parse HEAD)" = "$PRODUCT_SHA"
test "$(git rev-parse 'HEAD^{tree}')" = "$PRODUCT_TREE"

failed=0
run_group() {
    local group=$1 expected=$2 status=0
    shift 2
    local logfile="$EVIDENCE/$group.log"
    {
        printf 'selector:'
        printf ' %q' "$@"
        printf '\n'
        printf 'binary=%s\nversion=%s\n' "$binary" "$version"
        sha256sum "$binary"
        python3 "$fixtures/reap.py" "$@"
    } 2>&1 | tee "$logfile" || status=$?
    if (( status == 0 )); then
        grep -Eq "^Ran $expected tests? in " "$logfile" &&
            grep -Fxq "OK" "$logfile" &&
            grep -Fxq "Own descendants reaped: ECHILD, extra cleanup signals: 0" "$logfile" ||
            status=1
    fi
    printf '%s exit=%s\n' "$group" "$status" | tee -a "$EVIDENCE/results.log"
    if (( status != 0 )); then
        failed=1
    fi
}

case "${1:-}" in
    combination)
        run_group runtime-tmux 5 python3 -Bu tests/relay.py "$binary" -v \
            Relay.test_tmux_bound_absence_is_reported_and_recovers \
            Relay.test_unverified_tmux_timeout_and_abandoned_relay \
            Relay.test_codex_unicode_appended_activity \
            Relay.test_runtime_scope_takeover_fences_the_inflight_read \
            Relay.test_runtime_notice_through_the_private_queue
        run_group owner 2 python3 -Bu tests/owner.py "$binary" -v \
            Owner.test_takeover_fences_wait_and_ack_but_worker_keeps_its_callback \
            Owner.test_takeover_waits_for_scoped_delivery_and_unregisters_detached_relay
        run_group reclaim 3 env WFC_TEST_STICKY_OWNERSHIP=1 python3 -Bu tools/check_tmux.py "$binary" -v \
            Backend.test_public_reclaim_requires_sticky_ownership \
            Backend.test_public_reclaim_stops_on_scope_authority_loss \
            Backend.test_public_reclaim_rechecks_selected_execution_under_scope_lock
        run_group relay-opencode 2 python3 -Bu tests/opencode_relay.py "$binary" -v \
            OpenCodeRelay.test_runtime_notice_preserves_the_explicit_opencode_target \
            OpenCodeRelay.test_204_detached_preserves_batch_order_scope_and_session
        ;;
    herdr)
        test -x "${HERDR_BIN:?verified Herdr binary required}"
        run_group runtime-herdr 2 python3 -Bu tests/herdr_public.py "$HERDR_BIN" "$binary" -v \
            PublicLifecycle.test_runtime_read_evidence_and_independent_recovery \
            PublicLifecycle.test_close_after_scope_takeover_fences_stale_herdr_controls
        ;;
    *) printf 'Unsupported host diagnostics group: %s\n' "${1:-}" >&2; exit 2 ;;
esac
exit "$failed"
