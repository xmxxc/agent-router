# Linux 版 wfc 行为对照

这份清单回答两个问题：Python 调用方今天能做什么，哪张 Rust 工单负责让同一个调用成立。对照起点是 Rust `2e7a6963`。这个提交能启动真实终端内核，但它把 12 个工作流命令保留为占位入口。Python 的 23 个公开子命令（含 4 个顶层别名）在这个 Rust 提交上均未实现。Linux 发布验收由 #24 汇总。[Rust 入口](https://github.com/xmxxc/agent-router/blob/2e7a6963c53b35834309b8dd553d7edad879d955/src/main.rs) · [Python 解析器](https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/router.py#L3568-L3745) · [Linux 主票 #24](https://github.com/xmxxc/agent-router/issues/24)

| 要查的内容 | 入口 |
| --- | --- |
| 每个命令的参数、默认值、JSON、错误、退出码、旧接口与旧状态 | [CLI 与数据兼容](python-cli.md) |
| 一次派工如何续轮、提问、收信、确认、撤销写入权和恢复 | [工作流与回执](protocol.md) |
| Codex / OpenCode / Claude、两种 relay、tmux / Herdr / bundled、进程归属与回收 | [宿主和后端](hosts.md) |
| 哪些工单负责各项验收、哪些可并行 | [工单映射与验收](work-items.md) |
| 如何在临时目录复验、核对测试条件和冻结后的变化 | [复验与 Python #44](recheck.md) |

## 固定对象与证据边界

| 对象 | 采用的事实与用途 |
| --- | --- |
| [Python PR #39](https://github.com/xmxxc/workflow-core/pull/39) · [源码 2409c40b](https://github.com/xmxxc/workflow-core/tree/2409c40b469e0e40bd5c55326df3d5ebb86ca81b) | 基于 `3c06c090`。worker 回调独立于总控租约，跨 owner 接替仍能 ask/done。由在途 create 的发起进程身份与时限判定故障。较迟到达的过期观察不能覆盖已经确认的退出。PR 的 Linux 全套报告为 444 项、440 过、4 个 Windows 跳过；该数字属于上游记录。 |
| [Python PR #40](https://github.com/xmxxc/workflow-core/pull/40) · [源码 60d7d76e](https://github.com/xmxxc/workflow-core/tree/60d7d76e4bb861ac9e0bbb043aa1a36946578397) | 继承 #39，是本表各行的 Python 默认版本。CLI 解析未改，bundled Codex 接受粘贴后等 0.5 秒再 Enter；tmux 原有的等待提取为同一常量。新增确定性输入回归和 Windows 采集工具。PR 记录真实 Windows 双聊天、单条正式用户消息与一条投影、重复 ack 同版本；两种记录不能相加计数。CLI 版本字符串与 #39 相同，区分产物须用 SHA。 |
| [Rust PR #22](https://github.com/xmxxc/agent-router/pull/22) · [2e7a6963](https://github.com/xmxxc/agent-router/tree/2e7a6963c53b35834309b8dd553d7edad879d955) | `--help`、带 Rust/目标/构建编号的 `--version`、私有 `--kernel`、旧文件名无参数启动。真实 PTY（伪终端）可创建、读取、输入、缩放和关闭。调度凭据、数据库、后台执行树和宿主适配都在这个内核之外。 |
| [Rust #4](https://github.com/xmxxc/agent-router/issues/4) 的独立候选 [PR #28](https://github.com/xmxxc/agent-router/pull/28) `503f6cdd2b17e36be99743414c4f3efac4462ff3` | 已核对其 [src/workflow.rs](https://github.com/xmxxc/agent-router/blob/503f6cdd2b17e36be99743414c4f3efac4462ff3/src/workflow.rs) 和 [tests/dispatch.rs](https://github.com/xmxxc/agent-router/blob/503f6cdd2b17e36be99743414c4f3efac4462ff3/tests/dispatch.rs)。它同步运行指定程序，成功后公布 `dispatches/d-<32 hex>.json`；仅接受 `dispatch --role ROLE --task FILE --cwd DIR -- PROGRAM ...` 和 `status [--dispatch ID]`。本轮用该 build 派发 /usr/bin/true 后另起进程按 ID 查到 round=1 和相同 attempt（两次退出 0）；Python 形式的 host/report 派发与 status --active 均退出 2。Python 的 host/report、默认 tmux、`d-<8 hex>`、既有数据库、后台返回、状态信封及消息历史仍要分别验证。#28 对非 UTF-8 的 task/cwd 与 Python 均拒绝、无可见派发；对缺失任务或目录先行拒绝，Python 在受控后端接受时可查 1 条。WFC_STATE 未设、空值、字面 ~/custom 的落盘位置或返回码也不同。见[逐例实测与归属](python-cli.md#linux-派发路径实测)。 |
| [Rust PR #23](https://github.com/xmxxc/agent-router/pull/23) · [dbae2c45](https://github.com/xmxxc/agent-router/tree/dbae2c45e4ce46d528ec276de743f6d20dd1b51f) | 候选下载、清单、哈希与署名检查；[#3 的 PR #27](https://github.com/xmxxc/agent-router/pull/27) 在 `0502a4f7` 补 Windows 子进程退出观测。两者都没有交付 Linux 工作流。#20/#21 将在功能集成后对同一下载件验收。 |
| [Python 冻结票 #44](https://github.com/xmxxc/workflow-core/issues/44) | #44 已关闭，冻结产品为 #40 的 60d7d76e，完整 tree 与参数清单相同。固定交接文档为 0ecbe94c；已测 Windows 下载件与待验 Linux 六项见[冻结复查](recheck.md#44-冻结后复查)。 |

每张子表沿用上述固定对象。除入口/内核的明确记录外，表内的 Rust 票号均表示**责任归属或建议新增验收**。只有在集成提交或固定下载件上实际执行且留下结果，才能把对应行改成“通过”。平台一栏写“通用”表示同一外部合同适用于可用的后端；Linux 证据须附实际架构、后端和宿主。Windows 和 macOS 的结果单列。

## 本轮并排实测

Linux x86_64，Python 3.12.3，在两个固定源码树和单独的临时状态目录运行；Rust 使用自报 build `2e7a6963c53b35834309b8dd553d7edad879d955` 的本地 release 文件，SHA-256 为 `edbfd217cc700e84dfd2f6be8508e2f5d2f1d3a9775171cb7030c10149d9d9eb`。这是源码与本地二进制对照，#20 另验下载件。[复验命令](recheck.md)

| 入口和条件 | Python #39 / #40 | Rust 固定起点 |
| --- | --- | --- |
| 无参数、`--version` | 退出 0；帮助摘要、`wfc 0.4.7-rc.4` | 退出 0；显示保留词和 Rust 构建身份 |
| `-V extra` | 退出 0，版本按首参数短路 | 退出 2，stderr 参数错误；记录为兼容差异 |
| 空目录 `status --active`、`reclaim` | 退出 0；前者含空 dispatches/reclaimable/undelivered/waiters，后者含 yes=false 和 sessions=[] | 两项均退出 2，提示入口未开放 |
| `dispatch --help`、`owner --help`、`claim --help` | 均退出 0，参数见子表 | 均退出 2；第一项为保留命令，其余为未知参数 |
| `wait --timeout 0` | stdout 为含 messages=[]、timedOut=true 的 JSON，退出 3 | 退出 2 |
| `status --active --dispatch missing`、`ack xxx` | 前者互斥，stderr usage / 2；后者未知消息，stderr 诊断 / 1 | 均为入口未开放 / 2 |
| `--kernel`，标准输入 EOF | Python 公共 CLI 参数错误 / 2；另用 companion 调内核 | 收到 protocol=1 握手并正常退出；另用真实临时子进程完成 PTY 往返和 close |

在独立状态中还执行了旧库只读/并发首次升级、done 重放/ack、unknown 消息对账、跨宿主替换、外人会话拒绝回收、resume 换 attempt、错误 worker 身份与缺 companion 测试。该组 #39 为 9/9，#40 为 10/10，多的一项验证 Codex 粘贴与 Enter 的次序。worker、tmux 与模型输入由上游夹具控制。作者首轮 #40 的 CLI/迁移、协议、宿主/后端分别为 7/7、54/54、27/27。独立审查同三组为 7/7、53/54、26/27；协议失败项单跑仍超时，宿主组第二次仍为 26/27，失败项单跑通过。两轮公共抽样均为 9/9 和 10/10。各组有重叠。真实登录、原聊天唤醒、整个 Linux 无 Python 下载件流程留给对应实施票。[测试与重跑](recheck.md)

## 集成候选怎样验收

1. #20 固定下载 URL、候选 SHA/tree、包与内部可执行文件 SHA-256、体积、CPU/系统/动态库、工具与外部宿主版本。在新的 Linux 运行环境解包，验证构建编号；将用于比较的 Python 解释器和源码放在**另一套环境**。从下载件进入所有命令，worker 的 ask/done、后台 relay 和重启后命令也使用该文件。记录运行环境中 Python 不可用以及实际执行文件。参考测试机可使用 Python 驱动比较。
2. #21 对同一套输入记录 stdout JSON、stderr、返回码与外部副作用。随机 ID、路径、PID、时刻按一次运行内的对应关系比较；重复 ID、代数、消息个数、状态版本、去向、是否创建/清理进程必须严格核对。每项记 Python SHA、Rust SHA/包哈希、正常/失败/跳过及原因、日志/测试链接和负责票。
3. #24 取得各宿主与后端的结果、真实 A/B 总控、异常/清理证据和独立审查后，一次性交给 Windows 总控（#10）。macOS 在 #25 原生 runner 复验。未具备账号、委派 cgroup、桌面或相应服务时记录受影响的行和原因，并保留待验状态。[#20 分发](https://github.com/xmxxc/agent-router/issues/20) · [#21 全量行为验收](https://github.com/xmxxc/agent-router/issues/21) · [#24 交接条件](https://github.com/xmxxc/agent-router/issues/24)
