#!/usr/bin/env bash
set -euo pipefail

helpers=$(cd "$(dirname "$0")" && pwd)
cd "$GITHUB_WORKSPACE/product"
wfc="$PWD/target/$TRIPLE/release/wfc"
herdr="$RUNNER_TEMP/herdr-linux-x86_64"
reaper="$GITHUB_WORKSPACE/reaper-source/tools/ci/tmux-r11/reap.py"
test -f "$EVIDENCE/elf.sha256"
sha256sum --check --strict "$EVIDENCE/elf.sha256" | tee "$EVIDENCE/test-binary.log"

failed=0
printf 'suite\texit\n' > "$EVIDENCE/test-exits.tsv"

run_suite() {
    local name=$1 rc=0
    shift
    printf '\nRunning %s\n' "$name"
    python3 -B "$reaper" "$@" > "$EVIDENCE/$name.stdout.log" \
        2> "$EVIDENCE/$name.stderr.log" || rc=$?
    printf '%s\t%s\n' "$name" "$rc" | tee -a "$EVIDENCE/test-exits.tsv"
    cat "$EVIDENCE/$name.stdout.log"
    cat "$EVIDENCE/$name.stderr.log" >&2
    if ((rc != 0)); then
        failed=1
    fi
}

blocked() {
    printf '%s\tblocked (%s)\n' "$1" "$2" | tee -a "$EVIDENCE/test-exits.tsv"
    failed=1
}

policy=workflow::messages::tests::sticky_replacement_requires_confirmed_owner
run_suite pure-permissions cargo +1.96.1 test --release --locked --target "$TRIPLE" --bin wfc \
    "$policy" -- --exact
if ! grep -Fxq "test $policy ... ok" "$EVIDENCE/pure-permissions.stdout.log"; then
    printf '%s\n' 'The selected ownership policy test did not pass.' | tee -a "$EVIDENCE/selector.log" >&2
    failed=1
fi

if [[ "$TMUX_READY" == success ]]; then
    run_suite tmux env WFC_TEST_STICKY_OWNERSHIP=1 python3 -Bu tools/check_tmux.py "$wfc" -v \
        Backend.test_public_reclaim_preview_mixed_results_and_history \
        Backend.test_public_reclaim_checks_history_before_closing \
        Backend.test_public_reclaim_requires_sticky_ownership \
        Backend.test_public_reclaim_stops_on_scope_authority_loss \
        Backend.test_public_reclaim_rechecks_selected_execution_under_scope_lock
    if ! grep -Eq '^Ran 5 tests in ' "$EVIDENCE/tmux.stderr.log" ||
        ! grep -Fxq 'OK' "$EVIDENCE/tmux.stderr.log"; then
        printf '%s\n' 'All five selected tmux fixtures must pass without skips.' | tee -a "$EVIDENCE/selector.log" >&2
        failed=1
    fi
else
    blocked tmux "tmux setup"
fi

if [[ "$HERDR_READY" == success ]]; then
    run_suite herdr python3 -Bu tests/herdr_public.py "$herdr" "$wfc" -v \
        PublicLifecycle.test_reclaim_uses_verified_herdr_close_and_keeps_sibling \
        PublicLifecycle.test_question_replays_after_herdr_close_intent
    if ! grep -Eq '^Ran 2 tests in ' "$EVIDENCE/herdr.stderr.log" ||
        ! grep -Fxq 'OK' "$EVIDENCE/herdr.stderr.log"; then
        printf '%s\n' 'Both selected Herdr fixtures must pass without skips.' | tee -a "$EVIDENCE/selector.log" >&2
        failed=1
    fi
else
    blocked herdr "Herdr setup"
fi

for test in controlled_close_replays_from_a_read_only_archive \
    controlled_pending_close_rejects_new_question_and_replays_original; do
    run_suite "$test" cargo +1.96.1 test --release --locked --target "$TRIPLE" --test dispatch \
        "$test" -- --exact
    if ! grep -Fxq "test $test ... ok" "$EVIDENCE/$test.stdout.log"; then
        printf 'The selected close test did not pass: %s\n' "$test" | tee -a "$EVIDENCE/selector.log" >&2
        failed=1
    fi
done

run_suite owner python3 -Bu -c '
import sys
import unittest
sys.path.insert(0, "tests")
from owner import Owner
suite = unittest.TestSuite([Owner("test_question_requires_current_owner_or_original_worker_callback")])
sys.exit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
' "$wfc"
run_suite receipts python3 -Bu tests/receipts.py "$wfc" -v Receipts.test_mixed_legacy_inbox
run_suite codex python3 -Bu tests/relay.py "$wfc" -v Relay.test_question_envelope_matches_python_json_separators
run_suite opencode python3 -Bu tests/opencode_relay.py "$wfc" -v \
    OpenCodeRelay.test_204_detached_preserves_batch_order_scope_and_session \
    OpenCodeRelay.test_connection_refused_is_unsent_and_can_retry_original_id
run_suite legacy-public python3 -Bu "$helpers/legacy_matrix.py" tests/legacy_public.py "$wfc"
sha256sum --check --strict "$EVIDENCE/elf.sha256" | tee -a "$EVIDENCE/test-binary.log"
exit "$failed"
