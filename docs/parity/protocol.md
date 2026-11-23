# 工作流、消息与恢复

固定参照为 Python [#39 的提交][py39] 和 [#40 的提交][py40]。下表源码与行号指 #40。Rust `2e7a6963` 的入口将 12 个工作流词保留为退出码 2 的占位，其余工作流词报参数错误。本页行为均待 Rust 实现和验收，票号表示后续归属。[Rust 入口][rust]、[Rust CLI 测试][rust-test]

## 从任务到下一轮

scope 汇集同一总控负责的任务，dispatch 标识一份工作分配，round 表示第几轮任务，attempt 表示这轮的哪次执行。`status --dispatch ID` 能查当前身份、历史轮次、消息及操作记录。下表属通用合同；Linux 的宿主、后端和进程条件见[宿主和后端](hosts.md)。

| 操作与归属 | 可用条件和故障验收 | 固定源码与受控回归 |
| --- | --- | --- |
| 派发与查询 · #4/#5/#13 | 保存 scope、role、round=1、attempt、任务与报告。Herdr/bundled 在实际启动前提交 pending 创建记录；确定未发且没有分类的失败清掉 dispatch/round，已分类或不确定的结果可查。tmux 的 uncertain 初次派发保留 running 轮次，Herdr/bundled 保留 unknown。以另一进程查询并区分启动缺失、早退、无法确认。 | [dispatch][dispatch]；[RoundIdentity][round-test]、[BackendContract][backend-test] |
| 在原进程继续 · 补票；复用 #4/#12/#16 | continue 只接已完成且后端仍活着的任务。沿用 dispatch、scope、role 与终端，round 加一，新 attempt 和同目录未用过的报告名。并发只能登记、输入一次。明确拒收或未发且仍属本次时恢复前轮，无法确认保留新轮和故障。两次 continue、途中 takeover、旧报告迟到分别验收。 | [continue][input]；[ContinueDispatch][cont-test]、[ParallelState、RoundIdentity、ReportReuse][round-test]、[OwnerLifecycleFences][fence-test] |
| 同轮提示 · 补入继续票/#14 | nudge 在活着、可写的 attempt 上把多行压成一行，保存消息。取锁后重查 owner、轮次状态与未决操作。completed 引导 continue，退出引导 resume，疑似送达保留诊断。 | [nudge][input]；[Nudge][nudge-test]、[OwnerLifecycleFences][fence-test] |
| 恢复原宿主会话 · #15/#18/#19 | resume 需确认旧执行离线，并找到原会话。未完成沿用任务/报告/round，完成后成对提供新任务和新报告开下一轮。均生成新 attempt、保留历史，外部启动前登记 pending。锁内重查并发完成、归属及会话；身份无法判断先对账。用真实会话回忆原上下文，替身注入发起后崩溃和旧回调迟到。 | [resume][resume]；[Resume][resume-test]、[RoundIdentity][round-test]、[InterruptHandoff][life-test] |
| 中断与换 worker 宿主 · 补票；依赖 #5/#15/#16/#18 | interrupt 先将当前 attempt 设为 unknown，提交 pending 审计，再发中断；按键被接受后仍需显式后续处理。handoff 核对旧执行归属，记录 from/to attempt 与 host，确认旧执行关闭才启动另一宿主。未完成同轮接手，已完成须新任务和报告。无新任务、同宿主且当前尝试已 accepted 的重复 handoff 返回原 audit；pending/uncertain 必须对账。验收旧回调被拒、外来会话受保护、模型与历史可查，以及同一时刻最多一个写入者。 | [interrupt][interrupt]、[handoff][handoff]；[InterruptHandoff][life-test] |
| 关闭一个执行 · #5/#18 | close 单独记录资源处置结果。已消失返回 completed；未完成轮次确认关闭后置 cancelled。关闭前已完成或已失败的结果和回执继续可查，清理过程产生的进程退出不能覆盖任务结果。拒绝外来会话；未决 close 阻止重复 close/reclaim。 | [close][close]；[InterruptHandoff][life-test] |
| 批量回收 · 补票；复用 #5 | reclaim 默认只读预览已完成且仍活着的单元；显式指定可包含未完成任务，执行需 --yes。先验证所有显式 ID。默认目标若在锁内已进入下一轮则 changed；显式指定的目标依然可关。拒绝其他状态目录/未验证的执行。单个失败后继续下一项，输出逐项结果与整体错误码。保留历史、报告、工作树和宿主会话以供 resume。 | [reclaim][reclaim]；[Reclaim][reclaim-test] |

## 消息的四种状态

unsent 表示等待领取，unknown 表示已尝试外部投递、结果有待查明，received 表示收到，processed 表示总控确认已处理。每条消息有整数 id、稳定 messageId 和状态版本。请求方的 --message-id 为一次逻辑请求命名，与接收方看到的 messageId 分开。下表各项通用。[消息存储与响应][post]

| 操作与归属 | 候选应复验的外部结果 | 固定源码与测试 |
| --- | --- | --- |
| done、ask · #6/#11 | 同一 dispatch、稳定键和内容重试，返回原 id/messageId、当前版本及 duplicate；同键改内容或跨消息类型报冲突。带 --attempt 省略键时，done 按 dispatch+attempt，ask 再含问题/选项散列；旧形态同时省略二者没有自动幂等键。ask 的选项按逗号拆分去空项。done 要本轮实际存在的报告，保存路径、提交、含未跟踪文件的 dirty；非仓库、Git 超时或失败时 head/dirty 为 null，重试保留最初快照。 | [生产回执][done-ask]；[MessageRoundTrip][msg-test]、[DoneHead][git-test] |
| 晚到的问答/完成 · #6/#11/#15/#17 | 旧 attempt、旧轮报告不能推进当前轮。无 attempt 的新 ask 只允许无歧义的首轮首次尝试；同报告多 attempt 的新 done 拒绝省略 attempt。Herdr/bundled 的 resume/handoff 启动 pending 期间可记录精确新 attempt 的早到问题；unknown 轮次拒绝 reply。 | [生产回执][done-ask]、[reply 核验][input]；[RoundIdentity][round-test]、[受控 backend][backend-test] |
| wait、status · #4/#7/#9 | 按 id 排序一次领取本 scope 全部 unsent，打印前记为 received（v1→v2）。在打印前终止 wait，重启后可从 status --dispatch 的 messages 查回；再 wait 只取新的 unsent，超时返回 3 与空 messages/timedOut=true。#7 原票的“重取”可用历史查询验收；如果要求 wait 再次入队，需记录为接口差异。status 不标送达；undelivered 按旧 delivered 位筛选，可能包含 unknown。 | [领取][take]、[wait][wait]、[status][status]；[MessageRoundTrip][msg-test]、[Status][status-test] |
| relay · #9/#16/#18 | 把已领消息先记 unknown（v2），再送含 delivery=pending 和 `deliveryAttempt=relay:<messageId>:<statusVersion>` 的信封。明确送达→received（v3），确定未启动或目标缺失→unsent（v3）；结果不明保持 unknown，重启或换 owner 都不重发。只结算该批 ID、UID、dispatch、版本与状态均匹配的行；并发 resolve/ack 及后来消息不被覆盖。 | [relay][relay]；[ScopedDetachedCodexRelay][relay-test]、[RelayUncertainDelivery][round-test] |
| ack、resolve · #7/#18 | ack 接整数或稳定 ID，从 received→processed（wait 路径 v3），重复同一确认维持版本；即使已 processed，错 --body 也冲突。哈希取储存的 UTF-8 正文，Python `json.dumps(body, ensure_ascii=False, sort_keys=True)`，含默认空格及正文自身字段，不含信封。resolve 仅从 unknown→received，提交 actor/source（默认 operator）/reason/evidence/正文 SHA-256；完全相同声明可重取，改动依据拒绝。之后按实际处理情况单独 ack。以并发 CLI、旧 owner 与丢响应复验。 | [ack 与 resolve][ack]；[MessageRoundTrip][msg-test]、[OwnerLifecycleFences][fence-test] |
| reply 输入 · #12/#16/#18 | 以整数问题 ID 回答，合并换行。先声明该问题，再取 dispatch 输入锁与 scope 归属锁，重查可写性，提交一次。明确未发送、拒收或提交前等锁超时，只撤销本次声明。已发文字但 Enter 失败、响应丢失等保留声明，二次 reply 被拒。验证宿主正式用户输入次数、空输入框、滚屏、只读提示和答复后完成。 | [reply][input]；[PartialTyping][typing-test]、[DroppedEnter][enter-test]、[OwnerLifecycleFences][fence-test]、[#40 bundled 输入用例][paste-test] |

## 控制权与崩溃边界

| 场景与归属 | 验收与前置条件 | 固定依据 |
| --- | --- | --- |
| 争用、续租与接管 · #16/#17 | acquire/claim 在未认领或过期 scope 建立新代；活租冲突。renew 须有效同 token，只延租；takeover 可立即替换，旧 token 不可作为新代。未认领的明确 scope 允许无 token；一旦数据库存在 owner，无 scope 的 wait/relay 在扫描前拒绝。控制操作校验当前凭据；普通控制端仍可只读 status，worker 回调凭据仅限 ask/done。测试两 scope、过期和旧控制端。 | [owner 与回调][owner]、[命令分派][guard]；[ScopedDetachedCodexRelay][relay-test]、[BackendContract][backend-test] |
| 交接原 worker · #17 | A 换 B 时保留原 worker PID、创建身份、宿主会话和 attempt。独立回调凭据只授权当前分配，continue 为同进程附新轮引用，resume/handoff 给新尝试独立凭据。仅公开 attempt 和引用不能授权；旧控制 token、跨 dispatch、旧轮及 worker 借回调调用 status/ack/takeover 均被拒。B 收到可去重的 question，给一次 reply，收唯一 done 并确认。旧版已启动且缺独立凭据的 worker 在接管后仍会报 stale_controller_owner；旧库回填不能补发其启动时没有的凭据。 | [签发与验证][owner]、[continue][input]；[受控凭据测试][backend-test]、[含旧版迁移分支的原生内核/替身 worker][native-takeover] |
| 排队与归属竞争 · #9/#12/#16 | 已完成的接管要挡住旧 relay 下一批、旧控制输入和重复确认。在途的显式 scope 外部调用与接管串行，其他 scope 的 SQLite 写入可进展；在途成功时租约刚过期，仍只结算已领消息，下一次领取被拒。等待输入锁期间接管，旧 reply 只撤销本次尚未提交的声明。 | [relay][relay]、[reply 与 continue][input]；[ScopedDetachedCodexRelay][relay-test]、[OwnerLifecycleFences][fence-test] |
| 生命周期人工对账 · #18 | create/interrupt/close/handoff/resume 的 audit 记录操作、from/to、round/attempt、结果及发起 owner 代。pending/uncertain 阻止危险操作重放。reconcile 须指定 dispatch+audit、当前 owner（已认领 scope）、actor、evidence、result；同声明幂等，冲突和非当前尝试拒绝。保留历史发起代，另记实际裁决代。accepted 创建/接替/恢复→running，accepted close→cancelled，其余裁决对未终结轮次→unknown。显式 resume 证明绑定当前代且只能消费一次。分别在“已创建/回执未存”“已输入/无响应”“已完成/未确认”处中断，再启动只按已有证据处置。 | [pending 操作][interrupt]、[reconcile][reconcile]、[resume][resume]；[InterruptHandoff][life-test]、[OwnerLifecycleFences][fence-test] |
| bundled 创建中断 · #14/#18；Linux 需委派 cgroup v2 | pending 创建时按创建者 PID 加启动身份检查。确证退出/身份复用或超过本轮上限时送可对账的 unknown；身份缺失/读取失败不能据此判死。同 audit/attempt 已确认退出优先，晚到的期限样本与成功后迟到的观察不能再发或覆盖结论，兼容旧版本已记的退出。reconcile --cleanup-receipt --result unknown 只接受当前 bundled create 的匹配清理证明，观察后复查 handle/attempt/owner，留下 operator 证据，不重建或重发。 | [#39 修正][py39]、[创建观察][watch]、[cleanup 对账][reconcile]；[并发采样][create-test]、[原生 cleanup 证明][cleanup-test] |
| watchdog 与状态 · #13/#14/#18 | wait/relay 采样进程与会话在线、屏幕变化、结构化 host turn、待答问题、最近输入和启动回执。输出兼容 state、status、lifecycle 与 signals。故障类包括 quota、authentication、policy_rejected、permission_required、protocol_disconnected、input_not_delivered、timeout、process_exited、host_error。后端观察不明显示 unknown；保留已知终态。静屏超时只报一次，变化后可再报；待答问题暂停常规 stall 检查，重复刷新仍有最大轮时限。新输入前采到的旧错误丢弃。按 dispatch/round/attempt/故障指纹与消息同事务去重，并发与重启只报一次，新的 turn 或不同错误可报告。 | [状态计算][status]、[采样与去重][watch]；[Watchdog][watch-test]、[HostErrors][fault-test]、[MultiSignalContract][multi-test] |

## 复验方法与证据层级

在固定 Python 源码根目录运行以下用例。它们创建临时数据库。test_router.Env 使用替身 tmux/Codex/OpenCode/Claude，记录 argv、输入和退出结果；test_backend.BackendContract 使用可控后端，可核验跨进程 CLI 和持久状态。[替身实现][env-test]、[后端替身][backend-test]

```sh
env -u WFC_WORKER_CALLBACK -u WFC_CONTROLLER_TOKEN -u WFC_OWNER_TOKEN \
    -u WFC_DISPATCH -u WFC_STATE -u WFC_LAUNCH_CONFIG -u CODEX_THREAD_ID \
    PYTHONDONTWRITEBYTECODE=1 PYTHONPATH=.:tests python3 -m unittest \
    test_router.MessageRoundTrip test_router.InterruptHandoff \
    test_router.HostErrors test_backend.BackendContract
```

本次在 Linux 的固定 #40 克隆上运行以上四类共 54 项，通过 54 项。其他表内类可替换末尾用例名逐项复验。BundledNative 需 WFC_BUNDLED_TEST_KERNEL 与平台隔离条件；可核验实际进程 PID、创建身份、终端、后代退出与资源回执，其 worker 是本地 fixture。#17 需两个真实控制聊天，#8/#19 需登录的真实 worker 核实原会话、正式输入和回报，#21 在同一 Linux 下载件上汇合。#40 增加 bundled Codex 分开粘贴与 Enter 的 0.5 秒等待和一次提交回归，tmux 沿用原等待时长，Windows 桌面短测另有平台条件。[原生内核夹具][native]、[#39 到 #40 差异][diff]、[真实接管票][issue17]

[py39]: https://github.com/xmxxc/workflow-core/commit/2409c40b469e0e40bd5c55326df3d5ebb86ca81b
[py40]: https://github.com/xmxxc/workflow-core/commit/60d7d76e4bb861ac9e0bbb043aa1a36946578397
[diff]: https://github.com/xmxxc/workflow-core/compare/2409c40b469e0e40bd5c55326df3d5ebb86ca81b...60d7d76e4bb861ac9e0bbb043aa1a36946578397
[rust]: https://github.com/xmxxc/agent-router/blob/2e7a6963c53b35834309b8dd553d7edad879d955/src/main.rs#L8-L53
[rust-test]: https://github.com/xmxxc/agent-router/blob/2e7a6963c53b35834309b8dd553d7edad879d955/tests/cli.rs#L33-L55
[issue17]: https://github.com/xmxxc/agent-router/issues/17
[dispatch]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L781-L878
[input]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L1827-L2023
[resume]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2705-L2870
[interrupt]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2041-L2121
[handoff]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2285-L2402
[close]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2122-L2196
[reclaim]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2884-L3005
[post]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L968-L1066
[done-ask]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L1048-L1217
[take]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3006-L3030
[wait]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3306-L3324
[status]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2408-L2624
[relay]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3325-L3499
[ack]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L1218-L1287
[owner]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L224-L373
[guard]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3738-L3746
[reconcile]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L2197-L2284
[watch]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3102-L3305
[round-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L2019-L2312
[cont-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L887-L960
[fence-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L4138-L4515
[nudge-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L2728-L2779
[resume-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L1580-L1789
[life-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L3707-L4076
[reclaim-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L3484-L3706
[msg-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L165-L323
[git-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L503-L579
[status-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L2805-L2972
[relay-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L1187-L1470
[typing-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L1895-L1979
[enter-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L3253-L3420
[watch-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L815-L886
[fault-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L2417-L2727
[multi-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L4077-L4137
[env-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_router.py#L23-L164
[backend-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_backend.py#L21-L342
[paste-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_bundled.py#L15-L45
[native-takeover]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_bundled.py#L229-L336
[create-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_bundled.py#L337-L428
[cleanup-test]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_bundled.py#L599-L757
[native]: https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests/test_bundled.py#L92-L146
