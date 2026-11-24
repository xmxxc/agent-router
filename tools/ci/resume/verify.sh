#!/usr/bin/env bash
# Run from the fixed product checkout on the Linux runner.
set -euo pipefail

group=${1:?test group required}
case "$group" in
    pane-completion)
        fixture=../ci/tools/ci/resume/pane_completion.py
        selector=PaneCompletion.test_completed_herdr_to_tmux_handoff_starts_next_round_once
        ;;
    *)
        printf 'Unknown test group %s\n' "$group" >&2
        exit 2
        ;;
esac
test -x "${HERDR_BINARY:?fixed Herdr ELF required}"
sha256sum --check --strict "$EVIDENCE/herdr-elf.sha256"

scratch=$(mktemp -d /tmp/wfc-resume-XXXXXXXX)
cleanup() {
    local status=$? removed=no
    trap - EXIT
    if rm -rf -- "$scratch" && [[ ! -e "$scratch" && ! -L "$scratch" ]]; then
        removed=yes
    else
        status=1
    fi
    printf '%s exit=%s group_root_removed=%s\n' "$group" "$status" "$removed" |
        tee -a "$EVIDENCE/results.log"
    exit "$status"
}
trap cleanup EXIT

test "$(id -u)" -ne 0
test "$(git rev-parse HEAD)" = "$PRODUCT_SHA"
test "$(git rev-parse 'HEAD^{tree}')" = "$PRODUCT_TREE"
binary=$(realpath "target/$TARGET/release/wfc")
python=$(command -v python3)
reaper=$(realpath ../ci/tools/ci/resume/reap.py)
runner=$(realpath ../ci/tools/ci/resume/run.py)
version=$("$binary" --version)
printf 'fixture=%s binary=%s\n%s\n' "$fixture" "$binary" "$version"
[[ "$version" == *"(Rust, $TARGET, release, build $PRODUCT_SHA)" ]]
sha256sum --check --strict "$EVIDENCE/elf.sha256"
sha256sum "$binary"

mkdir -- "$scratch/home" "$scratch/codex" "$scratch/state" "$scratch/tmp"
printf 'HOME=%s CODEX_HOME=%s WFC_STATE=%s TMPDIR=%s WFC_TMUX_SOCKET=%s\n' \
    "$scratch/home" "$scratch/codex" "$scratch/state" "$scratch/tmp" "$scratch/tmux.sock"
status=0
env -i PATH="$PATH" LC_ALL=C.UTF-8 TERM=xterm-256color PYTHONUTF8=1 \
    HERDR_BINARY="${HERDR_BINARY:-}" \
    HOME="$scratch/home" CODEX_HOME="$scratch/codex" WFC_STATE="$scratch/state" \
    TMPDIR="$scratch/tmp" WFC_TMUX_SOCKET="$scratch/tmux.sock" \
    "$python" -Bu "$reaper" "$scratch" "$EVIDENCE/$group-processes.log" \
    timeout --kill-after=10s 4m "$python" -Bu "$runner" "$scratch" "$fixture" "$binary" \
    "$EVIDENCE/$group-observations.log" "$selector" || status=$?
sha256sum --check --strict "$EVIDENCE/elf.sha256"
sha256sum --check --strict "$EVIDENCE/herdr-elf.sha256"
exit "$status"
