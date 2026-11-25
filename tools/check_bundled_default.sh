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
    user-service)
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
        ;;
    delegated)
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
jq -n --arg host "$work/host" '{hosts:{codex:{command:[$host]}}}' > launch.json

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

if [ "$mode" = delegated ]; then
    export WFC_BUNDLED_CGROUP_ROOT="$delegation"
else
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
if [ "$mode" = user-service ]; then
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
    mkdir neighbor
    cp task neighbor/task
    (cd neighbor && dispatch) > neighbor/created
    neighbor=$(jq -er '.dispatch' neighbor/created)
    test "$neighbor" != "$id"
    test "$(jq -er '.create.identity.userService.unit' neighbor/created)" != "$unit"
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
