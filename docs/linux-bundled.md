# Linux bundled execution backend

The Linux build exposes a backend entry point for dispatch (#5) and reconcile (#18). Each invocation of `wfc --bundled` reads one JSON object on stdin through EOF and prints one JSON result. The launcher, detached guardian, runtime and PTY kernel use the same executable. Set `WFC_STATE` to the owning state directory and `WFC_BUNDLED_CGROUP_ROOT` to a writable cgroup v2 delegation with `cgroup.kill`. The launcher must be allowed to migrate its children into that subtree. Reserve its use for processes trusted to stay within the delegation.

| Operation | Request and result |
| --- | --- |
| `create` | Supply `unit`, `attempt`, absolute `cwd`, string array `argv` and optionally a string map `env` for the worker. Unit and attempt use 1–120 ASCII letters, digits, hyphens or underscores. On acceptance, save the returned `ticket` verbatim and `identity`. Identity records boot ID, PID and start time for the guardian, service, kernel and worker, plus the cgroup path, device and inode. |
| `lookup` | Supply the current unit and attempt to recover its ticket after losing a launch response. The response includes the phase, creator process identity and observed state, and a Unix millisecond deadline for a pending create. Expiry calls for inspection. Only a verified receipt permits cleanup reconciliation. |
| `inspect`, `read`, `resize` | Supply the ticket. A live read/inspect returns `accepted`, PTY data and `last_input`. Resize also requires `cols` and `rows`. Inspect/read after confirmed cleanup return the terminal proof. |
| `input` | Supply the ticket, UTF-8 `text` and positive integer `seq`. Number inputs from 1 in each generation. Keep the sequence number with the workflow input claim. Only the next number can write. An interrupt uses the same operation with U+0003. Host-specific paste/Enter policy belongs to the caller. |
| `close` | Supply the ticket. `completed` requires the guardian's signed empty-tree receipt. After a lost response the adapter makes a read-only proof check. |
| `cleanup-receipt` | Read-only check against the current ticket. Returns `completed` with `proof.proves = execution-empty`, terminal state and actual identity, or `uncertain` with `cleanup = unknown`. |
| `reconcile` | Apply that proof under the unit lock. Exactly one concurrent caller gets `newly_applied = true`. Repeats return the same proof and `false`. Workflow-level transaction/claim ownership remains with #18. |
| next `create` | Supply a new attempt and the exact `previous` ticket. The adapter checks the applied receipt again. A signed pre-launch rejection also permits an explicit replacement. |

A launcher example from a delegated shell follows.

```sh
printf '%s\n' '{"op":"create","unit":"d-example","attempt":"a-1","argv":["/bin/cat"],"cwd":"/tmp"}' | wfc --bundled
```

The guardian places the runtime in its newly created group before exec. The PTY kernel and worker inherit membership. Between connections the runtime reads the PTY to detect worker exit. On runtime exit the guardian uses the original group descriptor to kill the tree, observes `populated 0`, removes empty subgroups and the owned group, and publishes its HMAC-SHA-256 receipt. If the guardian also dies, missing proof leaves cleanup unknown. Only execution emptiness is proved. An outer create/input claim still needs its own delivery outcome.

The private loopback protocol authenticates both endpoints with fresh challenges. Pre-auth frames are bounded to 256 bytes and 0.5 seconds, authenticated requests to 2 MiB and 2 seconds, and a client exchange to 6 seconds. A lost or mismatched reply after sending input returns `uncertain`. The client never retransmits. A pre-send connection or handshake failure returns `definitely-not-sent` for that call. Retry decisions require the original workflow claim and its stable sequence. Keep the private backend directory beneath trusted, owner-controlled state. Keep capability files out of status and logs.

Run the ordinary tests with `cargo test --workspace --locked`. In a disposable real delegation run the native tree cases with the following command.

```sh
WFC_TEST_DELEGATION=/path/to/delegated/group cargo test --locked --test bundled_linux -- --include-ignored --nocapture
```
