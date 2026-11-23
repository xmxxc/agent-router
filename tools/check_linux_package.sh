#!/usr/bin/env bash
set -euo pipefail

bin=$(realpath -- "${1:?usage: check_linux_package.sh WFC}")
work=$(mktemp -d)
export WFC_STATE="$work/state"
dispatch=''

cleanup() {
    if [[ -n "$dispatch" ]]; then
        "$bin" close "$dispatch" >/dev/null || true
    fi
    if [[ -s "$work/worker.log" ]]; then
        cat "$work/worker.log"
    fi
    rm -rf -- "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

printf '%s\n' \
    'Smoke coverage: controlled Bash worker on Linux x86-64 with local procfs.' \
    'Worker tools: Bash, cat, and sleep. No model host or terminal backend is started.' \
    'Issue #21 covers real model hosts and final download acceptance on the same frozen candidate.'

task="$work/task.md"
report="$work/report.md"
printf 'Linux package smoke report\n' > "$task"
"$bin" dispatch --task "$task" --cwd "$work" --report "$report" \
    --role smoke --scope linux-package-ci -- /bin/bash -euc '
        exec >worker.log 2>&1
        printf "%s\n" "$$" > worker.pid
        cat "$WFC_TASK" > "$WFC_REPORT"
        "$1" done --report "$WFC_REPORT" --attempt "$WFC_ATTEMPT"
        exec sleep 60
    ' worker "$bin" | tee "$work/dispatch.json"
dispatch=$(jq -er '.dispatch | strings | select(startswith("d-"))' "$work/dispatch.json")
attempt=$(jq -er '.attempt | strings | select(startswith("a-"))' "$work/dispatch.json")

"$bin" wait --dispatch "$dispatch" --scope linux-package-ci --timeout 15 | tee "$work/wait.json"
message=$(jq -er --arg dispatch "$dispatch" --arg attempt "$attempt" --arg report "$report" '
    select(.messages | length == 1) | .messages[0]
    | select(.dispatch == $dispatch and .attempt == $attempt and .report == $report
        and .scope == "linux-package-ci" and .round == 1 and .type == "done"
        and .state == "received" and .statusVersion == 2)
    | .messageId | strings | select(length > 0)
' "$work/wait.json")
cmp "$task" "$report"

"$bin" ack "$message" | tee "$work/ack.json"
jq -e --arg message "$message" '
    .messageId == $message and .state == "processed" and .statusVersion == 3 and .duplicate == false
' "$work/ack.json" >/dev/null

"$bin" close "$dispatch" | tee "$work/close.json"
jq -e --arg dispatch "$dispatch" --arg attempt "$attempt" '
    .dispatch == $dispatch and .attempt == $attempt and .round == 1
    and .operation == "close" and .result == "completed"
' "$work/close.json" >/dev/null

"$bin" status --dispatch "$dispatch" | tee "$work/status.json"
jq -e --arg message "$message" '
    .dispatches | length == 1 and (.[0] | .worker.state == "cancelled"
        and (.messages | length == 1 and (.[0] | .messageId == $message and .state == "processed")))
' "$work/status.json" >/dev/null
read -r pid < "$work/worker.pid"
[[ "$pid" =~ ^[0-9]+$ && ! -e "/proc/$pid" ]]
dispatch=''
printf 'Unpacked CLI passed dispatch, wait, ack and close. Worker PID %s was reaped.\n' "$pid"
