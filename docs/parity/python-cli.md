# Python wfc 的 CLI 与状态兼容合同

本表以 Linux 上的 Python 单文件入口为基准。#39 和 #40 的公开解析器、数据库结构相同。#40 把 tmux 原有的 0.5 秒输入间隔提为常量，并在 bundled 向 Codex 粘贴后、发送 Enter 前也等待该间隔。两版均自报 `0.4.7-rc.4`，对照请固定提交。表中的票号均属 agent-router。[#39 源码][p39] · [#40 源码][p40] · [两版差异][delta]

## 调用约定

共有 23 个公开顶层命令词，含四个 owner 别名。Python 包装器的无参帮助只列部分命令，逐项以解析器为准。表内尖括号为位置参数，加方括号的选项可省。未写默认值的可选字符串参数默认为 None，开关缺省关闭。下文“O”代表 `[--owner-token TOKEN]`。除明确的平台限定外，CLI 和逻辑状态属于通用合同。[入口][entry] · [完整解析器][parser]

| 入口 | stdout、stderr 与退出码 | Rust 基线和验收 |
| --- | --- | --- |
| 无参、`-h`、`--help`、各子命令的 `-h/--help` | 顶层包装器打印简要说明并退出 0。子命令给出具体用法。首参为顶层帮助或版本时 Python 忽略后续参数。其他命令缺参数、未知选项、缩写如 `relay --deta`、互斥冲突、类型/枚举非法都由 argparse 输出到 stderr，退出 2。 | #2 Rust 顶层已有帮助。额外参数退出 2，各业务子命令也尚无独立帮助。#21 明列这些差异。 |
| `-V`、`--version` | 单行 `wfc 0.4.7-rc.4`，退出 0。 | #2 Rust 有自己的版本、目标、配置和构建身份，这是该票要求的差异。 |
| 内部启动 | Python 的 `_host-run`、`_bundled-runtime`、`_bundled-guard` 直接分派内置进程，`_herdr-call` 用有界 stdin/stdout JSON 执行 RPC。开发用 `build.py` 打包。旧 `notify_controller.py` 在 README 中标为历史参考。 | #2 Rust 的 `--kernel` 和兼容文件名无参握手已实现。宿主监督、guard、Herdr RPC 的对等行为分别归启动/后端票，私有 Python 命令拼写不构成用户 CLI。 |
| 结果编码 | 业务命令通常向 stdout 输出一个 JSON 对象，正常返回 0。`SystemExit(文本或 JSON 字符串)` 的业务错误在 stderr，退出 1。部分后端不确定/拒绝结果是 **退出 0 的 JSON**，调用方还要看 `result`。`wait` 超时退出 3。 | #4–#21 对实际进程分别捕获两个输出流和 code，校验字段类型、可选键及状态变更。 |

Rust 基线 `2e7a6963` 的分派表只保留 dispatch、done、ask、reply、continue、nudge、resume、status、reclaim、wait、relay、ack 这 12 个词，本轮实现均返回 2。其他业务词走 unexpected arguments，也返回 2。下表的工单是后续归属。Rust 当前通过帮助/版本/内核入口测试，并不能据此给下列业务命令记为通过。[Rust main][rust] · [Rust 测试][rusttests] · [Python 单文件测试][buildtest]

## 参数、返回与失败路径

表中的“回执”都有 `message`（整数）、`messageId`（稳定标识）、`state`、`statusVersion` 和 `duplicate`。有人工核实记录时还含 `resolution`。时间、路径与生成的身份值按类型和关联关系比对。源中的测试类名可在固定版本直接检索。[回执结构][receipt] · [测试类索引][tests]

| 命令与参数 | 成功结果和边界。建议复验的错误或竞态 | Rust 归属 / Python 证据 |
| --- | --- | --- |
| `dispatch --host H --task PATH --cwd DIR --report PATH [--backend B] [--herdr-endpoint S] [--launch-config PATH] [--scope S] [--role R] [--model M] [--effort E] [--label L] [--git] [--network]` O | H 为 codex/opencode/claude，B 为 tmux（默认）/herdr/bundled。任务参数是**文件路径**。报告父目录自动创建，worker 收到绝对路径。省略 scope 生成 `s-…`，role 从报告目录尾段派生，兜底 worker。label 默认从项目与报告目录生成，清除控制字符，最多 120 字符。返回 dispatch、scope、role、round=1、attempt、host、backend、label、tmux，条件字段 trustedBy、ignored。Codex 的 git/network 开关在其余两宿主被说明为忽略。无 tmux 的确定失败可不留派发。不确定的外部创建留下可查诊断和尝试。 | #4、#8、#13、#19。Claude/后端见缺口。Delivery、Labels、GitOption、Races。[处理器][dispatch] |
| `done --report PATH [--attempt A] [--callback-ref R] [--message-id K]` O | 要求 `WFC_DISPATCH` 指向已知任务和已存在的当前报告。返回回执，消息体带 report、scope、role、round、attempt、head、dirty。Git 缺失、非工作树、任一 Git 查询超过 5 秒时仍可完成，head/dirty 为 null。带 attempt 的默认键为 `done:<dispatch>:<attempt>`。用原键重试读回原快照，工作树变更也不会改写。无键且无 attempt 的老调用没有上述幂等保证。拒绝旧报告、错误 attempt 或错误回调。 | #6、#15、#17。MessageRoundTrip、DoneHead、RoundIdentity。[处理器][done] |
| `ask <question> [--options CSV] [--attempt A] [--callback-ref R] [--message-id K]` O | 按逗号切分选项并去掉空项，缺省 []。首次回执带 next。幂等命中只返回回执。带 attempt 且未指定键时，基于 dispatch、attempt、问题与选项生成稳定键。相同键变更内容退出 1。无 attempt 限首轮且此报告只有一条尝试。Herdr/bundled 正在 resume/handoff 时可记下准确新 attempt 提前发来的问题，未完成创建前回复仍受限制。 | #11、#15、#17、#18。MessageRoundTrip、RoundIdentity。[处理器][ask] |
| `wait [--timeout SEC] [--stall-after SEC] [--scope S]` O | 两个浮点秒数默认 3600/300。按消息 id 返回该 scope 的 `{messages:[…]}`，含 id、messageId、dispatch、type、state、statusVersion 和正文，先标 received 再输出。空且到期返回 `{"messages":[],"timedOut":true}`、退出 3。领过但未 ack 的消息可在 `status --dispatch` 找回，第二个 wait 不会再领取它。已有受管 scope 时，不带 scope 会被拒绝。 | #6、#7、#14、#16。#7 的“重取”应独立验证从状态读回与重复投递的区别。Waiting、Watchdog。[领取][take] · [等待][wait] |
| `ack <message> [--body TEXT]` O | 接受数字 id 或稳定 messageId，且只接受发往总控的 received/processed。首次收到后转 processed 并增加版本，重复返回相同版本的 duplicate。若有 --body，以其 UTF-8 字节的 SHA-256 匹配存储的规范正文。尚未收到、正文不符或编号不存在时退出 1。 | #7、#16。MessageRoundTrip。[处理器][ack] |
| `resolve <message> --actor NAME --reason WHY --evidence REF --body-hash SHA256 [--source SRC]` O | source 默认 operator。面向外部已确认的 unknown 投递。核对正文哈希，附 actor/source/reason/evidence/at，将消息转 received 并留审计，然后才能 ack。已核实/处理的同证据请求返回 duplicate。状态、哈希、证据冲突退出 1。不会向目标重发。 | #7、#18。须补该显式命令。MessageRoundTrip、OwnerLifecycleFences。[处理器][resolve] |
| `reply <message:int> <answer>` O | 仅数字问题编号。换行折成空格，回 `{replied,dispatch}`。两个并发回复只提交一次。确定未发送/被拒时释放本次占用。结果未知保留占用和诊断。已回答、旧尝试或锁后失权退出 1。 | #12、#16。Replies、PartialTyping、DroppedEnter、OwnerLifecycleFences。[处理器][reply] |
| `continue <dispatch> --task PATH --report PATH` O | 完成当前任务且会话存活时，原会话接下一轮，round 加一并产生新 attempt。报告在原父目录内且未用过。回 `{continued,message}`。确定未发且无人推进时撤销本次新轮，不确定则留痕。拒绝未完成任务、已死会话、跨目录/复用报告。 | #12/#15 共用输入与轮次。需覆盖 continue。ContinueDispatch、ContinueBeforeDone、ReportReuse、ParallelState。[处理器][continue] |
| `nudge <dispatch> --text TEXT` O | 存活、未完成且当前尝试可写时给予一行提示，回 `{nudged,message}`。与另一输入串行。已结束/完成、待对账、失权均不能再写。不确定的输入在 status 中可查。 | #12/#14/#18 共用。需覆盖 nudge。Nudge、ReadOnlySessions、OwnerLifecycleFences。[处理器][nudge] |
| `resume <dispatch> [--task PATH --report PATH]` O | 会话退出后按保存/查证的宿主会话身份重开。未完成时两项均省略，同轮新 attempt。完成后两项同时给且报告未复用，新一轮。回 resumed、operation、result、session、tmux、round、attempt。存活、找不到会话、报告目录不符、另一个进程已推进、旧派发表没有 report_dir 均有诊断。 | #15、#18、#19。Resume、ResumePartialTyping、RoundIdentity、InterruptHandoff。[处理器][resume] |
| `interrupt <dispatch>` O | 先提交审计并将旧尝试置 unknown、禁止继续写，再请求后端中断。回 dispatch、operation、result、round、attempt、可选 diagnostic。即使按键已送达，旧尝试也不能报新结果。未决生命周期操作返回含 audit 的拒绝。 | #15/#18 共用，需单测 interrupt。InterruptHandoff。[处理器][interrupt] |
| `close <dispatch>` O | 关闭经验证属于本任务的资源。返回 dispatch、operation、result、round、attempt、可选 diagnostic。结束资源与任务结果分开：保留 completed/failed，未完成已关闭转 cancelled。已消失可返回 completed，检查不明可返回 JSON，拒绝外部会话和未决操作。 | #5、#18。InterruptHandoff。[处理器][close] |
| `reclaim [--dispatch ID]… [--yes]` O | 默认只读列出已完成、未回收且存活的会话。点名可含未完成任务，重复名去重，未知名在关闭任何会话前失败。--yes 执行并逐项报告。返回 state、yes、sessions，项含 dispatch、label、tmux、state、reclaimable、result，视情况含 note/error/message。默认列表与执行间发生续派时重新检查。已消失为 gone。存在收集到的执行错误仍输出各项，stderr 诊断、退出 1。账本、工作树、会话历史保留。 | #5/#18 的关闭基础另需覆盖批量预览和竞态。Reclaim。[处理器][reclaim] |
| `handoff <dispatch> --host H [--task PATH --report PATH] [--model M]` O | H 同 dispatch。未完成任务保持 round。已完成需要一对新路径。更换宿主/attempt，处理原写者，回 dispatch、operation、result、fromAttempt、attempt、host、round。同请求的已接受情况可回 duplicate、audit。关闭或创建不确定可回 diagnostic 和非成功 result，拒绝外来会话/失权/报告冲突。待对账前不可隐式再发。 | #15/#18 共用。跨宿主 handoff 与 #17 的总控换人各自验收。InterruptHandoff。[处理器][handoff] |
| `reconcile <dispatch> <audit:int> --result X --actor NAME --evidence REF [--cleanup-receipt]` O | X 为 accepted、definitely-not-sent、unknown、refused。为生命周期审计写入裁决，回 dispatch、operation、result、audit、duplicate，首次带 attempt。对当前 bundled create 的 `--cleanup-receipt` 要求 X=unknown、可证实清理和匹配身份，额外回 cleanupReceipt、backendAlive=false、inputReplayed=false。同证据幂等。过期 attempt、证据冲突或 cleanup 条件不符退出 1。 | #18、#16。InterruptHandoff、OwnerLifecycleFences、BundledNative。[处理器][reconcile] |
| `herdr-reconcile <dispatch> --pane P --terminal T --actor NAME --evidence REF [--workspace W] [--old-writer-evidence REF]` O | 为 Herdr 重新绑定观察到的运行单元，缺省 workspace 沿用原 handle。须有匹配 ownership nonce，服务变更时须有原写者证据，发现部分进程身份则拒绝。回 dispatch、result、audit、attempt、inputReplayed=false。缺身份/非 Herdr/空 actor 或 evidence 退出 1。 | Herdr 缺口 + #18。HerdrContract、HerdrCleanup。[处理器][herdr] |
| `status [--dispatch ID ∣ --active]` | 两选项互斥。回 state（绝对目录）、dispatches、undelivered、waiters。--dispatch 加全量 messages，--active 对已完成但存活的另列 reclaimable，老库加 schema 说明。空库为空数组，点名不存在单独退出 1。无 scope/token 参数。详情见下表。 | #4、#7、#14、#18。旧数据缺口。Status、MultiSignalContract、WaiterLiveness。[处理器][status] |
| `relay [--codex ∣ --codex-thread UUID ∣ --opencode-url URL --opencode-session ID] [--scope S] [--stall-after SEC] [--for SEC] [--detach]` O | 实际运行时必须选择一种目标。Codex 自动目标取 CODEX_THREAD_ID，通过顶层 rollout 查证，拒绝子代理、错 UUID 和环境不匹配。OpenCode 两项必须成对并使用 HTTP(S)。stall 默认 300 秒。--for 省略持续运行（Python 的 0 也视作未设置）。--detach 回 pid、log 到独立进程。前台到期退出 0。缺少 rollout 的确定失败退出 1、消息恢复 unsent。目标可能已接收时保持 unknown，人工 resolve。 | #9、#16、#18，补 OpenCode 控制端。ScopedDetachedCodexRelay、CodexRelayThread、OpenCodeRelay。[目标与投递][relay] |
| `owner <acquire∣claim∣renew∣takeover> --scope S [--owner ID] [--token T] [--lease SEC]` O。四个动作也可直接作顶层命令，参数相同 | --owner 默认 WFC_CONTROLLER_ID 或 controller，lease 默认 60 秒。acquire/claim 建立租约，未过期返回 scope_owned。takeover 即刻换凭证、增 generation，不可复用旧 token。renew 校验当前 token 和租约有效期，延长租约且 generation 与数据库中的 owner 不变（响应中的 owner 取本次参数）。回 scope、owner、token、generation、leaseUntil、action，claim 显示 acquire。错误含 stale_controller_owner 或 owner_token_conflict。 | #16、#17。ScopedDetachedCodexRelay、OwnerLifecycleFences。[租约逻辑][owner] · [别名][parser] |

## 老调用、状态目录和迁移

| 边界 | Python 可观察行为与 Rust 验收样例 | 现有票/缺口 |
| --- | --- | --- |
| 状态发现与只读 | `WFC_STATE` 展开为绝对路径，Linux 缺省 `~/.local/state/workflow-core`。Windows 缺省 LOCALAPPDATA/workflow-core。无库跑 status、status --active 或 reclaim 预览，不创建目录/库。对旧库以只读模式连接，用临时空表/视图补足缺表缺列。能读多少就展示多少，新增列可为 null，不凭空制造身份。取副本，比对查询前后文件 SHA、sqlite_master 和记录。 | #4 状态入口，需旧库专项。[读写路径][db] · [跨平台目录][platform] |
| 首次写入与旧消息 | 写指令（含空 wait）创建/升级 router.db，加列在写锁下重新检查。历史消息保留原 numeric id、原始正文。缺少 message_uid 则分配永久 m-…，缺 body_hash 按存储正文原字节 SHA-256 回填。保留 processed/unknown，历史 delivered 位映射 received/unsent，已填但矛盾的 delivered/state 修复并增加 status_version。并发首写及中途退出后重跑应稳定。 | #6/#7 提供回执语义，需旧库专项。ParallelState、Status、MessageRoundTrip。[写入与修复][db] |
| 旧 dispatch/轮次 | 空 backend→tmux、scope→生成 s-…、role→报告目录尾段或 worker、current_round→1、current_attempt→生成 a-…。缺历史时建立首个快照。按 dispatch 和 report 匹配旧 done 回填 completed、原 result 正文和时间。预先存在的身份、历史任务/报告不可因重启更换。缺必要报告或宿主会话资料时 resume 给出缺项或新派发提示。 | #4、#15、旧库专项。RoundIdentity、Status、ParallelState。[回填][db] · [恢复][resume] |
| 老输入权限及回调 | 未注册 owner 的 scope 保持旧总控入口。一旦有受管 scope，无 scope 的 wait/relay 会被拒。凭证优先级为 --owner-token、WFC_CONTROLLER_TOKEN、WFC_OWNER_TOKEN。新 worker 在 WFC_DISPATCH 对应当前尝试上以 WFC_WORKER_CALLBACK 回调。带此环境调用除 ask/done 外的解析器业务命令返回 worker_callback_only。原会话续轮用 --callback-ref 和已有回调秘密证明新轮。旧宿主未得到独立凭证时走 owner/无主兼容路径。公开 attempt/ref 单独不能充当秘密。同一报告有多个尝试时，无 attempt 的新 done 被拒，当轮 ask 还有首轮限制。 | #6、#11、#15–#17。旧库专项要固定无主/有主分别测试。[凭证绑定][cb] · [入口限制][parser] |
| 两套任务状态 | status 的旧 `state` 包括 working、asking、exited、done、cancelled、unknown、reclaimed。另给 `status` 和 `lifecycle`（online 对应 running），canonicalStates 列出 running、online、waiting-human、blocked、unknown、completed、failed、cancelled、reclaimed。signals 并列给 processOnline/online、hostActive、activity、waitingHuman、blocked、idle、fault。后端 inspect 无法确认时，结果保守处理。保留后台存活但任务完成、等待人且在线、已退出但已完成等组合。 | #4、#13、#14、#18。MultiSignalContract、Status、InterruptHandoff。[判定及详情][status] |
| 审计、历史与进程 | status 详情保留 rounds 的 task/report/state/result/created/completed，audit 的操作、尝试、结果与裁决证据，launch、backendResult/backendAlive/backendHandle、旧 tmux/tmuxAlive、最后消息、路径、可选 controllerOwner（不暴露 token）。status --dispatch 的消息视图有 `to/at`、可选 delivered/resolution 和正文。waiter 用 PID 加创建身份过滤复用。Linux 的 boot id 与 /proc starttime 精确比对，老 lstart 容许 ±10 秒，仅 PID 的老行只能看进程是否存在。Windows 另有 liveness/diagnostic，POSIX 其他平台用 lstart。 | #4、#5、#7、#14、#18、#25。Status、WaiterLiveness。[详情][status] · [身份][pid] |
| 兼容范围核验 | 从旧表样本和 #39/#40 创建的库复制到独立目录，比对升级前后的回执身份、历史与只读性。启动端、旧 worker 实际回调程序路径、未确认消息、backend handle/外部运行时均须独立验证。SQL 字段相似只能说明静态匹配。活跃 Python 宿主/relay 接管、Python 与 Rust 同库并发写、由 Rust 写回后的降级可读性在本基线均无 Rust 证据。若候选只支持静止数据库迁移，应列明停写条件、在途条目处理与保留原副本方式，按 #21 记录真实支持范围。 | 旧库专项 + #17/#18/#21/#24。[旧库测试][tests] · [Rust 分派][rust] |

### 环境与可用条件

| 输入 | 默认值、适用端与复验位置 |
| --- | --- |
| 状态及认证 | WFC_STATE 在各端选择状态目录。WFC_DISPATCH 和 WFC_WORKER_CALLBACK 由启动过程注入，WFC_CONTROLLER_ID、WFC_CONTROLLER_TOKEN、WFC_OWNER_TOKEN 参与总控身份选择。权限判断按上表分别在无主、已占用和已过期 scope 验证。[状态目录][db] · [身份][cb] |
| 会话和配置位置 | WFC_LAUNCH_CONFIG 选择 JSON 文件，可由 dispatch --launch-config 覆盖。Codex 从 CODEX_HOME（默认 ~/.codex）下的 sessions 找 rollout，CODEX_THREAD_ID 定位控制端会话。OpenCode 优先 WFC_OPENCODE_DB，否则 XDG_DATA_HOME（默认 ~/.local/share）下的 opencode/opencode.db。Claude 的会话来自 CLAUDE_CONFIG_DIR（默认 ~/.claude）下的 projects，信任设置可用 WFC_CLAUDE_CONFIG 指定，默认 ~/.claude.json。[路径查找][hostpaths] · [派发][dispatch] |
| 观察和等待 | WFC_TYPED_CHECK_SECONDS 默认 1.5、WFC_RESUME_CHECK_SECONDS 默认 5、WFC_ACTIVITY_AFTER 默认 300、WFC_MAX_TURN_SECONDS 默认 3600（单位均为秒）。这些配置由 seconds_setting 读取，负数钳到 0，不能转成浮点时回默认。WFC_QUEUE_TIMEOUT 默认 60 秒用于外部 Codex/OpenCode 投递。逐项以相同设置比较两端，用不同于默认的值验证实际生效。[输入与恢复][typed] · [状态][status] · [watchdog][watch] · [relay][relay] |
| 后端依赖 | WFC_HERDR_ENDPOINT 可由 --herdr-endpoint 覆盖。WFC_HERDR_TIMEOUT 默认 8 秒，要求大于 0 且不超过 60。WFC_BUNDLED_KERNEL 指向可执行伴随内核，缺省查找同目录 wfc-kernel。Linux bundled 需要 WFC_BUNDLED_CGROUP_ROOT 指向包含 cgroup.kill 的已委派 cgroup v2 子树，WFC_BUNDLED_DIAGNOSTICS=1 启用诊断。条件不齐时核对拒绝/诊断，真实成功另记环境。[Herdr 配置][herrpc] · [bundled 选择][bundle] · [cgroup 校验][cg] |

### Linux 复验入口

在 #40 临时检出目录执行。使用固定 Python 测试套件内的隔离目录、替身 tmux 与宿主，实际运行 SQLite、文件系统与 CLI 子进程。以下七项已在本轮通过。跨进程真实宿主、活跃旧会话接管和 Rust 迁移仍须在对应候选上验收。

```sh
env -u WFC_WORKER_CALLBACK -u WFC_CONTROLLER_TOKEN -u WFC_OWNER_TOKEN -u WFC_DISPATCH \
  PYTHONDONTWRITEBYTECODE=1 PYTHONPATH=.:tests python3 -m unittest \
  test_router.Status.test_status_without_a_state_database_is_empty_and_creates_nothing \
  test_router.Status.test_status_reads_an_old_state_database_without_upgrading_it \
  test_router.Status.test_a_write_command_adds_the_process_start_to_an_older_waiter_table \
  test_router.ParallelState.test_parallel_first_use_of_an_old_state_database_upgrades_it_once \
  test_router.MessageRoundTrip.test_attempt_aware_legacy_shape_gets_a_stable_default_receipt_key \
  test_router.MessageRoundTrip.test_unknown_receipt_has_explicit_resolve_path \
  test_router.Waiting.test_wait_timeout_is_reported_with_a_nonzero_exit_code
```

[p39]: https://github.com/xmxxc/workflow-core/tree/2409c40b469e0e40bd5c55326df3d5ebb86ca81b
[p40]: https://github.com/xmxxc/workflow-core/tree/60d7d76e4bb861ac9e0bbb043aa1a36946578397
[delta]: https://github.com/xmxxc/workflow-core/compare/2409c40b469e0e40bd5c55326df3d5ebb86ca81b...60d7d76e4bb861ac9e0bbb043aa1a36946578397
[entry]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/wfc.py
[parser]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3568-L3745
[buildtest]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_wfc.py
[tests]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py
[platform]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/wfc_platform.py#L12-L17
[db]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L40-L222
[cb]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L225-L319
[owner]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L321-L371
[dispatch]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L773-L878
[receipt]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L1036-L1045
[done]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L1048-L1158
[ask]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L1161-L1215
[ack]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L1218-L1243
[resolve]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L1246-L1284
[reply]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L1840-L1893
[nudge]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L1973-L2022
[resume]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2724-L2870
[interrupt]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2041-L2119
[close]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2122-L2194
[reconcile]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2197-L2282
[handoff]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2285-L2401
[status]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2408-L2622
[pid]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2625-L2703
[reclaim]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2884-L3003
[take]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3006-L3028
[wait]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3306-L3322
[relay]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3325-L3497
[herdr]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3500-L3554
[continue]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L1897-L1969
[rust]: https://github.com/xmxxc/agent-router/blob/2e7a6963c53b35834309b8dd553d7edad879d955/src/main.rs
[rusttests]: https://github.com/xmxxc/agent-router/blob/2e7a6963c53b35834309b8dd553d7edad879d955/tests/cli.rs
[hostpaths]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L513-L542
[typed]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L1346-L1699
[watch]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3146-L3257
[herrpc]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/herdr_backend.py#L167-L184
[bundle]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/bundled_backend.py#L19-L151
[cg]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/bundled_runtime.py#L202-L230
