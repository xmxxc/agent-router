#!/bin/sh
# Run as a delegated user in a disposable Linux cgroup v2 subtree. Requires
# wfc, jq, setsid, Perl Digest::SHA and strace with syscall fault injection.
set -eu
: "${WFC_BUNDLED_CGROUP_ROOT:?set a disposable delegation}"
work=$(mktemp -d)
export WFC_STATE="$work/state" WFC_LAUNCH_CONFIG="$work/launch.json"
cd "$work"
printf 'public lifecycle\n' > task
cat > host <<'HOST'
#!/bin/sh
set -eu
test -z "${WFC_CONTROLLER_TOKEN-}${WFC_OWNER_TOKEN-}${WFC_WORKER_CALLBACK-}"
printf 'private-terminal-canary\n'
setsid sh -c 'echo $$ > "$WFC_REPORT.desc"; exec sleep 600' &
while [ ! -f "$WFC_REPORT.go" ]; do sleep 0.05; done
cat "$WFC_TASK" > "$WFC_REPORT"
"$WFC_BIN" done --report "$WFC_REPORT" --attempt "$WFC_ATTEMPT" > "$WFC_REPORT.first"
"$WFC_BIN" done --report "$WFC_REPORT" --attempt "$WFC_ATTEMPT" > "$WFC_REPORT.retry"
while [ ! -f "$WFC_REPORT.exit" ]; do sleep 0.05; done
HOST
chmod +x host
jq -nc --arg host "$work/host" '{hosts:{codex:{command:[$host]}}}' > "$WFC_LAUNCH_CONFIG"
dispatch() {
    WFC_OWNER_TOKEN=owner-canary WFC_WORKER_CALLBACK=callback-canary WFC_CONTROLLER_TOKEN=controller-canary \
        wfc dispatch --backend bundled --host codex --cwd . --task task --report "$1" --scope public33 > "$1.create"
    jq -e '.backend=="bundled" and .create.result=="accepted" and .create.ticket.unit==.dispatch and .create.ticket.attempt==.attempt' "$1.create" >/dev/null
}
wait_file() {
    for i in $(seq 1 150); do [ ! -s "$1" ] || return 0; sleep 0.05; done
    echo "missing $1" >&2
    return 1
}
public_row() { wfc status --dispatch "$1" | jq -ec '.dispatches | select(length==1) | .[0]'; }
backend_op() { jq -nc --arg op "$1" --argjson ticket "$2" '{op:$op,ticket:$ticket}' | wfc --bundled; }
tree() {
    ids=$(jq -cr '.create.identity' "$1.create")
    group=$(echo "$ids" | jq -er .containment.path)
    dev_inode=$(stat -c '%d/%i' "$group")
    test "$dev_inode" = "$(echo "$ids" | jq -r '.containment | "\(.device)/\(.inode)"')"
    pids=""
    for role in guardian service kernel worker; do
        pid=$(echo "$ids" | jq -r --arg r "$role" '.[$r].pid')
        start=$(awk '{print $22}' "/proc/$pid/stat")
        test "$start" = "$(echo "$ids" | jq -r --arg r "$role" '.[$r].start_ticks')"
        if [ "$role" != worker ]; then test "$(readlink "/proc/$pid/exe")" = "$(command -v wfc)"; fi
        if [ "$role" != guardian ]; then grep -qx "$pid" "$group/cgroup.procs"; fi
        pids="$pids $pid"
    done
    wait_file "$1.desc"
    child=$(cat "$1.desc")
    grep -qx "$child" "$group/cgroup.procs"
    test "$(awk '{print $6}' "/proc/$child/stat")" = "$child"
    pids="$pids $child"
    echo "TREE $1 ids=$ids descendant=$child device/inode=$dev_inode"
}
gone() {
    for i in $(seq 1 120); do
        left=""
        for pid in $pids; do [ ! -d "/proc/$pid" ] || left="$left $pid"; done
        if [ -z "$left" ] && [ ! -e "$group" ]; then return; fi
        sleep 0.05
    done
    echo "remaining pids=$left group=$group" >&2
    return 1
}
verify_signature() {
    # Rust signs the compact sorted JSON body, without a trailing newline.
    jq -cSj .body "$1/cleanup.json" > body
    signature=$(jq -jr . "$1/key.json" | perl -MDigest::SHA=hmac_sha256_hex -0777 -ne '
        BEGIN { $key = <STDIN> } print hmac_sha256_hex($_, $key)' body)
    test "$signature" = "$(jq -er .signature "$1/cleanup.json")"
    echo "RECEIPT $(cat "$1/cleanup.json")"
}

dispatch sibling
sibling=$(jq -r .dispatch sibling.create)
for outcome in closed exited faulted; do
    dispatch "$outcome"
    id=$(jq -r .dispatch "$outcome.create")
    attempt=$(jq -r .attempt "$outcome.create")
    run="$WFC_STATE/bundled/$id/$(jq -r .create.ticket.generation "$outcome.create")"
    tree "$outcome"
    public_row "$id" > row
    jq -e '.backendHandle==.create.ticket and .tmuxAlive==.backendAlive and .backendAlive and .worker.state=="running" and .status=="online"' row >/dev/null
    if grep -qE 'private-terminal-canary|callback-canary|owner-canary|controller-canary' row; then exit 1; fi
    case "$outcome" in
        closed)
            # The launcher already returned. A different CLI receives this callback.
            touch closed.go
            wait_file closed.retry
            jq -e --slurpfile first closed.first '.duplicate and .messageId==$first[0].messageId' closed.retry >/dev/null
            if WFC_DISPATCH="$id" wfc done --attempt a-000000000000 --report closed > wrong.out 2>&1; then exit 1; fi
            wfc wait --scope public33 --timeout 0 > received
            jq -e --arg id "$id" --arg a "$attempt" '.messages | length==1 and .[0].dispatch==$id and .[0].attempt==$a' received >/dev/null
            wfc ack "$(jq -r '.messages[0].messageId' received)" > ack
            wfc status --active | jq -e --arg id "$id" '.reclaimable | any(.dispatch==$id)' >/dev/null
            wfc close "$id" > close
            jq -e '.result=="completed" and .cleanup=="confirmed" and .proof.state=="closed"' close >/dev/null
            wfc close "$id" | jq -e --slurpfile c close '.proof==$c[0].proof' >/dev/null
            echo "DONE first=$(cat closed.first) retry=$(cat closed.retry) ack=$(cat ack)"
            ;;
        exited) touch exited.go exited.exit ;;
        faulted) kill -KILL "$(echo "$ids" | jq -r .service.pid)" ;;
    esac
    wait_file "$run/cleanup.json"
    gone
    verify_signature "$run"
    public_row "$id" > row
    jq -e --arg s "$outcome" '.backendAlive==false and .backendObservation.cleanup=="confirmed" and .backendObservation.proof.state==$s' row >/dev/null
    case "$outcome" in
        closed) jq -e '.status=="completed" and .worker.state=="cancelled" and .messages[0].state=="processed"' row >/dev/null ;;
        exited) jq -e '.worker.state=="exited" and .status=="completed"' row >/dev/null ;;
        faulted) jq -e '.worker.state=="exited" and .status=="failed"' row >/dev/null ;;
    esac
    if [ "$outcome" = exited ]; then
        mkdir "$WFC_STATE/bundled/$id/interrupted-generation"
        strace -f -qq -e trace=connect,sendto -o terminal-trace wfc close "$id" > terminal.reply
        test ! -s terminal-trace
        jq -e '.cleanup=="unknown" and .execution_cleanup=="confirmed" and .execution_proof.state=="exited"' terminal.reply >/dev/null
        echo FIRST_CLOSE_TERMINAL_EXECUTION_ONLY
    fi
    public_row "$sibling" | jq -e '.backendAlive==true' >/dev/null
    echo "LIFECYCLE $outcome cleanup=confirmed all_pids_absent=true sibling_alive=true"
done
# An unreadable index before a send leaves a later close available after repair.
sibling_unit="$WFC_STATE/bundled/$sibling"
# Simulate losing the published create reply. Lookup must bind the original
# attempt without launching another worker or creating a missing unit.
original_row="$WFC_STATE/dispatches/$sibling.json"
cp "$original_row" saved-row
jq 'del(.create)' saved-row > "$original_row"
public_row "$sibling" | jq -e --slurpfile r saved-row '.createPending and .backendAlive and .backendHandle==$r[0].create.ticket' >/dev/null
mv "$sibling_unit" saved-unit
public_row "$sibling" | jq -e '.backendAlive==null' >/dev/null
test ! -e "$sibling_unit"
mv saved-unit "$sibling_unit"
mv saved-row "$original_row"
mv "$sibling_unit/current.json" saved-current
strace -f -qq -e trace=connect,sendto -o preflight wfc close "$sibling" > preflight.reply
test ! -s preflight
jq -e '.result=="uncertain" and .cleanup=="unknown"' preflight.reply >/dev/null
mv saved-current "$sibling_unit/current.json"
public_row "$sibling" | jq -e '.backendAlive==true' >/dev/null
wfc close "$sibling" | jq -e '.result=="completed"' >/dev/null
mv "$sibling_unit" saved-unit
public_row "$sibling" | jq -e '.backendAlive==null' >/dev/null
test ! -e "$sibling_unit"
mv saved-unit "$sibling_unit"
chmod 0755 "$sibling_unit"
wfc close "$sibling" | jq -e '.cleanup=="unknown" and .proof==null' >/dev/null
chmod 0700 "$sibling_unit"
echo PRE_SEND_RECOVERED_AND_READ_ONLY_STATUS

# Seed the row normally. Build a second attempt through the private primitive
# to model an existing resumed row. Public resume/reconcile are separate tickets.
for mode in ordered intact missing mismatch; do
    dispatch "$mode"
    id=$(jq -er .dispatch "$mode.create")
    unit="$WFC_STATE/bundled/$id"
    row="$WFC_STATE/dispatches/$id.json"
    a=$(jq -c .create.ticket "$mode.create")
    a_run="$unit/$(echo "$a" | jq -r .generation)"
    run="$a_run"
    if [ "$mode" != ordered ]; then
        wfc close "$id" | jq -e '.cleanup=="confirmed"' >/dev/null
        backend_op reconcile "$a" | jq -e .newly_applied >/dev/null
        cp "$unit/current.json" saved-index
        next=a-222222222222
        jq -nc --arg u "$id" --arg a "$next" --argjson p "$a" '{op:"create",unit:$u,attempt:$a,previous:$p,cwd:"/tmp",argv:["/bin/cat"]}' |
            wfc --bundled > second
        jq -e '.result=="accepted"' second >/dev/null
        if wfc close "$id" > stale.out 2> stale.err; then exit 1; fi
        grep -q 'stale execution identity' stale.err
        jq --arg a "$next" --slurpfile b second '.attempt=$a | .round=2 | .create=$b[0]' "$row" > row.tmp
        mv row.tmp "$row"
        run="$unit/$(jq -r .ticket.generation second)"
        # Simulate loss of a retirement and restoration of a genuine old index.
        mv "$a_run/superseded.json" saved-retirement
        cp saved-index "$unit/current.json"
        public_row "$id" | jq -e '.round==2 and .backendAlive==null and .worker.state=="running" and .backendObservation.scope=="execution"' >/dev/null
    fi
    group=$(jq -er .body.data.containment.path "$run/ready.json")
    strace -f -qq -s 0 -e trace=connect,sendto,recvfrom -e inject=recvfrom:error=EIO:delay_exit=3000000:when=2 \
        -o trace wfc close "$id" > reply &
    tracer=$!
    wait_file "$run/cleanup.json"
    test ! -s reply
    cp "$run/cleanup.json" original
    case "$mode" in
        missing) mv "$run/cleanup.json" removed ;;
        mismatch) cp "$a_run/cleanup.json" "$run/cleanup.json" ;;
    esac
    wait "$tracer"
    grep -q INJECTED trace
    test "$(grep -c 'connect(' trace)" = 1
    test "$(grep -c 'sendto(' trace)" = 3
    test ! -e "$group"
    case "$mode" in
        ordered) jq -e --slurpfile p original '.result=="completed" and .cleanup=="confirmed" and .proof==$p[0].body.data' reply >/dev/null ;;
        intact) jq -e --slurpfile p original '.result=="uncertain" and .scope=="execution" and .cleanup=="unknown" and .execution_cleanup=="confirmed" and .execution_proof==$p[0].body.data and .ticket==$p[0].body.ticket' reply >/dev/null ;;
        *) jq -e '.result=="uncertain" and .cleanup=="unknown" and .execution_proof==null' reply >/dev/null ;;
    esac
    strace -f -qq -e trace=connect,sendto -o retry-trace wfc close "$id" > retry
    test ! -s retry-trace
    public_row "$id" > status
    jq -e --slurpfile r retry '.backendObservation.result==$r[0].result and .backendObservation.execution_proof==$r[0].execution_proof and .backendObservation.proof==$r[0].proof' status >/dev/null
    if [ "$mode" != ordered ]; then
        jq -e '.backendAlive==null and .backendObservation.cleanup=="unknown"' status >/dev/null
        case "$mode" in
            missing|mismatch)
                jq -e '.backendObservation.execution_cleanup==null and .worker.state=="unknown"' status >/dev/null
                cp original "$run/cleanup.json" ;;
        esac
        public_row "$id" | jq -e --slurpfile p original '.backendAlive==null and .backendObservation.cleanup=="unknown" and .backendObservation.execution_proof==$p[0].body.data' >/dev/null
        jq -nc --arg u "$id" --argjson p "$a" '{op:"create",unit:$u,attempt:"replacement",previous:$p,cwd:"/tmp",argv:["/bin/true"]}' |
            wfc --bundled | jq -e '.result=="uncertain" and .cleanup=="unknown"' >/dev/null
    fi
    verify_signature "$run"
    echo "CLOSE_LOSS $mode frames=3 connections=1 retry_frames=0 reply=$(cat reply)"
done
echo PUBLIC_BUNDLED_PASS
