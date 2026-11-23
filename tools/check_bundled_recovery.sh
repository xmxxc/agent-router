#!/bin/sh
# Run under an unprivileged, disposable cgroup v2 delegation with wfc, jq, flock
# and strace. Each public request runs in a fresh CLI process.
set -eu
: "${WFC_BUNDLED_CGROUP_ROOT:?set a disposable delegation}"
cd "$(mktemp -d)"
export NO_COLOR=1
export WFC_STATE="$PWD/state" WFC_LAUNCH_CONFIG="$PWD/config"
echo review > task
cat > host <<'HOST'
#!/bin/sh
set -eu
r="$WFC_REPORT"
echo "$WFC_DISPATCH" >> "$r.id"
echo "$WFC_ATTEMPT" > "$r.attempt"
echo independent-report > "$r"
while :; do
    for key in first retry other; do
        if [ -f "$r.$key.go" ] && [ ! -f "$r.$key.rc" ]; then
            msg=initial
            [ "$key" != other ] || msg=later
            rc=0
            "$WFC_BIN" done --report "$r" --attempt "$WFC_ATTEMPT" --message-id "$msg:$WFC_DISPATCH" > "$r.$key.out" 2>"$r.$key.err" || rc=$?
            echo "$rc" > "$r.$key.rc"
        fi
    done
    [ ! -f "$r.fail" ] || exit 17
    [ ! -f "$r.exit" ] || exit 0
    sleep .05
done
HOST
chmod 755 host
mkdir bin
cat > bin/git <<'GIT'
#!/bin/sh
case "${WFC_REPORT-}" in
    */racing)
        echo sampling > "$WFC_REPORT.sampling"
        while [ ! -f "$WFC_REPORT.release" ]; do sleep .01; done ;;
esac
exit 1
GIT
chmod +x bin/git
export PATH="$PWD/bin:$PATH"
jq -nc --arg h "$PWD/host" '{hosts:{codex:{command:[$h]}}}' > config
wait_file() {
    for i in $(seq 1 200); do
        [ ! -s "$1" ] || return 0
        sleep .05
    done
    echo "timed out waiting for $1" >&2
    return 1
}
dispatch() {
    r=$1
    wfc dispatch --host codex --backend bundled --label "$r" --cwd . --task task --report "$r" --scope review > "$r.create"
    jq -e '.create.result=="accepted"' "$r.create" >/dev/null
    wait_file "$r.id"
    id=$(cat "$r.id")
    unit="$WFC_STATE/bundled/$id"
    gen=$(jq -r .create.ticket.generation "$r.create")
    receipt="$unit/$gen/cleanup.json"
}
row() {
    wfc status --dispatch "$id" | jq -ce '.dispatches[0] |
        {dispatch,status,worker:.worker.state,alive:.backendAlive,result:.rounds[-1].result.messageId,
         cleanup:.backendObservation.cleanup,messages:[.messages[] | .messageId]}'
}
active() {
    wfc status --active | jq -ce --arg id "$id" '{
        active:any(.dispatches[];.dispatch==$id),reclaimable:any(.reclaimable[];.dispatch==$id)}'
}
callback() {
    touch "$r.$1.go"
    wait_file "$r.$1.rc"
    echo "CALLBACK $r/$1 rc=$(cat "$r.$1.rc") $(cat "$r.$1.out" "$r.$1.err")"
}
pending() {
    strace -qq -e trace=connect,sendto -e inject=sendto:error=EIO:when=3 -o trace wfc close "$id" > close
    grep -q INJECTED trace
    test "$(grep -c 'sendto(' trace)" = 3
    jq -e '.cleanup=="unknown" and .result=="uncertain"' close >/dev/null
    strace -qq -e trace=connect,sendto -o retry.trace wfc close "$id" > retry
    test ! -s retry.trace
    test "$(grep -c 'connect(' trace)" = 1
    echo "PENDING connections=1 send_calls=3 $r $(cat close) retry_frames=0"
}
finish() {
    touch "$r.exit"; wait_file "$receipt"
    wfc close "$id" | jq -ce 'select(.cleanup=="confirmed") | {result,cleanup,terminal:.proof.state}'
}
readonly_close() {
    strace -qq -e trace=connect,sendto -o read.trace wfc close "$id" > read.reply
    test ! -s read.trace
}
dispatch neighbor
neighbor=$id
neighbor_ok() {
    ticks=$(awk '{print $22}' "/proc/$(jq -r .create.identity.worker.pid neighbor.create)/stat")
    test "$ticks" = "$(jq -r .create.identity.worker.start_ticks neighbor.create)"
    wfc status --dispatch "$neighbor" | jq -e --slurpfile c neighbor.create '.dispatches[0] | .backendAlive and .backendHandle==$c[0].create.ticket' >/dev/null
}
for name in late existing racing; do
    dispatch "$name"
    case "$name" in
        existing) callback first; test "$(cat "$r.first.rc")" = 0 ;;
        racing) touch "$r.first.go"; wait_file "$r.sampling" ;;
    esac
    pending
    if [ "$name" = existing ]; then
        callback retry
        jq -e --slurpfile f "$r.first.out" '.duplicate and (del(.duplicate)==($f[0]|del(.duplicate)))' "$r.retry.out" >/dev/null
        callback other
        echo conflict > conflict.report
        if WFC_DISPATCH="$id" wfc done --attempt "$(cat "$r.attempt")" --report conflict.report --message-id "initial:$id"; then exit 1; fi
        test "$(cat "$r.other.rc")" != 0
        msg=$(jq -er .messageId "$r.first.out")
        row | jq -ce --arg m "$msg" 'select(.messages==[$m] and .result==$m and .status=="completed")'
    else
        if [ "$name" = racing ]; then touch "$r.release"; fi
        callback first
        test "$(cat "$r.first.rc")" != 0
        row | jq -ce 'select(.messages==[] and .status=="unknown")'
    fi
    finish
    if [ "$name" = existing ]; then
        # Existing installations wrote the ticket alone as the intent.
        marker="$WFC_STATE/dispatches/$id.$(cat "$r.attempt").closing"
        jq -e .ticket "$marker" > old-intent
        mv old-intent "$marker"
        readonly_close
        jq -e --slurpfile s "$receipt" '.proof==$s[0].body.data' read.reply >/dev/null
        WFC_DISPATCH="$id" wfc done --attempt "$(cat "$r.attempt")" --report "$r" --message-id "initial:$id" > replay
        jq -ce --slurpfile f "$r.first.out" 'select(.duplicate and (del(.duplicate)==($f[0]|del(.duplicate))))' replay
        if WFC_DISPATCH="$id" wfc done --attempt "$(cat "$r.attempt")" --report "$r" --message-id "post-close:$id"; then exit 1; fi
        row | jq -ce --arg m "$msg" 'select(.messages==[$m] and .result==$m)'
    fi
    neighbor_ok
done
dispatch snapshot
exec 9<"$unit/lock"
/bin/flock -x 9
wfc status --dispatch "$id" 9<&- > snapshot.old &
reader=$!
blocked=no
for i in $(seq 1 200); do
    for fd in /proc/$reader/fd/*; do
        if [ "$(readlink "$fd" || true)" = "$unit/lock" ]; then blocked=yes; break; fi
    done
    [ "$blocked" = no ] || break
    sleep .02
done
test "$blocked" = yes
callback first
touch "$r.exit"
wait_file "$receipt"
/bin/flock -u 9
exec 9<&-
wait "$reader"
echo "SNAPSHOT first=$(jq -c '.dispatches[0]|{status,worker:.worker.state,terminal:.backendObservation.proof.state,messages:[.messages[]|.messageId]}' snapshot.old) next=$(row)"

jq -ce --slurpfile f "$r.first.out" '.dispatches[0] | select(.status=="completed" and .worker.state=="exited" and .rounds[-1].result.messageId==$f[0].messageId and [.messages[].messageId]==[$f[0].messageId]) | {dispatch,status,result:.rounds[-1].result.messageId}' snapshot.old
neighbor_ok
# Hold the publication lock until the worker has exited. The close had
# already sampled a live worker before it started waiting on this lock.
dispatch while_waiting
exec 9<"$WFC_STATE/messages.lock"
/bin/flock -x 9
strace -qq -e trace=connect,sendto -o waiting.trace sh -c 'echo $$ > waiting.pid; exec wfc close "$1"' close "$id" 9<&- > waiting.reply &
tracer=$!
wait_file waiting.pid
reader=$(cat waiting.pid)
blocked=no
for i in $(seq 1 200); do
    for fd in /proc/$reader/fd/*; do
        if [ "$(readlink "$fd" || true)" = "$WFC_STATE/messages.lock" ]; then blocked=yes; break; fi
    done
    [ "$blocked" = no ] || break
    sleep .02
done
test "$blocked" = yes
touch "$r.fail"
wait_file "$receipt"
/bin/flock -u 9
exec 9<&-
wait "$tracer"
test ! -s waiting.trace
jq -e '.cleanup=="confirmed" and .proof.state=="exited"' waiting.reply >/dev/null
mv "$receipt" saved
row | jq -ce 'select(.status=="failed" and .worker=="exited" and .alive==null and .cleanup=="unknown")'
readonly_close
mv saved "$receipt"
echo "EXIT_DURING_CLOSE_LOCK original=$id initial_frames=0 repeat_frames=0 failure_preserved=true"
neighbor_ok
for name in during_close after_close; do
    dispatch "$name"
    touch "$r.fail"; wait_file "$receipt"
    row | jq -ce 'select(.status=="failed" and .worker=="exited" and .cleanup=="confirmed")'
    if [ "$name" = during_close ]; then
        strace -qq -o failed.trace -e trace=fsync,sendto,connect -e inject=fsync:delay_exit=4s:when=2 wfc close "$id" > failed.reply &
        tracer=$!
        wait_file "$WFC_STATE/dispatches/$id.$(cat "$r.attempt").closing"
        test ! -s failed.reply
        mv "$receipt" saved
        wait "$tracer"
        jq -e '.result=="uncertain" and .cleanup=="unknown"' failed.reply >/dev/null
    else
        strace -qq -o failed.trace -e trace=sendto,connect wfc close "$id" > failed.reply
        jq -e '.cleanup=="confirmed" and .proof.state=="exited"' failed.reply >/dev/null
        mv "$receipt" saved
    fi
    if grep -Eq '(connect|sendto)\(' failed.trace; then exit 1; fi
    readonly_close
    jq -e '.result=="uncertain" and .cleanup=="unknown"' read.reply >/dev/null
    echo "RECEIPT_HIDDEN $r frames=0 retry_frames=0 $(row)"
    row | jq -e '.status=="failed" and .worker=="exited" and .alive==null and .cleanup=="unknown" and .messages==[]' >/dev/null
    mv saved "$receipt"
    readonly_close
    jq -e --slurpfile s "$receipt" '.cleanup=="confirmed" and .proof==$s[0].body.data' read.reply >/dev/null
    neighbor_ok
done
dispatch sync
rc=0
strace -qq -e trace=fsync,connect,sendto -e inject=fsync:error=EIO:when=2 -o sync.trace wfc close "$id" > sync.reply 2>sync.err || rc=$?
test "$rc" != 0
grep -q INJECTED sync.trace
if grep -Eq '(connect|sendto)\(' sync.trace; then exit 1; fi
test ! -e "$WFC_STATE/dispatches/$id.$(cat "$r.attempt").closing"
row | jq -e '.alive and .worker=="running"' >/dev/null
strace -qq -e trace=connect,sendto -o sync.retry.trace wfc close "$id" > sync.retry
test "$(grep -c 'connect(' sync.retry.trace)" = 1
test "$(grep -c 'sendto(' sync.retry.trace)" = 3
readonly_close
jq -ce --slurpfile r sync.retry 'select(.proof==$r[0].proof and .cleanup=="confirmed") | {dispatch,attempt,cleanup,state:.proof.state}' read.reply
echo "DIR_FSYNC initial_rc=$rc initial_frames=0 retry_connections=1 retry_frames=3 repeat_frames=0"
neighbor_ok
# Stop the publisher after reserving its identity, before creating the runtime.
strace -qq -e trace=clone,clone3 -e inject=clone,clone3:delay_enter=12s -o pre.trace sh -c 'echo $$ > pre.pid; exec wfc dispatch --host codex --backend bundled --label before --cwd . --task task --report before --scope review' > pre.create &
tracer=$!
before_id=
for i in $(seq 1 200); do
    for pending in "$WFC_STATE"/dispatches/*.pending; do
        [ -f "$pending" ] || continue
        before_id=$(jq -er 'select(.label=="before") | .dispatch' "$pending" || :)
        [ -z "$before_id" ] || break
    done
    [ -z "$before_id" ] || break
    sleep .05
done
test -n "$before_id"
kill -KILL "$(cat pre.pid)"
wait "$tracer" || :
wfc status --dispatch "$before_id" | jq -ce '.dispatches[0] | select(.createPending and .backendAlive==null) | {dispatch,attempt,createPending,backendAlive}'
strace -qq -e trace=connect,sendto -o pre.close.trace wfc close "$before_id" > pre.close
test ! -s pre.close.trace
jq -ce 'select(.result=="uncertain" and .cleanup=="unknown")' pre.close
test ! -e before.id
test ! -e "$WFC_STATE/bundled/$before_id"
echo "PRELAUNCH_KILLED id=$before_id workers=0 sends=0"
neighbor_ok
r=publisher
strace -qq -e trace=rename,renameat,renameat2 -e inject=rename,renameat,renameat2:delay_enter=12s:when=2 -o publisher.trace sh -c 'echo $$ > publisher.pid; exec wfc dispatch --host codex --backend bundled --label publisher --cwd . --task task --report publisher --scope review' > publisher.create 2>publisher.err &
tracer=$!
wait_file publisher.id
id=$(cat publisher.id)
unit="$WFC_STATE/bundled/$id"
test -f "$WFC_STATE/dispatches/$id.pending"
test ! -e "$WFC_STATE/dispatches/$id.json"
test ! -s publisher.create
kill -KILL "$(cat publisher.pid)"
wait "$tracer" || :

a=$(cat "$r.attempt")
jq -nc --arg u "$id" --arg a "$a" '{unit:$u,attempt:$a,op:"lookup"}' | wfc --bundled > lookup
jq -e '.result=="accepted" and .ticket!=null' lookup >/dev/null
jq -nc --slurpfile l lookup '{op:"inspect",ticket:$l[0].ticket}' | wfc --bundled > inspected
jq -e '.result=="accepted" and .identity.worker.pid!=null' inspected >/dev/null
test "$(awk '{print $22}' "/proc/$(jq -r .identity.worker.pid inspected)/stat")" = "$(jq -r .identity.worker.start_ticks inspected)"
wfc status --dispatch "$id" > named
jq -ce --slurpfile l lookup --arg a "$a" '.dispatches | select(length==1) | .[0] | select(.createPending and .attempt==$a and .backendHandle==$l[0].ticket and .backendAlive==true) | {dispatch,attempt,backendHandle,createPending,backendAlive}' named
wfc status | jq -e --arg d "$id" '[.dispatches[]|select(.dispatch==$d)] | length==1' >/dev/null
active | jq -ce 'select(.active and (.reclaimable|not))'
strace -qq -e trace=connect,sendto -o recovered.trace wfc close "$id" > recovered
test "$(grep -c 'connect(' recovered.trace)" = 1
test "$(grep -c 'sendto(' recovered.trace)" = 3
receipt="$unit/$(jq -r .ticket.generation lookup)/cleanup.json"
wait_file "$receipt"
jq -ce --slurpfile l lookup --slurpfile s "$receipt" --slurpfile i inspected 'select(.cleanup=="confirmed" and .proof==$s[0].body.data and .proof.identity==$i[0].identity and $s[0].body.ticket==$l[0].ticket) | {dispatch,attempt,cleanup,proof}' recovered
readonly_close
jq -e --slurpfile r recovered '.proof==$r[0].proof' read.reply >/dev/null
test "$(wc -l < publisher.id)" = 1
test "$(find "$unit" -mindepth 1 -maxdepth 1 -type d | wc -l)" = 1
echo "PUBLISHER_KILLED original=$id attempt=$a launches=1 generations=1 connections=1 sends=3 repeat_frames=0"
neighbor_ok
echo "NEIGHBOR_UNCHANGED $(jq -c '{dispatch,attempt,worker:.create.identity.worker,ticket:.create.ticket}' neighbor.create)"
wfc close "$neighbor" | jq -ce 'select(.cleanup=="confirmed") | {dispatch,cleanup}'
echo PUBLIC_BUNDLED_RECOVERY_PASS
