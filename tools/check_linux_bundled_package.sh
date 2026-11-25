#!/usr/bin/env bash
set -euo pipefail
umask 077

bin=$(realpath -- "${1:?usage: check_linux_bundled_package.sh WFC}")
provided=${WFC_BUNDLED_CGROUP_ROOT:-}
if [[ -z "$provided" ]] || ! delegation=$(realpath -e -- "$provided") ||
    [[ ! -d "$delegation" || ! -w "$delegation" || ! -x "$delegation" ||
       ! -f "$delegation/cgroup.kill" || ! -w "$delegation/cgroup.procs" ]] ||
    [[ "$(stat -f -c %T -- "$delegation")" != cgroup2fs ]]; then
    printf 'BLOCKED: this runtime lacks a platform-provided writable cgroup v2 delegation with cgroup.kill.\n' >&2
    exit 78
fi
printf 'Delegation visible to uid %s: %s\n' "$(id -u)" "$delegation"
cat /proc/self/cgroup

work=$(mktemp -d /tmp/wfc-bundled-smoke.XXXXXXXX)
mkdir "$work/home"
scope=linux-package-bundled-smoke
dispatch=''
close_started=false

wfc() {
    env -i PATH=/usr/bin:/bin LC_ALL=C.UTF-8 TERM=xterm-256color HOME="$work/home" \
        WFC_STATE="$work/state" WFC_BUNDLED_CGROUP_ROOT="$delegation" "$bin" "$@"
}

cleanup() {
    local rc=$?
    if [[ -n "$dispatch" && "$close_started" == false ]]; then
        if wfc close "$dispatch" > "$work/cleanup.json"; then
            jq '{operation,dispatch,attempt,result,cleanup}' "$work/cleanup.json" || true
        fi
    fi
    if ((rc == 0)); then
        rm -r -- "$work"
    else
        printf 'Private fixture evidence retained at %s\n' "$work" >&2
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

proc_start() {
    local stat_line
    local -a fields
    IFS= read -r stat_line < "/proc/$1/stat" || return
    read -r -a fields <<< "${stat_line##*) }"
    printf '%s\n' "${fields[19]}"
}

printf 'Linux default bundled package receipt\n' > "$work/task.md"
cat > "$work/worker.sh" <<'WORKER'
set -euo pipefail
sha256sum "$WFC_BIN" > "$WFC_REPORT.elf"
cat "$WFC_TASK" > "$WFC_REPORT"
timeout 10 "$WFC_BIN" done --report "$WFC_REPORT" --attempt "$WFC_ATTEMPT" > "$WFC_REPORT.pending"
mv -- "$WFC_REPORT.pending" "$WFC_REPORT.done"
exec /bin/sleep 60
WORKER
jq -nc --arg worker "$work/worker.sh" '{hosts:{codex:{command:["/bin/bash",$worker]}}}' > "$work/launch.json"
dispatch_exit=0
wfc dispatch --host codex --role smoke --scope "$scope" --cwd "$work" --task "$work/task.md" \
    --report "$work/report.md" --launch-config "$work/launch.json" > "$work/dispatch.json" || dispatch_exit=$?
dispatch=$(jq -er '.dispatch | strings | select(test("^d-[0-9a-f]+$"))' "$work/dispatch.json") || dispatch=''
if [[ -z "$dispatch" ]] && wfc status > "$work/dispatch-recovery.json"; then
    dispatch=$(jq -er --arg scope "$scope" '
        [.dispatches[] | select(.scope == $scope and .backend == "bundled" and .backendAlive == true
            and .backendHandle.unit == .dispatch and .backendHandle.attempt == .attempt)]
        | select(length == 1) | .[0].dispatch | strings | select(test("^d-[0-9a-f]+$"))' \
        "$work/dispatch-recovery.json") || dispatch=''
fi
((dispatch_exit == 0)) || exit "$dispatch_exit"
[[ -n "$dispatch" ]]
jq -e '.backend == "bundled" and .create.result == "accepted"
    and .create.ticket.unit == .dispatch and .create.ticket.attempt == .attempt' "$work/dispatch.json" >/dev/null
attempt=$(jq -er '.attempt | strings | select(startswith("a-"))' "$work/dispatch.json")
jq '{dispatch,round,attempt,backend,identity:.create.identity}' "$work/dispatch.json"
create_ticket=$(jq -cS '.create.ticket' "$work/dispatch.json" | sha256sum | cut -d ' ' -f 1)
printf 'dispatch_ticket_sha256=%s\n' "$create_ticket"

boot=$(cat /proc/sys/kernel/random/boot_id)
group=$(jq -er '.create.identity.containment.path | strings' "$work/dispatch.json")
[[ "$(dirname -- "$group")" == "$delegation" && "$(basename -- "$group")" == wfc-* ]]
identity=$(jq -er '.create.identity.containment | select(.kind == "cgroup-v2") | "\(.device):\(.inode)"' "$work/dispatch.json")
[[ "$(stat -c '%d:%i' -- "$group")" == "$identity" ]]
jq -er --arg boot "$boot" '.create.identity | [.guardian,.service,.kernel,.worker][]
    | select(.boot == $boot and .pid > 0 and .start_ticks > 0) | "\(.pid) \(.start_ticks)"' \
    "$work/dispatch.json" > "$work/processes"
mapfile -t processes < "$work/processes"
[[ "${#processes[@]}" == 4 ]]
for process in "${processes[@]}"; do
    read -r pid ticks <<< "$process"
    [[ "$(proc_start "$pid")" == "$ticks" ]]
done

wfc wait --scope "$scope" --dispatch "$dispatch" --timeout 15 | tee "$work/wait.json"
for _ in {1..30}; do
    [[ ! -f "$work/report.md.done" ]] || break
    sleep 0.1
done
cmp "$work/task.md" "$work/report.md"
read -r worker_digest _ < "$work/report.md.elf"
printf '%s  %s\n' "$worker_digest" "$bin" | sha256sum --check --strict
message=$(jq -er --arg dispatch "$dispatch" --arg attempt "$attempt" --arg scope "$scope" \
    --arg report "$work/report.md" --slurpfile done "$work/report.md.done" '
    select(.messages | length == 1) | .messages[0]
    | select(.dispatch == $dispatch and .attempt == $attempt and .scope == $scope
        and .report == $report and .round == 1 and .type == "done" and .state == "received"
        and .id == $done[0].message and .messageId == $done[0].messageId)
    | .messageId | strings | select(length > 0)' "$work/wait.json")
wfc ack "$message" | tee "$work/ack.json" | jq -e --arg id "$message" --slurpfile done "$work/report.md.done" \
    '.messageId == $id and .message == $done[0].message and .state == "processed"
        and .statusVersion == 3 and .duplicate == false' >/dev/null
cat "$work/ack.json"

close_started=true
wfc close "$dispatch" > "$work/close.json"
jq '{dispatch,round,attempt,operation,result,cleanup,proof:{state:.proof.state,proves:.proof.proves}}' "$work/close.json"
jq -e --arg dispatch "$dispatch" --arg attempt "$attempt" --slurpfile creation "$work/dispatch.json" '
    .dispatch == $dispatch and .attempt == $attempt and .round == 1 and .operation == "close"
    and .result == "completed" and .cleanup == "confirmed"
    and .ticket == $creation[0].create.ticket
    and .proof.state == "closed" and .proof.proves == "execution-empty"
    and .proof.identity == $creation[0].create.identity' "$work/close.json" >/dev/null
close_ticket=$(jq -cS '.ticket' "$work/close.json" | sha256sum | cut -d ' ' -f 1)
printf 'close_ticket_sha256=%s\n' "$close_ticket"
wfc status --dispatch "$dispatch" | jq -e --arg id "$message" '
    .dispatches | length == 1 and (.[0] | .backend == "bundled" and .backendAlive == false
        and .backendObservation.cleanup == "confirmed"
        and (.messages | length == 1 and (.[0] | .messageId == $id and .state == "processed")))' >/dev/null
for _ in {1..50}; do
    remaining=false
    for process in "${processes[@]}"; do
        read -r pid ticks <<< "$process"
        if [[ -e "/proc/$pid" ]]; then
            if ! current=$(proc_start "$pid") || [[ "$current" == "$ticks" ]]; then
                remaining=true
            fi
        fi
    done
    if [[ "$remaining" == false && ! -e "$group" ]]; then
        printf 'Default bundled smoke passed: original receipt processed, same ticket closed, original processes and group removed.\n'
        exit 0
    fi
    sleep 0.1
done
printf 'Original process or group still present after the verified close.\n' >&2
exit 1
