# Python router.db 的离线读取与迁移

从仓库根目录执行 `cargo build --locked -p wfc-legacy-state`。生成的
`target/debug/wfc-legacy-state` 内置 SQLite，可独立运行。
读命令按 Python 规则查找 `WFC_STATE/router.db`。Linux 上变量缺省或为空时使用
`~/.local/state/workflow-core/router.db`，支持相对路径、`~`、`~/...` 及系统账户的 `~用户名/...`。
状态路径中的符号链接按目标路径解析，目标目录可以尚未创建。
现有 Rust 前台派发使用自己的 JSON 记录。

## 从副本核对

1. 在原 Python 总控记录仍在运行的 worker、relay、waiter 和最后收到的消息。
   为实际切换安排停写窗口，让各写者完成或停下，再复制整个状态目录、任务和报告。
   如只需查看某一时刻的账本，可用 SQLite Backup API 创建一致的数据库副本。
   开启 WAL 时必须包含已提交的 WAL 数据；可在导出的私有副本上执行
   `PRAGMA journal_mode=DELETE` 以便离线预览。保留原目录、原执行文件和原报告路径。
2. 以下命令中的 `copy` 指向操作者创建的独立目录。迁移命令只使用显式给出的目录。
   应复制文件内容；Linux 迁移会拒绝硬链接或符号链接的 router.db。
   保持副本和备份只对原操作者可读，其中保存了 owner 和 worker 凭据。

   ```sh
   copy=/path/to/private/copied-state
   WFC_STATE="$copy" wfc-legacy-state status
   WFC_STATE="$copy" wfc-legacy-state status --active
   WFC_STATE="$copy" wfc-legacy-state status --dispatch d-example
   WFC_STATE="$copy" wfc-legacy-state reclaim --dispatch d-example
   wfc-legacy-state migrate --offline-copy "$copy"
   ```

3. 对照 `messages` 的数字 id、messageId、state、statusVersion、bodyHash 和原始正文，
   再核对 `rounds` 的 attempt、任务、报告、结果及完成时间。旧库的 `schema` 提示表示缺失字段暂按
   null 查询。只有历史回执、故障、审计或尝试的任务也可以点名查询。此时
   `dispatchRecorded: false`，当前身份与后端为 null，回执和尝试保留各自的原值。
   reply 的顶层 `to` 是所答问题的数字 ID。审计的 `ownerGeneration` 记录发起代数，
   `resolution.ownerGeneration` 记录裁决代数。已有记录第一次需要修复时，成功输出的 `backup` 指向同目录的
   `router.before-*.db`。它是 SQLite 一致性备份。事务失败时可通过同一命名查找操作前副本。
   空库首次初始化和无需变更的库返回 `backup: null`。多个 Rust 写者的首次升级由同一 SQLite 写锁串行化。
4. 临时目录里的 `running` 只表示账本最后记录的状态。旧 worker 的回调命令、启动回执、报告、
   backend handle、PID 和 relay 的控制端仍指向原环境。工具展示记录的身份，`backendAlive: null`
   与 `result: unknown` 表示需回到原后端核实。会话生存性、回收、重发和接管需要后端或调度层实际查证。
   有完成报告但会话未知时，reclaim 预览列出该 dispatch 的诊断。`waiters` 保留原登记身份。
   `received` 可用 dispatch 详情找回，`unknown` 需按原证据对账；从副本查询不会发送输入。
5. 回退时先停止使用工作副本的写者，再将 Python 指回保留的原目录。核对停写窗口之后的回执后再恢复处理。
   也可将 `router.before-*.db` 放到全新私有目录，命名为 `router.db` 后只读复核。
   避免覆盖带有后续写入或旧 WAL/journal 的目录。数据库备份仅包含数据库，任务与报告文件须单独保留。

## 接入与复验

库入口为 `state_dir()`、`status(state, dispatch, active)`、
`reclaim_preview(state, dispatch)` 和 `open_offline_copy(state)`。
最后一个入口返回已提交升级的 rusqlite 连接与可选的备份路径，供调度层在完成停写切换后接续。
状态输出是持久账本的离线视图；后端在线状态、待投递权限、owner 授权、回调真实性和回收执行由各自的调用方实现。
快照缺失时，迁移会核对回执正文、生命周期审计和已有尝试记录。能确定归属的 done 按原轮次、attempt、报告及原始正文恢复。
取得写连接前，每条消息正文都必须是 SQLite 能以 UTF-8 解码的 TEXT，包括尚无摘要的旧消息。按解码后的原文字节核对已有 SHA-256。摘要矛盾时错误指明派工和消息编号，事务回滚；缺失的摘要在升级时按原文补齐。
逐轮核对会保留已有结果原文和快照完成时间。Python 写入快照与 done 收件时分别读取时钟；
快照时间丢失时，仅能以唯一匹配的 done 收件时间作为完成的观察时间。两处时间都缺失时会拒写。
现代账本可包含同一报告多次完成的回执。按消息编号核对提交顺序，最终快照须能印证同一 attempt 的最后一次 Git 结果。
同一 attempt 的回执按解析后的 JSON 值比较，属性顺序差异不会改变结果。每条回执保留自己的正文、摘要、标识、版本和时间。
早期 Python 升级器按数字编号选取当前报告的第一条旧 done，直接复制其正文与时间。重复提交的旧回执保留各自的创建时间。现代 worker 随后提交时则由末次带 attempt 的回执核验更新后的快照。按任务、当前轮次和 attempt 核对父行与快照的报告路径，运行中快照也可从派工记录补回路径。两处都缺失时，从能确认归属的记录在同一事务内恢复。
现代完成结果中的 report、head、dirty 分别核验。缺键时仅补有证据的值，保留已有字段及完成时间；非 Git 工作区写入的 JSON null 是有效值。早期仅含报告路径的回执按其原格式恢复。
早期 done 曾只记录报告路径。升级后再次提交时，按旧回执当时已有的字段核对归属，由末次回执核验最终的 Git 结果和完成时间。
后端观测失败也可能写入完成时间列。running 快照没有 done 时，若只留有后端诊断或没有结果，且同一尝试有随后接受的启动裁决，就保留其运行状态、原时间和裁决记录。
resume 或 handoff 的前后 attempt 在启动前已提交，离线核验沿审计链确定当前身份，并保留每次启动的原始结果。
早期库保留各报告的历史消息，用当前报告定位对应的 done。旧父行若仍指向已被后续记录替代的快照，
需要历史记录能唯一确定其后继才会更新。
缺少结果证据、存在多个当前候选或证据冲突时，迁移点名任务并回滚。早于 attempt 机制的记录仍可分配首轮身份。
仅剩屏幕观察记录的任务可按 ID 查看观察时间 `screenObservedSince`，当前身份和在线状态显示为未知。
已有尝试历史时，取得写连接需要当前轮次和 attempt 同时匹配快照。身份冲突时返回任务诊断；即使库无需升级也执行此检查。多次尝试复用同一报告时，只把能确定归属的 done 计入轮次。

```sh
cargo test --locked -p wfc-legacy-state
python3 crates/legacy-state/tests/python_ledger.py /path/to/workflow-core-checkout target/debug/wfc-legacy-state
python3 crates/legacy-state/tests/python_recovery.py /path/to/workflow-core-checkout target/debug/wfc-legacy-state
python3 -B crates/legacy-state/tests/python_fields.py /path/to/workflow-core-checkout target/debug/wfc-legacy-state
python3 -B crates/legacy-state/tests/python_history.py /path/to/workflow-core-checkout target/debug/wfc-legacy-state
python3 -B crates/legacy-state/tests/python_pre_attempt.py /path/to/pr39 /path/to/pr40 target/debug/wfc-legacy-state
python3 -B crates/legacy-state/tests/python_equivalent.py /path/to/pr39 /path/to/pr40 target/debug/wfc-legacy-state
```

跨版本检查用 #39 或 #40 固定版自带的隔离宿主夹具生成两轮记录。它在副本上核对已有 scope、
已收未 ack 与 unknown 回执、各表逐条数据，以及迁移后原 Python 对 owner 的校验和 ack。

共享根命令接入后，需用最终 Linux 下载件复验状态目录发现、历史回执公开字段、owner/worker 鉴权、实际后端身份、回收与回退。

## 在 Linux 的公共 `wfc` 中导入已停副本

`wfc migrate-legacy --offline-copy SOURCE --stop-proof FILE` 用于已停止写入的独立
#39/#40 SQLite 副本。将 `WFC_STATE` 显式设为一个尚不存在的绝对目录，目录的父级必须已存在，
并且源和目标的目录树须分开。旧 writer、worker、waiter、relay 和重启入口须先停止。提交无
WAL 或 journal 的一致快照。命令会拒绝还记录有 waiter、非终态轮次或未决生命周期事件的副本。

`FILE` 是 JSON。`sourceSha256` 为 `SOURCE/router.db` 的 SHA-256，`target` 为规范化的绝对目标，`writersStopped` 必须为
`true`，`stoppedBy` 与 `evidence` 记录经办人和停写依据。每个原受管 scope 都需要一条
`ownerReplacements` 项，键为原 scope，值含 `tokenSha256`（新高熵凭据的 SHA-256）和
`leaseUntil`（未来的 Unix 秒时间）。原 owner 身份保留，代数增加一。新凭据本身应由操作者
独立交给授权方。日志和回执只展示 owner 元数据。未受管 scope 不填替换项。

导入会原子发布完整目标，原 SQLite 字节留在目标的私有 `legacy-source/router.db` 备份。
同一来源和同一停写凭证可重复请求，已发布目标中的后续写入会保留。当前切片接受具有完整
身份和摘要的 controller `done`、`question` 回执。无法安全表示的数据会使整个导入拒绝。
原 `received` 可用原数字 ID 或 `messageId` 确认，`unknown` 留待明确对账，旧 `unsent`
留待审查。旧派工可查询历史，旧会话和回调仍不属于 Rust 管理范围。启用后已经产生写入的
回退需保全新回执并对账外部效果。
