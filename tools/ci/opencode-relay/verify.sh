#!/bin/bash
# Run the OpenCode argument validation method from the pinned product checkout.
set -euo pipefail
ci_dir=$(cd "$(dirname "$0")" && pwd)
binary=$(realpath "target/$TARGET/release/wfc")
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
    env -i PATH="$PATH" HOME="$work/home" TMPDIR="$work/tmp" LANG=C.UTF-8 TERM=xterm         PYTHONUTF8=1 PYTHONDONTWRITEBYTECODE=1         WFC_STATE="$work/state" WFC_TMUX_SOCKET="$work/tmux.sock"         "$python" -Bu "$ci_dir/reap.py" timeout --kill-after=5s 180s         "$python" -Bu "$script" "$binary" -v "$selector" 2>&1 | tee -a "$log" || status=$?
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

run_suite opencode tests/opencode_relay.py OpenCodeRelay.test_invalid_target_and_worker_callback_do_not_claim 1
printf 'firstFailedSuite=%s\n' "$first_failure" | tee -a "$EVIDENCE/results.log"
test "$first_failure" = none
