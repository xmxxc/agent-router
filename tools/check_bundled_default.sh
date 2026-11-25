#!/bin/sh
# Run from a clean environment with a separately built Linux wfc and jq.
# The delegated case accepts only a dedicated subtree already assigned to this job.
set -eu
binary=$(realpath "$1")
mode=$2
delegation=${WFC_TEST_DELEGATION-}
export PATH=/usr/bin:/bin
case "$mode" in
    ordinary) ;;
    delegated|reply)
        if [ -z "$delegation" ]; then
            echo "BUNDLED_DELEGATION_UNVERIFIED no authorized runner delegation"
            exit 0
        fi
        if ! delegation=$(realpath -e "$delegation"); then
            echo "BUNDLED_DELEGATION_UNVERIFIED supplied delegation is unavailable"
            exit 0
        fi
        if [ "$delegation" = /sys/fs/cgroup ] ||
            [ ! -f "$delegation/cgroup.kill" ] ||
            [ ! -O "$delegation" ] ||
            [ ! -w "$delegation/cgroup.procs" ] ||
            ! grep -Fxq "$$" "$delegation/cgroup.procs"; then
            echo "BUNDLED_DELEGATION_UNVERIFIED runner is outside the supplied writable delegation"
            exit 0
        fi
        ;;
    *) exit 2 ;;
esac
if [ "$mode" = reply ]; then
    composer=$(realpath -e "$(dirname "$0")/../crates/herdr/tests/composer.py")
fi

work=$(mktemp -d)
id=
finish() {
    # The controlled worker has a time limit. Request its natural exit as a
    # fallback if an assertion failed before the successful public close.
    touch "$work/finish"
    if [ -n "$id" ]; then
        "$binary" close "$id" >/dev/null 2>&1 || :
    fi
}
trap finish EXIT
export HOME="$work/home" WFC_STATE="$work/state" WFC_LAUNCH_CONFIG="$work/launch.json"
export WFC_TMUX_SOCKET="$work/missing-tmux.sock" WFC_HERDR_ENDPOINT="$work/missing-herdr.sock"
export WFC_TEST_EXTERNAL="$work/external-called"
unset WFC_CONTROLLER_TOKEN WFC_OWNER_TOKEN WFC_WORKER_CALLBACK WFC_BUNDLED_CGROUP_ROOT
cd "$work"
mkdir home bin ordinary
printf 'ordinary bundled task\n' > task
cat > bin/tmux <<'STUB'
#!/bin/sh
echo "$0" > "$WFC_TEST_EXTERNAL"
exit 1
STUB
cp bin/tmux bin/herdr
chmod +x bin/tmux bin/herdr
export PATH="$work/bin:$PATH"
cat > host <<'HOST'
#!/bin/sh
set -eu
test -z "${WFC_CONTROLLER_TOKEN-}${WFC_OWNER_TOKEN-}"
echo started >> started
for i in $(seq 200); do
    if [ -f report-now ]; then
        cat "$WFC_TASK" > "$WFC_REPORT"
        "$WFC_BIN" done --report "$WFC_REPORT" --attempt "$WFC_ATTEMPT" --message-id fixture-original > done-first
        "$WFC_BIN" done --report "$WFC_REPORT" --attempt "$WFC_ATTEMPT" --message-id fixture-original > done-repeat
        touch reported
        break
    fi
    sleep .05
done
for i in $(seq 300); do
    [ ! -f finish ] || exit 0
    sleep .05
done
HOST
chmod +x host
if [ "$mode" = reply ]; then
    jq -n --arg composer "$composer" '{hosts:{codex:{command:["/usr/bin/timeout", "30s",
        "/usr/bin/python3", $composer, "one", "--submit-first", "--reply-worker", "normal"]}}}' > launch.json
else
    jq -n --arg host "$work/host" '{hosts:{codex:{command:[$host]}}}' > launch.json
fi

dispatch() {
    "$binary" dispatch --host codex --label fixture --cwd . --task task --report report --scope fixture "$@"
}
if [ "$mode" = ordinary ]; then
    if dispatch > reply 2> missing; then exit 1; fi
    grep -F 'WFC_BUNDLED_CGROUP_ROOT' missing
    export WFC_BUNDLED_CGROUP_ROOT="$work/ordinary"
    if dispatch > reply 2> wrong; then exit 1; fi
    grep -F 'expected cgroup v2 filesystem' wrong
    test ! -s reply
    test ! -e external-called
    test ! -e started
    "$binary" status | jq -e '.dispatches==[]'
    echo BUNDLED_DEFAULT_ORDINARY_PASS
    exit 0
fi

export WFC_BUNDLED_CGROUP_ROOT="$delegation"
dispatch > created
id=$(jq -er '.dispatch' created)
jq -e '.backend=="bundled" and .create.result=="accepted"
    and .create.ticket.unit==.dispatch and .create.ticket.attempt==.attempt
    and .backendHandle==.create.ticket and .tmuxSocket==null' created
unset WFC_BUNDLED_CGROUP_ROOT
"$binary" status --dispatch "$id" > running
jq -e --slurpfile create created '.dispatches[0] | .backend=="bundled"
    and .worker.state=="running" and .backendAlive==true
    and .backendHandle==$create[0].create.ticket' running
if [ "$mode" = reply ]; then
    for i in $(seq 200); do
        [ ! -f question.json ] || break
        sleep .05
    done
    qid=$(jq -er '.message' question.json)
    jq -c '{op:"read",ticket:.create.ticket}' created > viewport-query
    ready=
    for i in $(seq 40); do
        "$binary" --bundled "$WFC_STATE" < viewport-query > viewport
        jq -e '.result=="accepted" and .data.screen=="Primary"' viewport >/dev/null
        if jq -e '.data.text | contains("Ask Codex to do anything")' viewport >/dev/null; then
            ready=yes
            break
        fi
        sleep .05
    done
    test "$ready" = yes
    "$binary" reply "$qid" confirmed > answered
    jq -e --slurpfile question question.json --arg id "$id" '
        .replied==$question[0].message and .dispatch==$id' answered
    if "$binary" reply "$qid" repeated > duplicate 2>&1; then exit 1; fi
    grep -F 'already answered' duplicate
    "$binary" status --dispatch "$id" > after-reply
    jq -e --slurpfile created created --slurpfile question question.json '
        [.dispatches[0].messages[] | select(.type=="reply")] | length==1 and
        (.[0] | .inputResult=="accepted" and .to==$question[0].message
            and .questionMessageId==$question[0].messageId and .dispatch==$created[0].dispatch
            and .attempt==$created[0].attempt and .round==$created[0].round)' after-reply
    printf '[router] Answer to question %s: confirmed\n' "$qid" > expected
    cmp expected accepted
    "$binary" close "$id" > closed
    jq -e --slurpfile created created '.result=="completed" and .cleanup=="confirmed"
        and .ticket==$created[0].create.ticket' closed
    test ! -e external-called
    id=
    echo BUNDLED_REPLY_RECEIPT_PASS
    exit 0
fi
touch report-now
for i in $(seq 200); do
    [ ! -f reported ] || break
    sleep .05
done
test -f reported
test "$(cat report)" = 'ordinary bundled task'
jq -e --slurpfile first done-first '.duplicate and .messageId==$first[0].messageId' done-repeat
"$binary" status --dispatch "$id" > reported-row
jq -e --slurpfile done done-first '.dispatches[0] | .status=="completed"
    and .rounds[-1].result.messageId==$done[0].messageId' reported-row
"$binary" close "$id" > closed
"$binary" close "$id" > repeated-close
jq -e --slurpfile first closed --slurpfile create created '
    .result=="completed" and .cleanup=="confirmed" and .proof==$first[0].proof
    and .ticket==$create[0].create.ticket and .proof.identity==$create[0].create.identity
    and .proof.state=="closed"' repeated-close
"$binary" status --dispatch "$id" > exited
jq -e --slurpfile done done-first '.dispatches[0] | .status=="completed"
    and .backendAlive==false and .backendObservation.cleanup=="confirmed"
    and .rounds[-1].result.messageId==$done[0].messageId' exited
test ! -e "$(jq -er .create.identity.containment.path created)"
test "$(wc -l < started)" = 1
test ! -e external-called
jq -c '{dispatch,attempt,ticket,cleanup}' repeated-close
echo BUNDLED_DEFAULT_DELEGATED_PASS
id=
