# Linux tmux 后端接口

调度层可调用 `tmux::execute(state, request)`，也可启动一次 `wfc --tmux-backend`，将一个 UTF-8 JSON 对象写入标准输入后关闭输入，读取标准输出的一个 JSON 对象。后者用于跨进程集成和独立复验。每个请求带 `dispatch` 和 `op`。Linux 的新请求与旧记录中缺省、null、空字符串的 `backend` 都选 tmux，也接受显式的 `"tmux"`。其他后端由上层路由到对应实现。

| op | 输入和返回证据 |
| --- | --- |
| create | 必填 `cwd`、非空 `argv` 字符串数组，可选窗口名 `label` 和字符串映射 `env`。以 detached 方式启动 `wfc-<dispatch>`。启动结果引用私有 `receipt` 文件，记录宿主已启动、启动失败或已退出。 |
| inspect | 返回 `alive`、`owned`。已确认归属时还返回原 worker 窗格是否已知的 `workerBound` 和同一 wfc 程序的 `attach` 参数数组，可在交互终端直接执行，执行时由 `--tmux-attach` 复核原服务的进程身份并固定 socket，再由原生 tmux 核对会话、pane、启动标记、窗口独占性和会话级 WFC_STATE，成功后可用 tmux 的 detach 键离开。 |
| read | 返回当前 pane 的 `screen`。 |
| submit | 必填 `text`，可选 `host`。在登记的 worker pane 上检查画面并按字面发送一次文字。每次按键都复核目标 pane 的会话归属和窗口独占性。Codex 有稳定匹配草稿时，最多补两次 Enter。 |
| interrupt | 向登记且仍归该会话独占的 worker pane 发送 C-c。 |
| resume | 接收与 create 相同的启动数据。调用方提供恢复原宿主会话的 argv。OpenCode 另取 `text`，在画面稳定后提交。Codex 可传 `probe_read_only: true` 检查恢复后的只读提示。 |
| close | 关闭前核对会话归属和全部窗口独占性。原 worker pane 应在其中，或同次启动已有进程退出回执且原 pane 已从整个服务消失。原会话消失后，已登记的 pane 仍在相邻会话中时保留未决。已确认关闭时返回 `result=completed`，可选字段 `closeEffect` 说明本次调用做了什么。 |
| revoke | 要求原生 `executionId` 和 `closeOperationId`。核销尚未消费的同次预留或重放该操作的核销事实。只访问本地登记、启动描述与回执。 |

| closeEffect | 本次 close 的证据 |
| --- | --- |
| `already_absent` | 核实原会话已不存在，本次没有关闭会话。重复 close 也可得到此值。worker 的退出状态须另查同次启动的回执。 |
| `session_closed` | 在原服务端复核会话归属、窗口独占、原 pane 仍活动，随后确认 kill-session 成功。该值说明本次主动关闭了活动会话。 |
| `unstarted_session_closed` | 普通 close 核实并关闭了同次启动留下的单一已退出监督窗格。已验证的同次 `not_started` 回执仍记录 worker 从未启动。 |
| `worker_exited` | 在关闭前已取得同次启动的 `process_exited` 及有效退出码，随后确认关闭其剩余会话。调用方从原启动回执读取退出码。 |

#42 在归档前应同时核对 dispatch、attempt 和轮次。`already_absent` 和 `worker_exited` 应保留同次执行的退出记录；缺少退出证据时保留未决。`session_closed` 记录本次主动关闭会话的动作。worker 自然退出与关闭动作可能重叠，单凭此值无法断定进程的死因。归档时应保留同次执行已确认的退出结果，并让随后取得的可靠退出证据可以对账。旧后端可能只返回 `result=completed`。遇到缺失或未知的 `closeEffect`，以及 `result=uncertain` 时，应保留已有执行证据，将未查明的退出原因留待对账。单凭 completed 或关闭前的 running 状态推断取消，会丢失恰在此时产生的真实退出码。

例如，已创建会话后可以在另一个进程读屏。

```sh
printf '%s\n' '{"op":"read","dispatch":"d-abc12345"}' | WFC_STATE=/path/to/state wfc --tmux-backend
```

用同一 `WFC_STATE` 与同一 tmux 服务访问会话。独立服务可用 `WFC_TMUX_SOCKET` 指定绝对 socket 路径，缺省沿用 tmux 的服务选择。调度层负责持久化服务选择、串行化同一 attempt 的业务操作，并向当前 worker 提供专用的 `env.WFC_WORKER_CALLBACK`。

### 公开 CLI 的调用约定

- **新派工预检。** 已知是全新派工时，inspect 或 create 传 `freshDispatch: true`。空 state、原 socket 尚不存在时，inspect 返回缺席，随后 create 可启动。无标识的兼容预检在服务不可达时可以返回未决，且不写入匿名待接管记录。显式新派工还可清理同一 socket 上 R3/R4 遗留的无身份预检记录。已有具体执行身份的登记始终参与核验。
- **原生启动。** #42 在调用 create 或 resume 之前保存本次新启动的 `executionId`（如唯一 attempt ID）。后端在启动 tmux 前将该 ID 与私有启动编号关联。同一 dispatch 的下一次实际启动使用新 ID。启动前中断仍可按原 ID 核销尚未消费的预留；核销关闭后，该 ID 只用于查询或重放。启动响应丢失时，以同一 state、dispatch、服务选择及 `executionId` 调 inspect。已核实原启动后，返回 `executionId`、32 位小写十六进制 `launchId` 和 `receipt` 路径。worker 已退出时也可取得关联。公开层核对自己的 dispatch、round、attempt、返回的执行标识及 state 下的 `launches/<launchId>.json`，再读取该 worker 的终态。未登记或证据不足时维持未决。普通 inspect 也可沿用旧接口。
- **原生终态与现场查询。** 创建响应丢失后，可直接用登记过的 `executionId` 首次查询。原 socket 暂离线而同次退出回执已写盘时，inspect 保持 `uncertain`，同时返回 `executionId`、`launchId` 和原 `receipt`。调用方从该文件读取退出码。已启动但未退出、回执不可读或编号不匹配时省略关联。新写的服务器记录与启动、退出回执含有同一个启动编号。早期私有记录沿用已登记编号、原服务器记录及 state 中同名回执的关联。
- **旧执行。** 首次探测前传历史记录的稳定 `legacyExecutionId` 以及原 state、dispatch 和服务选择，即使首次操作为 create 或 resume。可信原窗格传 `legacyWorkerPane`。R1 的私有启动回执编号可传 `legacyLaunchId`，如果它本身就是 `legacyExecutionId` 可省略。后端在原会话中核对监督进程启动时的 state、dispatch 和 tmux 服务及会话、启动参数、私有 started 回执及其 worker 的父进程，保存匹配的回执编号。若使用通用 attempt ID，且它也是 32 位十六进制字符串，请另传真正的 `legacyLaunchId`。原 pane 已离开整个服务、同一回执随后记录退出时，可清理剩余独占窗口。带原 `legacyExecutionId` 的 inspect 在已核验关联时返回该标识、`launchId` 与原 `receipt` 路径。关闭后或同名新执行出现后，重开 CLI 仍可按原标识查回。原服务暂时离线时，现场检查仍返回 uncertain，已核验关联若有可读的原退出回执，则同时返回三个关联字段。未验证的启动提示可由实际监督进程、正确启动编号或可信原窗格重新核实。用执行编号兼作启动提示时，显式传入同值与省略均要求相同证据。请核对原 state 和 `launches/<launchId>.json`，再读取退出码。仅有会话归属、回执缺失或仅由调用方提供路径时，省略关联字段。
- **只凭窗格接管。** 传 `legacyWorkerPane` 时，调用方须能确认这个窗格运行的是当前要接管的执行。后端把首次可信观测的原 pane 直属进程 PID 和 Linux 启动时刻与已核验的服务、会话一并保存。之后的输入、业务读屏和主动关闭核对进程是否仍是同一次启动，并在实际 tmux 命令中核对 pane PID。首次收到提示时服务离线，恢复后还要确认服务持有首次提示时的原监听套接字，且进程启动时刻严格早于提示。无法区分的同一时钟刻度保留未决。旧登记只有 pane 编号且没有进程证据时，仍可观察，恢复业务绑定需要独立启动证据。
- **观察与业务输入。** `owned` 表示会话归属，已知原 worker 窗格时 inspect 同时返回 `workerBound: true`。原生执行在每次业务发键前核对原监督进程及仍在运行的 worker，并在 tmux 发键时核对 pane 的实时进程。原窗格被重用时，inspect 报告 workerBound=false。首次接管时即使只剩一个 pane，也须有可信原 pane 或可核验的原启动关联才能发送文字、Enter 或 C-c。未建立绑定的会话仍可 inspect/read；业务输入和关闭保留未决。旧 Observed 记录中可能猜测出来的窗格也需要补证。只有会话名和 state 时，调用方须先定位原服务。
- **按执行重放。** 普通 close 延续“本次动作”的语义。需要重放时，#42 在首次 close 前保存稳定的 `closeOperationId`（例如 `close:<attempt>`），与 `executionId` 或已核验的 `legacyExecutionId` 一起发送。公开归档失败后用同一对 ID 重试，返回首次已确认的 `closeEffect` 及 `closeOperationId`，不会再次关闭会话。保护检查明确拒绝关闭时，同一操作号可安全重试。若后端只保存了操作意图且无法证实动作，返回 uncertain。其他执行或操作的事实不能为它补证。历史执行的 inspect/close 从本方核销记录作答，同名后继会话保留自己的登记。最近一次已核销执行仍可按现有约定 resume。
- **启动前核销。** 公开层在启动待核对时发送 `op=revoke`。只有登记的原生执行可用 `executionId` 与 `closeOperationId` 取得同次证据。关闭方在同目录原子改名取走一次性描述，用同号重试可恢复核销。证据包含 `category=not_started`、`dispatch`、两个请求 ID、`launchId` 和 `cancellationEvidence`。调用方按同一 state 下的 `launches/<launchId>.json` 或 `.revoked` 核对类别和全部标识。另一操作号、只有普通 `already_absent` 或启动描述可能已被宿主消费时，无法取得该证据。
- **启动前核销后的会话。** 后端在提交 tmux 创建命令前持久记录阶段、原 socket 和已知原服务。tmux 创建命令队列独立记录本次实际创建的服务、会话和监督窗格，以便创建方中断或会话改名后重新核对。仅核销在可证实创建命令未提交时返回 `completed / already_absent` 并释放登记。可能已提交时返回 `uncertain`，可同时带已核验的 worker 核销事实。登记在会话仍存在或无法核实时保留。普通 close 可按原操作号另外核对并清理同次会话，要求核验原服务、身份一致且只有一个归属独占的原始已退出监督窗格。成功时返回 `completed / unstarted_session_closed`。会话已核实缺席时返回 `completed / already_absent`。重试可重放已确认的结果。已执行清理但丢失确认时，同号保留 `uncertain` 并继续返回原 worker 启动事实。普通 inspect、close 和创建预检共用原执行、启动、服务和回执检查。旧版类别、空标记或 R7 仅凭 worker 核销归档的记录缺少可核验的会话证据，旧回执中身份匹配的 worker 事实仍可重放。
- **输入未送达。** 已登记执行的 submit 或 interrupt 无法启动首个只读 tmux 客户端时，返回 `definitely-not-sent / input_not_delivered`，并带 `inspectionResult=uncertain`。当前请求没有发送按键，原 worker 的生存状态仍需核实。文字已经送达后丢失响应继续按未决输入处理。

创建前，后端在 state 下为该 dispatch 预留启动编号。宿主必须先成功取走一次性启动文件，再登记服务的 Linux 启动标识、PID 命名空间、进程启动时间、会话和 pane 编号，然后启动 worker。创建失败或明确请求核销时，如果后端先取回该文件，会记录 `not_started`。待同次会话已关闭或核实缺席后，可以用同一 dispatch 重试。若写入启动回执失败，会尝试留下独立的无凭据撤销标记，并在创建结果的 `cancellationEvidence` 中返回路径。两种证据均无法落盘时，后续核验保留未决。若文件已被取走、服务身份及回执均缺失，则保留 `uncertain`。已登记的服务失联后，恢复原服务可重新核验；确认原服务已退出，或在原服务确认原会话和原 pane 均已消失后，才能按缺席处理。核验使用 Linux procfs 和可写的 state。

resume 创建成功后，`launchResult` 为 `accepted`，`receipt` 指向启动回执。需要投递或检查 brief 时，`inputResult` 单独记录 brief 的结果。例如，OpenCode 已启动、读取提示框时失联，整次 resume 返回 `uncertain`，brief 为 `definitely-not-sent`。调用方可对账已启动的执行，避免重走启动流程。

请求格式错误或选择了其他后端时，进程非零退出。有效操作从 JSON 的 `result` 取结果。`accepted` 表示后端确认操作，`completed` 表示完成关闭，`definitely-not-sent` 表示确认未送，`refused` 表示归属或宿主拒绝，`uncertain` 需检查现场。文字写入后丢失 Enter 回执，归入 `input_uncertain`，调用方须保留未决输入，避免重放。宿主实际产生几条正式消息、当前 attempt 是否完成，由调度层查宿主记录和业务回执。

真实后端测试使用新建的临时 socket、state 和受控终端程序。先构建 Rust 的 wfc，再运行 `python3 tools/check_tmux.py target/debug/wfc -v`。设置 `WFC_TEST_R1_BINARY` 为 R1 固定件的路径可同时运行真实跨版本回归。
