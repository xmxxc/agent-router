# Python router.db 的离线读取与迁移

从仓库根目录执行 `cargo build --locked -p wfc-legacy-state`。生成的
`target/debug/wfc-legacy-state` 内置 SQLite，可独立运行。
读命令按 Python 规则查找 `WFC_STATE/router.db`。Linux 上变量缺省或为空时使用
`~/.local/state/workflow-core/router.db`，支持相对路径、`~` 和 `~/...`。
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
   null 查询。已有记录第一次需要修复时，成功输出的 `backup` 指向同目录的
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
历史尝试已存在而当前身份残缺时，迁移返回 dispatch id 并回滚。多次尝试复用同一报告时，只把能确定归属的 done 计入轮次。

```sh
cargo test --locked -p wfc-legacy-state
python3 crates/legacy-state/tests/python_ledger.py /path/to/workflow-core-checkout target/debug/wfc-legacy-state
```

第二条用 #39 或 #40 固定版自带的隔离宿主夹具生成两轮记录。它在副本上核对已有 scope、
已收未 ack 与 unknown 回执、各表逐条数据，以及迁移后原 Python 对 owner 的校验和 ack。
