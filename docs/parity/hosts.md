# Linux 宿主、后端与唤醒对照

宿主（Codex、OpenCode、Claude Code）理解任务，后端（tmux、Herdr、bundled）提供终端与进程，relay 把结果送回总控。三条链路各有自己的成功信号。后端接受按键只说明输入已交给终端，验收单次回复要数宿主正式用户消息，完成要查当前 attempt 的 done，处理完成还要查 processed 回执。[Python 路由][router] · [Herdr 输入合同][herdr-doc] · [bundled 合同][bundled-doc]

基线固定为 Python #39 `2409c40b`、#40 `60d7d76e` 和 Rust `2e7a6963`。下表的“待验”表示该 Rust 提交缺相应公开路径。它已有帮助、版本、兼容内核入口与 PTY 协议，可以创建、读屏、输入、调整大小和关闭终端。Rust 的 `tools/check_wfc.py` 用**另一固定版本**的 Python 路由驱动 Rust 内核做六项测试；测试条件包括 Linux 委派 cgroup。这些证据计入内核层。完整 Rust 路由还须在同一下载件上运行。[Rust 入口][rust-cli] · [Rust 内核][kernel] · [跨仓内核测试][check-wfc]

## 宿主及会话

| 项目 / 平台 | #40 对外行为、条件与异常 | Rust 归属及复验 |
| --- | --- | --- |
| 启动配置 / 通用 | 显式 `--launch-config` 优先于 `WFC_LAUNCH_CONFIG`。保存绝对路径，resume/handoff 重读选定文件；没有记录的旧任务继续继承调用者。按调用环境、配置公共 env、宿主 env、适配器和路由自有值覆盖。运行时前缀、命令数组、空格/Unicode、相对 worker cwd 的 PATH、Unix shebang 与显式解释器均按 argv 传递。剥离 owner/controller 凭据，worker 得到当前分派的私有回调。陈旧宿主 shell 的 brief 显式指定 state/dispatch。 | #8/#13 扩展至三宿主；以带空格路径、旧 tmux PATH、坏配置、丢 runtime、无执行权限、退出 23 与未确认启动验收。status 保留 `launch`、类别、退出码及有界去敏诊断；确知未起与可能已起采取不同恢复。检查私有一次性 spec、无原始环境或凭据泄漏，包含截断的多字节凭据。待验。[配置合同][launch-doc] · [宿主启动测试][launch-tests] |
| Codex worker / 通用 | 需要可启动的 CLI、可用模型/认证、合适的 TERM 和工作区设置。新会话传 brief、`--no-alt-screen`、workspace-write、never、目录、模型/effort。按 `--network` 放行网络；`--git` 加 worktree 自身和 common git dir。恢复用 `codex resume` 加相同 worker 会话。按 `CODEX_HOME/sessions` 下 rollout 首次用户 brief 定位，跳过引用和 subagent；`task_started` 清旧错误，`task_complete.error` 给 watchdog。只读提示或待确认输入阻止盲目二次输入。 | #8/#12/#14/#15。用真实 CLI 留版本、日志、repo/工作目录、是否触发登录或只读，核对初始任务、一次 reply 的正式 user turn、done 与同会话恢复的记忆。三种终端的补 Enter 条件见下。旧 Python Linux tmux 真实 Codex 留有记录；Rust 待验。[宿主构造与查找][hosts] · [已记录的 Python 探针][launch-probe] |
| OpenCode worker / 通用 | 新会话用 cwd、`--prompt`、`-m`、`--variant`，进程级 `OPENCODE_CONFIG_CONTENT` 为 external_directory 与 .env 读取设权限；`--git`/`--network` 请求回显于 `ignored`。恢复同会话用 `-s`，另投一次 brief。只读打开 `WFC_OPENCODE_DB` 或 XDG 默认数据库，按顶层 session 的 brief 匹配，兼容旧 session/part/message 和 v2 session_v2/session_message。v2 仅终止的 failed idle 对应本轮 assistant error 时报告错误；新用户输入、流式重试、成功恢复须清旧判断。 | #19/#12/#14/#15。固定宿主版、模型与许可；Herdr/bundled 对 OpenCode argv 预填先读两次稳定输入框，仅本次 attempt 的匹配后缀可补一次 Enter，空框/别的草稿不补。tmux 首派直接交 argv，特定版本是否自行提交须单验。使用两代只读库 fixture 和真实宿主各验，自动首派→问答→完成→退出后恢复。待验。[宿主与会话][hosts] · [OpenCode 适配][herdr-doc] · [公开 CLI 测试][router-tests] |
| Claude Code worker / 通用，信任写入限 Unix | 首次分配 UUID 并传 `--session-id`，恢复传 `--resume`，首参数为 brief；auto permission、model/effort、name 和 add-dir，git/network 出现在 ignored。Unix 仅读取已有 `WFC_CLAUDE_CONFIG` 或 ~/.claude.json 的已信任 cwd/祖先，后者可在锁下原子写入子目录信任，保留权限；无已信任祖先留给宿主提示。Windows 在宿主内确认。按 `CLAUDE_CONFIG_DIR/projects` 内 UUID 读 JSONL，assistant 的 isApiErrorMessage 通知、user 清除故障。 | #37 Claude 适配，复用 #8/#12/#14/#15 的公共能力。三类受控目录（已信任、可信父目录、未信任）、并发工作树与损坏配置；真实已认证 CLI 留 session UUID、一次 user 回复、续派/恢复、done 与故障类别。文档记有旧 Python 真实 resume；历史 Windows bundled 实验手册明确把 Claude 排除在该次验收以外。#40 的通用派发参数接受三宿主，不能从参数表推断九种组合均已实测。待验。[信任代码][trust] · [历史验收范围][manual] · [Python 支持边界][py-readme] |

宿主记录不可读、轮转或格式变化时，watchdog（观察进程和画面的轮询器）仍有终端观察路径。Codex/Claude 只读完整换行记录，截断或换 inode 后从头读。两次检查独立分辨启动失败、backend observation、worker 终止、只读、结构化宿主错误、回合超时及相同画面超时；归属、轮次、最新输入或 attempt 改变时过时样本应丢弃。并发 wait/relay 以及重启用同一故障指纹只报告一次，故障变更或新轮次可报告。#13/#14 的验收应含“同一轮既超额又超时”的持久化回归，以及另一 scope 收不到通知。[监视逻辑][watch] · [故障去重实测记录][watch-probe]

## 执行后端

| 后端 / 平台 / 条件 | 操作合同、关键结果和回收 | Rust 归属及复验 |
| --- | --- | --- |
| tmux / Linux 与 macOS | Python 默认以及缺 backend 的旧行取 tmux。需外置 tmux。创建 detached `wfc-<dispatch>` 会话，以 label 命名窗口，用 cwd、私有启动 spec 与 receipt 启动宿主。可 attach/detach。inspect 查询存在性/状态目录，read 采 pane，interrupt 送 C-c，close 仅 kill 所有者状态目录匹配的 session。submit 先读只读提示，送 literal 文本，等待 0.5 秒再 Enter。可辨认的 Codex 框两次观测仍留匹配文本才有限补键（tmux 可处理可辨滚动文本）；Claude/OpenCode 普通输入一次。tmux 根本未启动为确定未发，文本可能已送或 Enter 失败为 uncertain。 | #31 tmux 适配，复用 #4/#5/#12/#15/#18。用独立真 tmux server、临时 state 验 `dispatch→status→reply/continue→interrupt→resume→close/reclaim`，检查 window 名，拒绝另一 state 的同名会话，有 sibling 时仅本会话消失。用替身区分 PATH 缺失、启动未确认、只读、掉 Enter；核实失败 receipt 与无重复正式输入。待验。[实现][tmux] · [归属及回收][reclaim] · [测试][router-tests] |
| Herdr / Linux；原生 Windows pipe、macOS peer 查询为平台专项 | 显式选 Herdr，保留指定 endpoint；Unix 同用户独占权限 socket，Linux SO_PEERCRED，服务 PID+创建身份；固定 Herdr **0.9.5 / protocol 22**。每次 RPC 单独连接，先 ping，再 snapshot 核对 workspace nonce、pane/terminal 与服务身份。建 workspace、标 nonce、启动 pane；读屏带 revision/截断。blocked 可打断但拒输；Codex 合并发送输入和 Enter 后，两次完整、稳定、未滚动的同一草稿才补 Enter，最多两次。未知响应保留 claim。关闭目标 pane 后须观察旧 writer 停止，保留 sibling。 | #32 Herdr 协议与 `herdr-reconcile`，复用 #12/#15/#18。真 Herdr+受控 TUI 验服务重启、输入响应丢失、共享 pane、只读、问答/继续/打断与恢复。显式对账需当前 owner、actor/evidence、映射 pane/terminal；service 换身须旧 writer 证据，nonce 丢失或部分身份不可见应拒绝。同一已核验服务、匹配 nonce、观察到单元缺席且零写者时，无旧写者停止证据则拒绝；给出 --old-writer-evidence 后返回 accepted、inputReplayed=false，原 attempt 的 handle 保存 not-started、writerProcesses=[]，并记审计。归 #32/#18/#21。配置端口不可用/协议错另验，`WFC_HERDR_TIMEOUT` 默认 8 秒且范围 (0,60]。待验。[Herdr 实现][herdr] · [对账命令][herdr-reconcile] · [真实服务测试][herdr-tests] |
| bundled / Linux | 显式选择；Python 按 `WFC_BUNDLED_KERNEL` 或程序旁 `wfc-kernel` 找可执行文件。需可写委派 cgroup v2 且有 cgroup.kill，预检验证真实 mount；无委派应在创建 worker 前报错。每执行单元有 detached guardian、runtime、PTY kernel；其他 CLI 以私有快照核验 loopback 服务端挑战/能力令牌，绑定 attempt、unit、nonce、generation、进程与容器身份。发前可确定未送，部分发出、响应丢失/串号为 uncertain。预认证帧 256 B / 总时限 0.5 s，认证后 2 MiB / 2 s，客户端交换 6 s；持续来连接时仍检查生命周期。 | #33 接入 #5/#18，提供守护、重连与清理凭据；#20/#21 同下载件无 Python 复验。测试环境含真实内核、委派 cgroup、受控子孙进程。伪目录、失配 nonce/代际、错误握手、慢连接、输出拥塞、写入后掉响应分别验。核对状态和私有收据；能力令牌不可出现在公开 status / 可选诊断轨迹。Rust 当前仅内核可用，独立 CLI 间的运行时和守护待验。[适配器][bundled] · [运行时][runtime] · [协议测试][wire-tests] |
| bundled 输入与存活 / 通用机制，Linux 原生回收 | bracketed paste 后 Enter。#39→#40：bundled **Codex** 新增粘贴成功后等 0.5 秒再首个 Enter；tmux 的原有 0.5 秒抽成常量。已空、改变或滚动的框不补；确知仍留完整同一草稿才有限补 Enter，丢回执不重打。interrupt 送 Ctrl-C，成功不证明树已停。continue 可复用活执行；resume 在确认回收后重建执行身份。Linux guardian 持有新子 cgroup fd，空闲 kernel 先入组再 spawn；父先退、runtime 遇 SIGKILL 或正常 close 均核对全组 `populated 0`，含自行 setsid 的后代。 | #12/#18。验一次正式 user turn、并发 reply 一次应用、快照显示 parent exited 而 descendant 尚存不能复用、guardian 清理后再恢复。close 或 inspect 丢响应后只读一次、仅认与原 handle 绑定的签名终止收据；旧无签名、错身份、篡改或拒绝不得升级为完成。pending create 有时限/creator 退出可见，对账读取的清理证明仅证明 execution-empty，不能证明创建送达。守护与 runtime 同时死亡超出该用户态保证。待验。[#39→#40 差异][diff] · [源码测试][native-tests] · [清理凭据回归][receipt-tests] |

Python 的 `ExecutionBackend` 有 create/inspect/read/submit/interrupt/resume/close 七种操作。Rust 侧共享断言应从 CLI 读取 `result`、`diagnostic`、故障类别、当前 attempt、审计和可否重试，把 accepted、completed、definitely-not-sent、refused、uncertain 各自落到后端证据。例如 Herdr 的 pane 已关闭但 writer 停止未知时返回 uncertain；tmux 的关会话成功按自身协议返回 completed。Rust 内核 Unix 的进程组 kill 单独无法证明离开进程组的后代已退出。[Python 后端接口][tmux] · [Rust 内核][kernel] · [受控后端合同][backend-tests]

### Linux 三宿主与三后端的交叉验收

每格均为最终 Rust 下载件待验。行给出运行所需设施，列给出需要实际观察的宿主行为；一次实测只计入它实际使用的那一格。每个可用组合从公开 CLI 走首派、提问、单次答复、完成、确认、关闭，另核同会话恢复与异常边界。首派能收到提示的依据是持久会话或实际回调。[公开解析器][router] · [宿主约束][hosts] · [上游实验范围][manual]

| Linux 后端及条件 | Codex（#8） | OpenCode（#19） | Claude（#37） |
| --- | --- | --- | --- |
| tmux，同一用户、server/socket | 真 CLI；核对工作目录、输入框、rollout 和原会话恢复。上游记录过 Linux 真 Codex。 | 真 CLI；确认带 prompt 的首次任务实际提交，重开后验证一次输入及两代会话记录读取。 | 真 CLI；记录信任条件、session UUID、原会话重开以及一次正式输入。 |
| Herdr 0.9.5，私有 socket、可核实服务身份 | 验稳定草稿才有限补键；真 Herdr + 受控宿主与真 Codex 分别记录。 | 验自动提交仅本轮预填，检查共享 pane 及服务重启后的归属。Windows 实测另记平台。 | 解析器接受；以实际授权的 Claude 和同一 Herdr 入口验 argv、会话、问答与回收。 |
| bundled，内核在位、可写委派 cgroup v2 | 核实 #40 等待后一次提交；将 Windows 双聊天证据与 Linux 本格分开。 | 验预填匹配、退出后新执行身份、后代清理和相同宿主会话。 | Python 参数与通用创建路径允许选择；实验手册标 Claude disabled。#44 的冻结交接给出 Codex 验收范围，#37/#21 仍须核实 Linux Claude 的实际条件。 |

## Relay 与平台

| 目标 / 平台 | 对外可观察行为 | Rust 票与验证边界 |
| --- | --- | --- |
| Codex controller / 通用 | `--codex` 取 CODEX_THREAD_ID；显式 UUID 须能在相应 rollout 首行匹配顶层线程，拒绝 subagent、错误 ID、与当前线程冲突。队列调用 `codex queue --thread … --message … --resume` 剥离路由凭据。成功→received；无法启动→可回到 unsent；确认 no rollout→unsent、relay 退出 1；一般非零或超时→unknown，保持不自动重放。 | #7/#9/#16/#17/#18。先用受控 queue 验 argv、四类结果、接替、跨 scope 隔离；再以真实 CLI 与可观察的两次独立聊天，在发起回合结束后查原聊天新回合、稳定消息 ID 与 processed。待验。[路由投递][relay] · [CLI 用例][router-tests] |
| OpenCode controller / 通用 | 需要正在监听的 OpenCode TUI 或 serve、有 URL 与 session，并选定这一种 relay 目标。POST `/session/<session>/prompt_async`，JSON parts 文本包含待投批次。默认超时 60 秒（两控制端共用 WFC_QUEUE_TIMEOUT）。2xx→received；明确拒连→unsent；HTTP 错误、重定向、超时、连接失败且结果不明→unknown。禁止跟随重定向产生第二次投递。 | #38 接入 #9 的公共 relay；#19 是 worker 验收。真 HTTP fixture 统计请求与 body，覆盖 204、拒连、3xx、超时；已启动并有会话的真实 OpenCode 上，核对同一会话收到一批、一次新回合。旧 Python 真实会话探针已有记录；Rust 待验。[投递与选择][relay] · [历史 HTTP 实机探针][opencode-probe] |
| Claude controller / 通用 | 当前 relay 的目标是 Codex、OpenCode。Claude 可通过控制端 wait 读取。旧的 `notify_controller.py hook` 会找祖先目录中的任务文件，按报告摘要去重，向 Claude events_dir 写事件或调 Codex queue；README 明示它是历史参考，打包清单也无该模块。 | #21 记录此接口边界；旧钩子、Claude 控制端宿主内的事件 waiter 与现行 worker 的显式 ask/done 分开审查。[入口选择][relay] · [历史钩子][hook] · [发布说明][py-readme] · [打包清单][pack] |
| 平台适配 / 通用及其他平台 | Linux/macOS 使用有界 flock 与独立 session；Linux 用 boot ID 与 /proc 启动时刻防 PID 复用，处理 zombie 和权限未知。Windows 默认状态在 LOCALAPPDATA，采用 UTF-8 输出、文件字节锁、私有 ACL、进程 creation time、Job Object/ConPTY、命名管道；原生 .cmd/.bat shim 路径拒绝，可指定 runtime + 脚本。Windows forced runtime 死亡即使 Job 清空也可能缺签名收据；保持 unknown。macOS 无 Linux cgroup，#40 的 bundled 预检拒绝。 | #20 验 Linux 架构、动态依赖和清理；#25 验 macOS tmux/原生行为，#3/#10 验 Windows 下载件、Job、桌面无弹窗（交接 #24 后）。CI 有受控 worker、模拟 Windows launch 的单测；原生短测/交互窗口与 Linux、用户真实账号分别记账。[平台层][platform] · [guard 测试][guard-tests] · [Windows 实验验收范围][manual] |

`relay --detach` 输出子 PID/log，POSIX 启动独立 session，Windows 独立进程组；`--for` 有界 relay 到期注销。消息投递前先持久认领为 unknown/pending，成功送达与处理后 ack 单独可查。针对交接与 redelivery，要测接替期间未决的外部调用仅结算对应 claim，以及不确定投递后显式 resolve/ack。已确实送达的日志、HTTP 204、终端收字和已处理的回执属于不同观察点。[relay 循环][relay] · [持久化回归][router-tests]

## 复验取证

1. 对固定 Python #40 用 `PYTHONPATH=.:tests python3 -m unittest test_herdr.CodexSubmission test_bundled.BundledSelection.test_codex_receives_one_enter_outside_the_paste_burst test_bundled_completion test_bundled_protocol test_host_launch.WindowsAdapter`。作者首轮 **27/27**。独立审查整组两次均为 **26/27**，WireBounds.test_nested_frame_is_a_connection_error 先触发帧时限，单项重跑通过。用例包括受控画面、时钟、收据、协议、Windows argv 分支。这组无模型、外置服务或真实桌面。
2. 后端测试使用各自条件。Herdr 以 `WFC_HERDR_TEST_BINARY` 指向固定版本运行 `python3 -m unittest discover -s tests -p test_herdr.py -v`；Linux bundled 用仓内脚本在独立的委派单元下对源码及移动到空格目录的包重跑，检查 cgroup 无残留。两组均用真实终端机制和受控 worker，缺二进制导致的 skip 须显式记录。独立审查在 #40 的真实 Herdr 0.9.5、隔离 socket 与受控 worker 上通过 test_read_only_host_refuses_input，含关闭，fixture 已回收服务。两版 Python 对缺任务路径使用真实 tmux 3.4 和受控脚本时，dispatch 均报 host startup not confirmed（退出 1），status 有 1 条且 worker 随后已启动；close 退出 1，审查者停止专属 server 后确认无会话。该结果与受控 tmux 接受创建的路径分别记录。审查环境的 cgroup v2 只读，整树回收待可写委派环境，完整 Python 套件和登录宿主此轮未验。[Herdr 测试][herdr-tests] · [Linux bundled 脚本][verify-bundled]
3. 后续 #20/#21 从同一固定 Rust 下载件验哈希，使用独立目录、state、scope，记录平台/架构、下载件 SHA、宿主/后端版本、委派/模型/认证条件和实际角色；按上面各通道保存 dispatch/round/attempt、正式 user 消息数、原控制会话 ID、ack 版本，以及关闭前后服务/worker/子孙创建身份。记录“受控进程”“真后端＋受控宿主”“已登录真实宿主”“真控制聊天”的实际组合。外部账号不可用时留下原生脱敏原因、已完成层级和未验组合。[#24 验收][tracker]

[router]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py
[hosts]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L422-L703
[trust]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L704-L781
[tmux]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L1490-L1730
[reclaim]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2873-L3005
[watch]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3102-L3306
[relay]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3325-L3499
[herdr-reconcile]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3500-L3556
[herdr]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/herdr_backend.py
[bundled]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/bundled_backend.py
[runtime]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/bundled_runtime.py
[platform]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/wfc_platform.py
[hook]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/notify_controller.py
[pack]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/build.py
[py-readme]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/README.md
[herdr-doc]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/docs/herdr-backend.md
[bundled-doc]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/docs/bundled-backend.md
[launch-doc]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/docs/host-launch.md
[launch-probe]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/docs/verification/host-launch-17.md
[watch-probe]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/docs/verification/watchdog-19.md
[opencode-probe]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/docs/probes-20261105-opencode.md
[manual]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/docs/verification/bundled-kernel-experiment.md
[verify-bundled]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tools/verify_bundled_linux.sh
[herdr-tests]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_herdr.py
[launch-tests]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_host_launch.py
[router-tests]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py
[backend-tests]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_backend.py
[guard-tests]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_bundled_guard.py
[native-tests]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_bundled.py
[wire-tests]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_bundled_protocol.py
[receipt-tests]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_bundled_completion.py
[diff]: https://github.com/xmxxc/workflow-core/compare/2409c40b469e0e40bd5c55326df3d5ebb86ca81b..60d7d76e4bb861ac9e0bbb043aa1a36946578397
[rust-cli]: https://github.com/xmxxc/agent-router/blob/2e7a6963c53b35834309b8dd553d7edad879d955/src/main.rs
[kernel]: https://github.com/xmxxc/agent-router/blob/2e7a6963c53b35834309b8dd553d7edad879d955/src/kernel.rs
[check-wfc]: https://github.com/xmxxc/agent-router/blob/2e7a6963c53b35834309b8dd553d7edad879d955/tools/check_wfc.py
[tracker]: https://github.com/xmxxc/agent-router/issues/24
