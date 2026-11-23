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

case "${1:-all}" in
  all)
    run_suite opencode tests/opencode_relay.py OpenCodeRelay 6
    run_suite managed-opencode "$ci_dir/managed_owner.py" ManagedOpenCode.test_current_owner_delivers_and_old_owner_cannot_claim 1
    run_suite codex tests/relay.py Relay 17
    ;;
  owner-only)
    ;;
  *)
    printf 'expected all or owner-only\n' >&2
    exit 2
    ;;
esac
run_suite owner-takeover tests/owner.py Owner.test_takeover_waits_for_scoped_delivery_and_unregisters_detached_relay 1
run_suite owner-expiry tests/owner.py Owner.test_relay_settles_its_claim_after_lease_expires 1
printf 'firstFailedSuite=%s\n' "$first_failure" | tee -a "$EVIDENCE/results.log"
test "$first_failure" = none
