#!/usr/bin/env bash
# Run from the fixed product checkout on the Linux runner.
set -euo pipefail

group=${1:?test group required}
case "$group" in
    public-interrupt)
        fixture=tools/check_tmux.py
        selectors=(
            Backend.test_public_interrupt_fences_before_tmux_key
            Backend.test_public_interrupt_lost_confirmation_keeps_fence_and_refuses_foreign_writer
            Backend.test_public_interrupt_holds_both_locks_after_controller_exit
            Backend.test_public_interrupt_leaves_pending_when_owner_expires
            Backend.test_public_interrupt_backend_spawn_failure_is_definitely_not_sent
            Backend.test_read_only_unowned_close_replays_confirmed_archive
            Backend.test_public_interrupt_preflight_zero_send_allows_takeover
            Backend.test_public_interrupt_blocks_owner_change_after_backend_dies_with_queued_key
        )
        ;;
    public-herdr-close)
        fixture=tests/herdr_public.py
        test -x "${HERDR_BINARY:?fixed Herdr binary required}"
        selectors=(PublicLifecycle.test_close_executed_without_reply)
        ;;
    codex-home-paths)
        fixture=tools/check_tmux.py
        selectors=(
            Backend.test_public_resume_dot_codex_home_created_after_dispatch
            Backend.test_public_resume_relative_codex_home_created_after_dispatch
        )
        ;;
    public-resume)
        fixture=tools/check_tmux.py
        selectors=(
            Backend.test_public_resume_preserves_conversation_rounds_and_callback_fences
            Backend.test_public_resume_relative_codex_home_created_after_dispatch
            Backend.test_public_resume_records_definite_spawn_failure
            Backend.test_public_resume_rechecks_completion_before_reserving
            Backend.test_public_resume_fences_pending_and_lost_launch_responses
        )
        ;;
    tmux-resume)
        fixture=tools/check_tmux.py
        selectors=(
            Backend.test_resume_host_inputs
            Backend.test_resume_keeps_started_receipt_when_brief_could_not_be_sent
        )
        ;;
    tmux-owner)
        fixture=tools/check_tmux.py
        selectors=(Backend.test_managed_dispatch_callback_survives_takeover)
        ;;
    scope)
        fixture=tests/owner.py
        selectors=(
            Owner.test_takeover_fences_wait_and_ack_but_worker_keeps_its_callback
            Owner.test_question_requires_current_owner_or_original_worker_callback
        )
        ;;
    receipts)
        fixture=tests/receipts.py
        selectors=(Receipts.test_concurrent_ack Receipts.test_scoped_snapshot_survives_confirmation)
        ;;
    *)
        printf 'Unknown test group %s\n' "$group" >&2
        exit 2
        ;;
esac

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
reaper=$(realpath ../tmux-reference/tools/ci/tmux-r9/reap.py)
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
    "$python" -Bu "$reaper" timeout --kill-after=10s 4m "$python" -Bu "$runner" \
    "$scratch" "$fixture" "$binary" "$EVIDENCE/$group-observations.log" "${selectors[@]}" || status=$?
sha256sum --check --strict "$EVIDENCE/elf.sha256"
exit "$status"
