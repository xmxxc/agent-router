#!/usr/bin/env bash
# Run from the fixed product checkout on the Linux runner.
set -euo pipefail

group=${1:?test group required}
case "$group" in
    tmux-handoff)
        fixture=tools/check_tmux.py
        selectors=(
            Backend.test_public_handoff_switches_host_once_in_the_same_round
            Backend.test_public_handoff_never_launches_while_the_original_writer_survives
            Backend.test_public_handoff_preserves_startup_notice_and_original_close
        )
        ;;
    cross-handoff)
        fixture=tests/herdr_public.py
        selectors=(
            PublicLifecycle.test_tmux_to_herdr_handoff_preserves_original_receipts_and_launches_once
            PublicLifecycle.test_tmux_to_herdr_handoff_does_not_retry_after_layout_reply_loss
            PublicLifecycle.test_tmux_to_herdr_handoff_waits_for_source_writer
            PublicLifecycle.test_tmux_to_herdr_handoff_refuses_a_replaced_service
            PublicLifecycle.test_tmux_to_herdr_handoff_reports_unconfirmed_zero_exit_once
        )
        ;;
    reverse-handoff)
        fixture=tests/herdr_public.py
        selectors=(
            PublicLifecycle.test_completed_herdr_to_tmux_handoff_starts_next_round_once
            PublicLifecycle.test_herdr_to_tmux_handoff_waits_for_confirmed_original_stop
            PublicLifecycle.test_tmux_herdr_tmux_handoff_restores_public_session
            PublicLifecycle.test_herdr_to_tmux_handoff_legacy_receipt_stays_pending
            PublicLifecycle.test_herdr_to_tmux_handoff_surviving_descendant_stays_pending
            PublicLifecycle.test_herdr_to_tmux_handoff_lost_close_keeps_original_audit
            PublicLifecycle.test_herdr_to_tmux_handoff_mismatched_stop_stays_pending
        )
        ;;
    offline-herdr)
        fixture=../ci/tools/ci/resume/check_offline_resume.py
        selectors=(
            OfflineResume.test_public_resume_reuses_saved_chat_and_task_once
            OfflineResume.test_public_resume_without_confirmed_stop_does_not_launch
            OfflineResume.test_public_resume_without_saved_chat_does_not_launch
            OfflineResume.test_public_resume_keeps_newly_observed_original_writer
            OfflineResume.test_public_resume_reports_unconfirmed_zero_exit_once
        )
        ;;
    legacy-write-lock)
        fixture=tests/legacy_write_lock_fence.py
        ;;
    *)
        printf 'Unknown test group %s\n' "$group" >&2
        exit 2
        ;;
esac
if [[ "$fixture" == tests/herdr_public.py || "$group" == offline-herdr ]]; then
    test -x "${HERDR_BINARY:?fixed Herdr ELF required}"
    sha256sum --check --strict "$EVIDENCE/herdr-elf.sha256"
fi

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
if [[ "$group" == legacy-write-lock ]]; then
    command=( "$python" -Bu "$fixture" "$binary" )
else
    command=( "$python" -Bu "$runner" "$scratch" "$fixture" "$binary"
              "$EVIDENCE/$group-observations.log" "${selectors[@]}" )
fi
status=0
env -i PATH="$PATH" LC_ALL=C.UTF-8 TERM=xterm-256color PYTHONUTF8=1 \
    HERDR_BINARY="${HERDR_BINARY:-}" \
    HOME="$scratch/home" CODEX_HOME="$scratch/codex" WFC_STATE="$scratch/state" \
    TMPDIR="$scratch/tmp" WFC_TMUX_SOCKET="$scratch/tmux.sock" \
    "$python" -Bu "$reaper" "$scratch" "$EVIDENCE/$group-processes.log" \
    timeout --kill-after=10s 4m "${command[@]}" || status=$?
sha256sum --check --strict "$EVIDENCE/elf.sha256"
if [[ "$fixture" == tests/herdr_public.py || "$group" == offline-herdr ]]; then
    sha256sum --check --strict "$EVIDENCE/herdr-elf.sha256"
fi
exit "$status"
