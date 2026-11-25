#!/bin/sh
# Run from a clean environment with a separately built Linux wfc and jq.
# The delegated case accepts only a dedicated subtree already assigned to this job.
set -eu
binary=$(realpath "$1")
mode=$2
delegation=${WFC_TEST_DELEGATION-}
export PATH=/usr/bin:/bin
input_mode=
user_service=
case "$mode" in
    ordinary|delegated) ;;
    user-service) user_service=yes ;;
    reply|nudge|continue)
        input_mode=yes
        if [ -z "$delegation" ]; then user_service=yes; fi
        ;;
    *) exit 2 ;;
esac
if [ "$user_service" = yes ]; then
    runtime="/run/user/$(id -u)"
    if [ ! -S "$runtime/bus" ]; then
        echo "BUNDLED_USER_SERVICE_UNVERIFIED no current-user manager socket"
        exit 77
    fi
    export XDG_RUNTIME_DIR="$runtime" DBUS_SESSION_BUS_ADDRESS="unix:path=$runtime/bus"
    if ! systemctl --user --no-pager show --property=Version --value; then
        echo "BUNDLED_USER_SERVICE_UNVERIFIED current-user manager unavailable"
        exit 77
    fi
elif [ "$mode" != ordinary ]; then
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
fi
if [ "$input_mode" = yes ]; then
    composer=$(realpath -e "$(dirname "$0")/../crates/herdr/tests/composer.py")
fi

work=$(mktemp -d)
id=
neighbor=
finish() {
    # The controlled worker has a time limit. Request its natural exit as a
    # fallback if an assertion failed before the successful public close.
    touch "$work/finish"
    [ ! -d "$work/neighbor" ] || touch "$work/neighbor/finish"
    if [ -n "$id" ]; then
        "$binary" close "$id" >/dev/null 2>&1 || :
    fi
    if [ -n "$neighbor" ]; then
        "$binary" close "$neighbor" >/dev/null 2>&1 || :
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
if [ "$input_mode" = yes ]; then
    fixture=bundled-paste
    if [ "$mode" = nudge ]; then fixture=bundled-nudge; fi
    if [ "$mode" = continue ]; then fixture=bundled-continue; fi
    jq -n --arg composer "$composer" --arg fixture "$fixture" '{hosts:{codex:{command:["/usr/bin/timeout", "30s",
        "/usr/bin/python3", $composer, "one", "--submit-first", "--reply-worker", $fixture]}}}' > launch.json
else
    jq -n --arg host "$work/host" '{hosts:{codex:{command:[$host]}}}' > launch.json
fi

dispatch() {
    "$binary" dispatch --host codex --label fixture --cwd . --task task --report report --scope fixture "$@"
}
if [ "$mode" = ordinary ]; then
    if [ ! -S "/run/user/$(id -u)/bus" ]; then
        if dispatch > reply 2> missing; then exit 1; fi
        grep -F "current user's systemd user manager" missing
        test ! -s reply
        echo BUNDLED_NO_USER_MANAGER_PASS
    fi
    if WFC_BUNDLED_CGROUP_ROOT= dispatch > reply 2> empty; then exit 1; fi
    grep -F 'WFC_BUNDLED_CGROUP_ROOT' empty
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

if [ "$user_service" != yes ]; then
    export WFC_BUNDLED_CGROUP_ROOT="$delegation"
fi
if [ "$mode" = nudge ] || [ "$mode" = continue ]; then
    owner=$("$binary" owner acquire --scope fixture --owner fixture-controller)
    WFC_CONTROLLER_TOKEN=$(printf '%s\n' "$owner" | jq -er '.token')
    owner_generation=$(printf '%s\n' "$owner" | jq -er '.generation | select(type=="number")')
    export WFC_CONTROLLER_TOKEN
    unset owner
elif [ "$mode" = user-service ]; then
    WFC_CONTROLLER_TOKEN=$("$binary" acquire --scope fixture | jq -er .token)
    export WFC_CONTROLLER_TOKEN
fi
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
if [ "$user_service" = yes ]; then
    unit=$(jq -er '.create.identity.userService.unit' created)
    invocation=$(jq -er '.create.identity.userService.invocation' created)
    uid=$(id -u)
    guardian=$(jq -er '.create.identity.guardian.pid' created)
    worker=$(jq -er '.create.identity.worker.pid' created)
    service_path="/user.slice/user-$uid.slice/user@$uid.service/app.slice/$unit"
    test "$unit" = "wfc-bundled-$(jq -er .create.ticket.generation created).service"
    test "$(jq -er '.create.identity.userService.uid' created)" = "$uid"
    test "$(systemctl --user show "$unit" --property=MainPID --value)" = "$guardian"
    test "$(systemctl --user show "$unit" --property=InvocationID --value)" = "$invocation"
    test "$(systemctl --user show "$unit" --property=ControlGroup --value)" = "$service_path"
    test "$(systemctl --user show "$unit" --property=Delegate --value)" = yes
    test "$(stat -Lc '%d:%i' "/proc/$guardian/exe")" = "$(stat -Lc '%d:%i' "$binary")"
    grep -Fx "0::$service_path" "/proc/$guardian/cgroup"
    grep -Fx "0::$(jq -er '.create.identity.containment.path' created | sed 's|^/sys/fs/cgroup||')" "/proc/$worker/cgroup"
fi
if [ "$mode" = user-service ]; then
    mkdir neighbor
    cp task neighbor/task
    (cd neighbor && dispatch) > neighbor/created
    neighbor=$(jq -er '.dispatch' neighbor/created)
    test "$neighbor" != "$id"
    test "$(jq -er '.create.identity.userService.unit' neighbor/created)" != "$unit"
fi
retired_unit() {
    for i in $(seq 100); do
        if [ ! -e "/sys/fs/cgroup$1" ] &&
            [ "$(systemctl --user show "$2" --property=LoadState --value 2>/dev/null || :)" = not-found ]; then
            return 0
        fi
        sleep .05
    done
    return 1
}
stop_input_worker() {
    "$binary" close "$id" > input-close
    jq -e --slurpfile create created '
        .result=="completed" and .cleanup=="confirmed"
        and .ticket==$create[0].create.ticket and .proof.identity==$create[0].create.identity' input-close
    test ! -e "$(jq -er .create.identity.containment.path created)"
    if [ "$user_service" = yes ]; then retired_unit "$service_path" "$unit"; fi
    id=
}
if [ "$mode" = reply ]; then
    for i in $(seq 200); do
        [ ! -f question.json ] || break
        sleep .05
    done
    qid=$(jq -er '.message' question.json)
fi
if [ "$input_mode" = yes ]; then
    jq -c '{op:"read",ticket:.create.ticket}' created > viewport-query
    read_viewport() {
        if [ "$mode" = continue ]; then
            # The viewport can contain the new callback reference. Store only the needed fields.
            "$binary" --bundled "$WFC_STATE" < viewport-query | jq -c '
                .data.promptVisible=((.data.text // "") | contains("Ask Codex to do anything"))
                | del(.data.text, .data.styled)' > viewport
        else
            "$binary" --bundled "$WFC_STATE" < viewport-query > viewport
        fi
        jq -e --slurpfile create created '.result=="accepted" and .ticket==$create[0].create.ticket
            and .data.screen=="Primary" and .data.exit==null and .data.eof==false' viewport >/dev/null
    }
    ready=
    for i in $(seq 40); do
        read_viewport
        if jq -e '.data.promptVisible or ((.data.text // "") | contains("Ask Codex to do anything"))' viewport >/dev/null; then
            ready=yes
            break
        fi
        sleep .05
    done
    test "$ready" = yes
    sequence=$(jq -er '.last_input | select(type=="number")' viewport)
    expected_sequence=$((sequence + 2))
    require_one_submission() {
        read_viewport
        jq -e --argjson expected "$expected_sequence" '.last_input==$expected' viewport >/dev/null
    }
fi
if [ "$mode" = continue ]; then
    wait_receipt() {
        for i in $(seq 200); do
            [ ! -f "$1" ] || return 0
            sleep .05
        done
        return 1
    }
    touch finish-first
    wait_receipt first.json
    first_id=$(jq -er '.messageId' first.json)
    "$binary" wait --dispatch "$id" --scope fixture --timeout 4 > first-claim
    jq -e --slurpfile first first.json '.messages | length==1 and .[0].id==$first[0].message' first-claim
    "$binary" ack "$first_id" > first-ack
    jq -e --slurpfile first first.json '.state=="processed" and .message==$first[0].message and .messageId==$first[0].messageId' first-ack
    "$binary" status --dispatch "$id" > first-status
    jq -e --slurpfile first first.json '.dispatches[0] | .state=="done" and .backendAlive==true and .attempt==$first[0].attempt' first-status
    printf 'ordinary bundled follow-up\n' > 'next task'
    advance_round() { "$binary" continue "$id" --task 'next task' --report 'next report'; }
    advance_round > continued
    require_one_submission
    advance_round > repeated-continue
    jq -e --slurpfile original continued '. == ($original[0] + {duplicate:true})' repeated-continue
    require_one_submission
    "$binary" status --dispatch "$id" > advanced
    jq -e --slurpfile create created --slurpfile old first-status --slurpfile next continued --arg root "$work" --argjson owner "$owner_generation" '
        .dispatches[0] | .state=="working" and .backendAlive==true and .round==2
        and .attempt==$next[0].attempt and .attempt!=$create[0].attempt and .executionId==$create[0].attempt
        and .create==$create[0].create and .backendHandle==$create[0].create.ticket
        and .task==($root+"/next task") and .report==($root+"/next report")
        and .rounds[0]==$old[0].dispatches[0].rounds[0]
        and .rounds[0].task==$create[0].task and .rounds[0].report==$create[0].report
        and ([.audit[] | select(.operation=="continue")] | length==1 and (.[0] |
            .result=="accepted" and .id==$next[0].audit and .message==$next[0].message and .ownerGeneration==$owner))' advanced
    touch finish-next
    wait_receipt second.json
    jq -e --slurpfile first first.json --slurpfile next continued '
        .pid==$first[0].pid and .attempt==$next[0].attempt and .message!=$first[0].message' second.json
    "$binary" status --dispatch "$id" > completed
    jq -e --slurpfile first first.json --slurpfile ack first-ack --slurpfile second second.json --slurpfile next continued --slurpfile create created '
        .dispatches[0] | .state=="done" and .backendAlive==true and .attempt==$second[0].attempt
        and .backendHandle==$create[0].create.ticket
        and (.rounds | length==2 and .[0].attempt==$first[0].attempt and .[0].result.messageId==$first[0].messageId
            and .[1].attempt==$second[0].attempt and .[1].result.messageId==$second[0].messageId)
        and ([.messages[] | select(.messageId==$first[0].messageId)] | length==1 and (.[0] |
            .id==$first[0].message and .state=="processed" and .statusVersion==$ack[0].statusVersion))
        and ([.messages[] | select(.type=="continue")] | length==1 and (.[0] |
            .id==$next[0].message and .attempt==$second[0].attempt and .inputEvidence=="bundled-appended-user-entry"))' completed
    advance_round > continued-after-done
    jq -e --slurpfile original continued '. == ($original[0] + {duplicate:true})' continued-after-done
    require_one_submission
    test "$(cat report)" = 'first report from the original pane'
    test "$(cat 'next report')" = 'second report from the original pane'
    test ! -e external-called
    stop_input_worker
    echo BUNDLED_CONTINUE_RECEIPT_PASS
    exit 0
fi
if [ "$mode" = nudge ]; then
    notice='ordinary reminder'
    nudge() { "$binary" nudge "$id" --text "$notice" --message-id original-reminder; }
    nudge > nudged
    require_one_submission
    jq -e --slurpfile create created '.result=="accepted" and .operation=="nudge"
        and .dispatch==$create[0].dispatch and .round==$create[0].round and .attempt==$create[0].attempt
        and (.message | type=="number" and .>0) and (.audit | startswith("i-"))' nudged
    jq -e '.data.text | contains("› [router] Message from the controller: ordinary reminder")' viewport >/dev/null
    nudge > repeat-nudge
    jq -e --slurpfile first nudged '. == ($first[0] + {duplicate:true})' repeat-nudge
    require_one_submission
    "$binary" status --dispatch "$id" > after-nudge
    jq -e --slurpfile create created --slurpfile receipt nudged --arg notice "$notice" --argjson owner "$owner_generation" '
        .dispatches[0] | .state=="working" and .backendAlive==true and .backendHandle==$create[0].create.ticket
        and ([.audit[] | select(.operation=="nudge")] | length==1 and
            (.[0] | .id==$receipt[0].audit and .message==$receipt[0].message and .result=="accepted" and .ownerGeneration==$owner))
        and ([.messages[] | select(.type=="nudge")] | length==1 and (.[0] |
            .id==$receipt[0].message and .dispatch==$create[0].dispatch and .round==$create[0].round
            and .attempt==$create[0].attempt and .text==$notice and .inputEvidence=="bundled-appended-user-entry"))' after-nudge
    printf '[router] Message from the controller: %s\n' "$notice" > expected
    cmp expected accepted
    test ! -e external-called
    stop_input_worker
    echo BUNDLED_NUDGE_RECEIPT_PASS
    exit 0
fi
if [ "$mode" = reply ]; then
    "$binary" reply "$qid" confirmed > answered
    require_one_submission
    jq -e --slurpfile question question.json --arg id "$id" '
        .replied==$question[0].message and .dispatch==$id' answered
    "$binary" status --dispatch "$id" > after-reply
    jq -e --slurpfile created created --slurpfile question question.json '
        [.dispatches[0].messages[] | select(.type=="reply")] | length==1 and
        (.[0] | .inputResult=="accepted" and .to==$question[0].message
            and .questionMessageId==$question[0].messageId and .dispatch==$created[0].dispatch
            and .attempt==$created[0].attempt and .round==$created[0].round
            and (.id | type=="number" and .>0) and (.messageId | startswith("m-")))' after-reply
    if "$binary" reply "$qid" repeated > duplicate 2>&1; then exit 1; fi
    grep -F 'already answered' duplicate
    require_one_submission
    "$binary" status --dispatch "$id" > after-repeat
    jq -e --slurpfile first after-reply '
        [.dispatches[0].messages[] | select(.type=="reply")] ==
        [$first[0].dispatches[0].messages[] | select(.type=="reply")]' after-repeat
    printf '[router] Answer to question %s: confirmed\n' "$qid" > expected
    cmp expected accepted
    printf 'BUNDLED_REPLY_PASTE_ENTER_NS=%s\n' "$(cat paste-enter-ns)"
    test ! -e external-called
    stop_input_worker
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
if [ "$mode" = user-service ]; then
    "$binary" wait --scope fixture --timeout 0 > delivered
    jq -e --arg id "$id" --slurpfile done done-first '
        [.messages[] | select(.type=="done" and .dispatch==$id)] | length==1 and .[0].messageId==$done[0].messageId' delivered
    message=$(jq -er '.messageId' done-first)
    "$binary" ack "$message" > acked
    "$binary" ack "$message" > ack-repeat
    jq -e --arg message "$message" '.state=="processed" and .messageId==$message and .duplicate' ack-repeat
fi
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
id=
if [ "$mode" = user-service ]; then
    retired_unit "$service_path" "$unit"
    "$binary" status --dispatch "$neighbor" | jq -e '.dispatches[0] | .backendAlive==true and .worker.state=="running"'
    test "$(wc -l < neighbor/started)" = 1
    "$binary" close "$neighbor" > neighbor/closed
    jq -e '.result=="completed" and .cleanup=="confirmed"' neighbor/closed
    neighbor_unit=$(jq -er '.create.identity.userService.unit' neighbor/created)
    retired_unit "${service_path%/*}/$neighbor_unit" "$neighbor_unit"
    neighbor=
    echo BUNDLED_USER_SERVICE_PASS
else
    echo BUNDLED_DEFAULT_DELEGATED_PASS
fi
