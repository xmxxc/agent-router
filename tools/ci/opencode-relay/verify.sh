#!/bin/bash
# Run each relay and ownership suite against the same pinned binary.
set -euo pipefail
ci_dir=$(cd "$(dirname "$0")" && pwd)
binary=$(realpath "${RELAY_TEST_BINARY:-target/$TARGET/release/wfc}")
python=$(command -v python3)
test "$(git rev-parse HEAD)" = "$PRODUCT_SHA"
test "$(git rev-parse 'HEAD^{tree}')" = "$PRODUCT_TREE"
sha256sum --check --strict "$EVIDENCE/binary.sha256"
test "$(id -u)" -ne 0

first_failure=none
run_suite() {
    local group=$1 script=$2 selector=$3 expected=$4
    local verdict=pass log="$EVIDENCE/$group.log" work status=0 selection=not-confirmed
    work=$(mktemp -d "$RUNNER_TEMP/wfc-relay-$group.XXXXXXXX")
    mkdir "$work/home" "$work/tmp"
    printf 'suite=%s selector=%s expected=%s\n' "$group" "$selector" "$expected" | tee "$log"
    env -i PATH="$PATH" HOME="$work/home" TMPDIR="$work/tmp" LANG=C.UTF-8 TERM=xterm \
        PYTHONUTF8=1 PYTHONDONTWRITEBYTECODE=1 \
        WFC_STATE="$work/state" WFC_TMUX_SOCKET="$work/tmux.sock" \
        "$python" -Bu "$ci_dir/reap.py" timeout --kill-after=5s 180s \
        "$python" -Bu "$ci_dir/run_selected.py" "$script" "$binary" "$selector" "$expected" \
        2>&1 | tee -a "$log" || status=$?
    local count_label="$expected tests"
    if (( expected == 1 )); then
        count_label="$expected test"
    fi
    if grep -Eq "^Ran $count_label in " "$log"; then
        selection=confirmed
    fi
    if (( status != 0 )) || [[ "$selection" != confirmed ]] ||
        ! grep -Fxq OK "$log" ||
        ! grep -Fxq 'Own descendants reaped: ECHILD, extra cleanup signals: 0' "$log"; then
        verdict=fail
        if [[ "$first_failure" == none ]]; then
            first_failure=$group
        fi
    fi
    printf '%s exit=%s expected=%s selection=%s verdict=%s\n' "$group" "$status" "$expected" "$selection" "$verdict" |
        tee -a "$EVIDENCE/results.log"
}

run_opencode_suites() {
    run_suite opencode tests/opencode_relay.py OpenCodeRelay 6
    run_suite managed-opencode "$ci_dir/managed_owner.py" ManagedOpenCode.test_current_owner_delivers_and_old_owner_cannot_claim 1
}

run_owner_suites() {
    run_suite owner-takeover tests/owner.py Owner.test_takeover_waits_for_scoped_delivery_and_unregisters_detached_relay 1
    run_suite owner-expiry tests/owner.py Owner.test_relay_settles_its_claim_after_lease_expires 1
}

run_startup_units() {
    local log="$EVIDENCE/startup-unit.log" status=0 checked=0 verdict=pass selector
    WFC_BUILD_ID="$PRODUCT_SHA" "$python" -Bu "$ci_dir/reap.py" timeout --kill-after=5s 300s \
        cargo test --release --locked --target "$TARGET" --bin wfc startup -- --test-threads=1 \
        2>&1 | tee "$log" || status=$?
    for selector in \
        workflow::backend::tests::startup_terminal_notice_requires_the_selected_launch_and_outcome \
        workflow::messages::recovery::tests::startup_recovery_advances_past_history_in_small_slices \
        workflow::messages::startup_recovery_tests::startup_recovery_rejects_replaced_assignment_and_preserves_existing_question \
        workflow::messages::relay::tests::startup_relay_rechecks_legacy_owner_proof_under_transport_lock \
        workflow::messages::tests::startup_receipt_with_report_does_not_count_as_done \
        workflow::startup::tests::startup_publication_uses_the_current_attempt_and_one_durable_key \
        workflow::startup::tests::unreaped_exit_keeps_creation_identity_across_a_slow_started_receipt \
        workflow::worker_owner::tests::startup_observes_natural_exit_during_controlled_publication
    do
        if grep -Fxq "test $selector ... ok" "$log"; then
            checked=$((checked + 1))
        fi
    done
    if (( status != 0 || checked != 8 )) ||
        ! grep -Fxq 'Own descendants reaped: ECHILD, extra cleanup signals: 0' "$log"; then
        verdict=fail
        if [[ "$first_failure" == none ]]; then
            first_failure=startup-unit
        fi
    fi
    printf 'startup-unit exit=%s expected=8 confirmed=%s verdict=%s\n' "$status" "$checked" "$verdict" |
        tee -a "$EVIDENCE/results.log"
}

run_startup_opencode_suites() {
    run_suite startup-interruption tests/startup_failure.py StartupFailure.test_startup_notice_recovers_after_dispatch_interruption 1
    run_suite startup-ledger-write tests/startup_failure.py StartupFailure.test_startup_notice_recovers_after_ledger_write_failure 1
    run_suite startup-confirmation tests/startup_failure.py StartupFailure.test_tmux_exit_follows_the_create_confirmation 1
    run_suite startup-managed tests/startup_failure.py StartupFailure.test_managed_startup_recovery_rechecks_owner 1
    run_suite startup-generations tests/startup_failure.py StartupFailure.test_startup_recovery_ignores_conflicting_reservations_and_keeps_neighbors 1
    run_suite startup-resume-polls tests/startup_failure.py StartupFailure.test_short_wait_restarts_resume_after_processed_prefix 1
    run_startup_units
    sha256sum --check --strict "$EVIDENCE/binary.sha256"
    run_suite receipt-legacy tests/receipts.py Receipts.test_mixed_legacy_inbox 1
    run_suite receipt-scoped tests/receipts.py Receipts.test_scoped_snapshot_survives_confirmation 1
    run_suite codex-envelope tests/relay.py Relay.test_question_envelope_matches_python_json_separators 1
    run_suite codex-budget tests/relay.py Relay.test_escaped_single_over_limit_leaves_legal_messages_available 1
    run_suite codex-deadline tests/relay.py Relay.test_deadline_bounds_locked_inbox_and_slow_queue 1
    run_suite codex-legacy-owner tests/relay.py Relay.test_conflicting_complete_generations_preserve_owner_and_identity 1
    run_suite opencode-order tests/opencode_relay.py OpenCodeRelay.test_204_detached_preserves_batch_order_scope_and_session 1
    run_suite codex-scope-refresh tests/relay.py Relay.test_running_relay_refreshes_repaired_and_legacy_owner_records 1
    run_suite reclaim-mixed tools/check_tmux.py Backend.test_public_reclaim_preview_mixed_results_and_history 1
    run_suite reclaim-preflight tools/check_tmux.py Backend.test_public_reclaim_checks_history_before_closing 1
    run_suite reclaim-owner tools/check_tmux.py Backend.test_public_reclaim_stops_on_scope_authority_loss 1
    run_suite opencode-refused tests/opencode_relay.py OpenCodeRelay.test_connection_refused_is_unsent_and_can_retry_original_id 1
    run_suite opencode-uncertain tests/opencode_relay.py OpenCodeRelay.test_errors_redirects_timeouts_and_eof_never_replay 1
    run_suite managed-opencode "$ci_dir/managed_owner.py" ManagedOpenCode.test_current_owner_delivers_and_old_owner_cannot_claim 1
    run_suite startup-opencode "$ci_dir/startup_recovery.py" StartupOpenCode.test_recovers_interrupted_notice_through_scoped_http 1
    sha256sum --check --strict "$EVIDENCE/binary.sha256"
}

run_relay_retry_suites() {
    run_suite opencode-refused tests/opencode_relay.py OpenCodeRelay.test_connection_refused_is_unsent_and_can_retry_original_id 1
    run_suite opencode-uncertain tests/opencode_relay.py OpenCodeRelay.test_errors_redirects_timeouts_and_eof_never_replay 1
    run_suite opencode-order tests/opencode_relay.py OpenCodeRelay.test_204_detached_preserves_batch_order_scope_and_session 1
    run_suite codex-retry tests/relay.py Relay.test_queue_outcomes_and_restart 1
    run_suite startup-resume-polls tests/startup_failure.py StartupFailure.test_short_wait_restarts_resume_after_processed_prefix 1
    run_suite codex-scope-refresh tests/relay.py Relay.test_running_relay_refreshes_repaired_and_legacy_owner_records 1
    run_suite startup-opencode "$ci_dir/startup_recovery.py" StartupOpenCode.test_recovers_interrupted_notice_through_scoped_http 1
    sha256sum --check --strict "$EVIDENCE/binary.sha256"
}

run_resume_owner_suites() {
    local copy overlay
    copy=$(mktemp -d "$RUNNER_TEMP/wfc-approved-resume.XXXXXXXX")
    overlay=$(realpath "$ci_dir/../resume/callback-fixture.patch")
    mkdir "$copy/tools"
    cp -- tools/check_tmux.py "$copy/tools/check_tmux.py"
    {
        printf '%s  %s\n' 3352bcc9d9784cb99adb975f0c04d7a5a21bd6d9570cf8f9f3e8f706771af6d8 "$overlay" | sha256sum --check --strict
        git -C "$copy" apply --no-index --unidiff-zero "$overlay"
        printf '%s  %s\n' 4c4e46cd7afdea99641454eb316400ba23bbddf464f7c3dcb57af7312a6f1116 "$copy/tools/check_tmux.py" | sha256sum --check --strict
        sha256sum "$overlay" "$copy/tools/check_tmux.py"
    } | tee "$EVIDENCE/resume-fixture.log"

    run_relay_retry_suites
    run_suite startup-blocked-progress tests/startup_failure.py StartupFailure.test_long_wait_advances_when_checkpoint_stays_blocked 1
    run_suite startup-confirmation tests/startup_failure.py StartupFailure.test_tmux_exit_follows_the_create_confirmation 1
    run_suite startup-managed tests/startup_failure.py StartupFailure.test_managed_startup_recovery_rechecks_owner 1
    run_suite startup-generations tests/startup_failure.py StartupFailure.test_startup_recovery_ignores_conflicting_reservations_and_keeps_neighbors 1
    run_suite startup-readonly-close tests/startup_failure.py StartupFailure.test_script_preflight_close_replays_from_read_only_history 1
    run_suite resume-pending "$copy/tools/check_tmux.py" Backend.test_public_resume_fences_pending_and_lost_launch_responses 1
    run_suite resume-spawn "$copy/tools/check_tmux.py" Backend.test_public_resume_records_definite_spawn_failure 1
    run_suite resume-owner "$copy/tools/check_tmux.py" Backend.test_public_resume_preserves_conversation_rounds_and_callback_fences 1
    run_suite resume-reclaim "$copy/tools/check_tmux.py" Backend.test_public_resume_after_reclaim_preserves_history 1
    run_suite reclaim-owner tools/check_tmux.py Backend.test_public_reclaim_rechecks_selected_execution_under_scope_lock 1
    rm -r -- "$copy"
    run_startup_units
    sha256sum --check --strict "$EVIDENCE/binary.sha256"
}

case "${1:-all}" in
  all)
    run_opencode_suites
    run_suite codex tests/relay.py Relay 18
    run_owner_suites
    ;;
  owner-only)
    run_owner_suites
    ;;
  target-validation)
    run_opencode_suites
    run_suite codex-envelope tests/relay.py Relay.test_question_envelope_matches_python_json_separators 1
    ;;
  startup-opencode)
    run_startup_opencode_suites
    ;;
  relay-retry)
    run_relay_retry_suites
    ;;
  resume-owner)
    run_resume_owner_suites
    ;;
  *)
    printf 'expected all, owner-only, target-validation, startup-opencode, relay-retry or resume-owner\n' >&2
    exit 2
    ;;
esac
printf 'firstFailedSuite=%s\n' "$first_failure" | tee -a "$EVIDENCE/results.log"
test "$first_failure" = none
