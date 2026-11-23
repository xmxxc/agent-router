#!/usr/bin/env bash
set -euo pipefail

helpers=$(cd "$(dirname "$0")" && pwd)
cd "$GITHUB_WORKSPACE/product"
wfc="$PWD/target/$TRIPLE/release/wfc"
reaper="$GITHUB_WORKSPACE/reaper-source/tools/ci/tmux-r11/reap.py"
herdr="$RUNNER_TEMP/herdr-linux-x86_64"
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

if [[ "$TMUX_READY" == success ]]; then
    run_suite tmux python3 -Bu tools/check_tmux.py "$wfc" -v \
        Backend.test_public_reclaim_preview_mixed_results_and_history \
        Backend.test_public_reclaim_stops_on_scope_authority_loss \
        Backend.test_public_reclaim_rechecks_selected_execution_under_scope_lock
else
    blocked tmux "tmux setup"
fi

if [[ "$HERDR_READY" == success ]]; then
    run_suite herdr python3 -Bu tests/herdr_public.py "$herdr" "$wfc" -v \
        PublicLifecycle.test_reclaim_uses_verified_herdr_close_and_keeps_sibling
else
    blocked herdr "Herdr setup"
fi

run_suite legacy-public python3 -Bu "$helpers/legacy_matrix.py" tests/legacy_public.py "$wfc"
run_suite owner python3 -Bu -c '
import sys
import unittest
sys.path.insert(0, "tests")
from owner import Owner
suite = unittest.TestSuite([Owner("test_takeover_fences_wait_and_ack_but_worker_keeps_its_callback")])
sys.exit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
' "$wfc"
run_suite receipts python3 -Bu tests/receipts.py "$wfc" -v Receipts.test_mixed_legacy_inbox
run_suite relay python3 -Bu tests/relay.py "$wfc" -v \
    Relay.test_two_relays_and_wait_claim_once_and_resolution_wins
run_suite workspace cargo +1.96.1 test --workspace --release --locked --target "$TRIPLE" \
    -- --test-threads=1
sha256sum --check --strict "$EVIDENCE/elf.sha256" | tee -a "$EVIDENCE/test-binary.log"
exit "$failed"
