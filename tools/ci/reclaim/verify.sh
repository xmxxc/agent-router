#!/usr/bin/env bash
set -euo pipefail

helpers=$(cd "$(dirname "$0")" && pwd)
cd "$GITHUB_WORKSPACE/product"
wfc="$PWD/target/$TRIPLE/release/wfc"
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
        Backend.test_public_reclaim_checks_history_before_closing \
        Backend.test_public_reclaim_requires_sticky_ownership
    if ! grep -Eq '^Ran 2 tests in ' "$EVIDENCE/tmux.stderr.log" ||
        ! grep -Fxq 'OK' "$EVIDENCE/tmux.stderr.log"; then
        printf '%s\n' 'Both selected tmux fixtures must pass without skips.' | tee -a "$EVIDENCE/selector.log" >&2
        failed=1
    fi
else
    blocked tmux "tmux setup"
fi

run_suite legacy-public python3 -Bu "$helpers/legacy_matrix.py" tests/legacy_public.py "$wfc"
sha256sum --check --strict "$EVIDENCE/elf.sha256" | tee -a "$EVIDENCE/test-binary.log"
exit "$failed"
