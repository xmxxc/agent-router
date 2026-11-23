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
        run_group runtime-tmux 7 python3 -Bu tests/relay.py "$binary" -v \
            Relay.test_tmux_bound_absence_is_reported_and_recovers \
            Relay.test_unverified_tmux_timeout_and_abandoned_relay \
            Relay.test_codex_unicode_appended_activity \
            Relay.test_runtime_scope_takeover_fences_the_inflight_read \
            Relay.test_runtime_reply_fences_an_already_captured_fault \
            Relay.test_runtime_notice_through_the_private_queue \
            Relay.test_timer_redraw_does_not_delay_stall
        # Import the fixture so unittest receives the method selectors.
        run_group startup-recovery 1 python3 -Bu tests/startup_failure.py "$binary" -v \
            StartupFailure.test_startup_notice_recovers_after_dispatch_interruption
        run_group startup-codex 1 python3 -Bu tests/relay.py "$binary" -v \
            Relay.test_startup_failure_uses_the_existing_scope_and_ack
        run_group owner 2 env PYTHONPATH=tests python3 -Bu -c '
import unittest
import owner

unittest.main(module=owner)
' "$binary" -v \
            Owner.test_takeover_fences_wait_and_ack_but_worker_keeps_its_callback \
            Owner.test_takeover_waits_for_scoped_delivery_and_unregisters_detached_relay
        run_group reclaim 3 env WFC_TEST_STICKY_OWNERSHIP=1 python3 -Bu tools/check_tmux.py "$binary" -v \
            Backend.test_public_reclaim_requires_sticky_ownership \
            Backend.test_public_reclaim_stops_on_scope_authority_loss \
            Backend.test_public_reclaim_rechecks_selected_execution_under_scope_lock
        run_group resume 3 bash "$fixtures/verify-resume.sh" "$binary" -v \
            Backend.test_public_resume_preserves_conversation_rounds_and_callback_fences \
            Backend.test_public_resume_after_reclaim_preserves_history \
            Backend.test_runtime_deadline_restarts_for_each_resumed_attempt
        run_group reply-tmux 2 python3 -Bu tools/check_tmux.py "$binary" -v \
            Backend.test_public_reply_requires_confirmed_tmux_chat_composer \
            Backend.test_public_reply_keeps_claim_when_tmux_draft_reappears
        run_group relay-opencode 2 python3 -Bu tests/opencode_relay.py "$binary" -v \
            OpenCodeRelay.test_runtime_notice_preserves_the_explicit_opencode_target \
            OpenCodeRelay.test_204_detached_preserves_batch_order_scope_and_session
        ;;
    herdr)
        test -x "${HERDR_BIN:?verified Herdr binary required}"
        run_group runtime-herdr 3 python3 -Bu tests/herdr_public.py "$HERDR_BIN" "$binary" -v \
            PublicLifecycle.test_runtime_read_evidence_and_independent_recovery \
            PublicLifecycle.test_close_after_scope_takeover_fences_stale_herdr_controls \
            PublicLifecycle.test_public_reply_distinguishes_zero_write_deadline_and_lost_receipt
        ;;
    *) printf 'Unsupported host diagnostics group: %s\n' "${1:-}" >&2; exit 2 ;;
esac
exit "$failed"
