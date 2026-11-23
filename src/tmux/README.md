# Linux tmux 后端接口

调度层可调用 `tmux::execute(state, request)`，也可启动一次 `wfc --tmux-backend`，将一个 UTF-8 JSON 对象写入标准输入后关闭输入，读取标准输出的一个 JSON 对象。后者用于跨进程集成和独立复验。每个请求带 `dispatch` 和 `op`。Linux 的新请求与旧记录中缺省、null、空字符串的 `backend` 都选 tmux，也接受显式的 `"tmux"`。其他后端由上层路由到对应实现。

| op | 输入和返回证据 |
| --- | --- |
| create | 必填 `cwd`、非空 `argv` 字符串数组，可选窗口名 `label` 和字符串映射 `env`。以 detached 方式启动 `wfc-<dispatch>`。启动结果引用私有 `receipt` 文件，记录宿主已启动、启动失败或已退出。 |
| inspect | 返回 `alive`、`owned`。已确认归属时还返回同一 wfc 程序的 `attach` 参数数组，可在交互终端直接执行，执行时由 `--tmux-attach` 复核原服务的进程身份并固定 socket，再由原生 tmux 核对会话、pane、启动标记、窗口独占性和会话级 WFC_STATE，成功后可用 tmux 的 detach 键离开。 |
| read | 返回当前 pane 的 `screen`。 |
| submit | 必填 `text`，可选 `host`。在登记的 worker pane 上检查画面并按字面发送一次文字。每次按键都复核目标 pane 的会话归属和窗口独占性。Codex 有稳定匹配草稿时，最多补两次 Enter。 |
| interrupt | 向登记且仍归该会话独占的 worker pane 发送 C-c。 |
| resume | 接收与 create 相同的启动数据。调用方提供恢复原宿主会话的 argv。OpenCode 另取 `text`，在画面稳定后提交。Codex 可传 `probe_read_only: true` 检查恢复后的只读提示。 |
| close | 关闭前核对会话归属和全部窗口独占性。原 worker pane 应在其中，或同次启动已有进程退出回执且原 pane 已从整个服务消失。原会话消失后，已登记的 pane 仍在相邻会话中时保留未决。已确认关闭时返回 completed。 |

例如，已创建会话后可以在另一个进程读屏。

```sh
printf '%s\n' '{"op":"read","dispatch":"d-abc12345"}' | WFC_STATE=/path/to/state wfc --tmux-backend
```

用同一 `WFC_STATE` 与同一 tmux 服务访问会话。独立服务可用 `WFC_TMUX_SOCKET` 指定绝对 socket 路径，缺省沿用 tmux 的服务选择。调度层负责持久化服务选择、串行化同一 attempt 的业务操作，并向当前 worker 提供专用的 `env.WFC_WORKER_CALLBACK`。

公开 CLI 首次派发时省略 `legacyExecutionId`。接管已有旧执行时，传入旧记录中该次实际启动的稳定唯一标识（如 attempt ID），以及原 state、dispatch 和服务选择。同一旧执行的重试使用相同标识；同一 dispatch 被旧客户端再次启动时使用新标识。即使首次操作是 create，也要传入旧执行信息。首次核验前会保存未决记录，服务失联、同名会话缺席、切换到另一显式 socket 或归属不符时返回 `uncertain`。后续省略该字段无法清除未决记录。关闭证明只适用于已核销的那次启动。若已有登记属于另一次执行，须先在原服务上证实前次执行已结束，再核验新传入的旧记录。R2 已登记的执行可直接沿用登记；如需给它传入标识，使用原登记中的启动编号或接管标记。对未提供此字段且无登记的兼容请求，create 可以在新服务上启动；其他操作首次发现服务不可达时会保存未决记录。

旧服务恢复后，以完整会话名和会话级 WFC_STATE 确认归属。旧执行记录如有原 worker 的 tmux pane ID（如 `%3`），同时传 `legacyWorkerPane`。后端保存该提示并在原会话中核验。缺少提示时，仅在该会话只含一个 pane 时接管；多个 pane 的旧会话保留未决，直到调用方提供可信的原 pane ID。调用方须定位原服务；升级前仅有名称和 state 的记录无法区分另一台逐项复制旧会话的服务。

创建前，后端在 state 下为该 dispatch 预留启动编号。宿主必须先成功取走一次性启动文件，再登记服务的 Linux 启动标识、PID 命名空间、进程启动时间、会话和 pane 编号，然后启动 worker。创建失败或明确请求 close 时，如果后端先取回该文件，会记录 `not_started`，可以用同一 dispatch 重试。若写入启动回执失败，会尝试留下独立的无凭据撤销标记，并在创建结果的 `cancellationEvidence` 中返回路径。两种证据均无法落盘时，后续核验保留未决。若文件已被取走、服务身份及回执均缺失，则保留 `uncertain`。已登记的服务失联后，恢复原服务可重新核验；确认原服务已退出，或在原服务确认原会话和原 pane 均已消失后，才能按缺席处理。核验使用 Linux procfs 和可写的 state。

resume 创建成功后，`launchResult` 为 `accepted`，`receipt` 指向启动回执。需要投递或检查 brief 时，`inputResult` 单独记录 brief 的结果。例如，OpenCode 已启动、读取提示框时失联，整次 resume 返回 `uncertain`，brief 为 `definitely-not-sent`。调用方可对账已启动的执行，避免重走启动流程。

请求格式错误或选择了其他后端时，进程非零退出。有效操作从 JSON 的 `result` 取结果。`accepted` 表示后端确认操作，`completed` 表示完成关闭，`definitely-not-sent` 表示确认未送，`refused` 表示归属或宿主拒绝，`uncertain` 需检查现场。文字写入后丢失 Enter 回执，归入 `input_uncertain`，调用方须保留未决输入，避免重放。宿主实际产生几条正式消息、当前 attempt 是否完成，由调度层查宿主记录和业务回执。

真实后端测试使用新建的临时 socket、state 和受控终端程序。先构建 Rust 的 wfc，再运行 `python3 tools/check_tmux.py target/debug/wfc -v`。
