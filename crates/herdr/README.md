# Linux 外置 Herdr 后端

`wfc-herdr` 连接独立运行的 Herdr 0.9.5（协议 22）。`Handle` 是可序列化的运行单元身份。它包含原始 socket 路径、attempt、随机归属标记、服务进程、workspace、pane、terminal 和已观察的写者进程。Linux 进程身份由 PID、启动时刻及系统启动 ID 组成。

## 调度器接入

- 派发方在当前 owner 和 attempt 的锁内预留创建操作，选择显式 endpoint 或 `WFC_HERDR_ENDPOINT`。把私有启动器的 argv、cwd 和 label 交给 `Handle::create`。它在创建外部资源前，以及每次取得新身份后调用保存函数。保存函数应同步提交私有记录。创建未确认时用已落盘的记录观察或显式对账。
- 后续调用直接反序列化原 handle。`inspect` 核查进程存活，`read` 返回画面、版本号和截断标记。`submit` 前持久认领输入，整个发送和 Codex 草稿观察期间持有该 dispatch 的输入锁。OpenCode 首派可在同一把锁内对已交给 argv 的 brief 调用一次 `activate_opencode_prompt`。
- `submit` 和 `interrupt` 成功表示后端接受。`FailureKind::Uncertain` 表示结果未知，须保留认领。worker 的完成回执由调度器结算。`close` 在发送关闭前保存已观测的写者，核实原 pane 消失且写者均已停止之后返回成功。此处跟踪的是 Herdr 报告的根进程和前台进程。
- `herdr-reconcile` 的调用方在当前 owner 授权下传入显式映射、actor 和 evidence。服务、映射或根进程改变时需要旧写者证据。对原映射，确认 pane 消失、已记录写者均退出且有显式停止证据，可以登记已停止状态。`Handle::reconcile` 核实 nonce、attempt、映射和可观测写者，返回候选 handle 和审计内容。提交事务时再次核对 owner、attempt 和读取的原记录，将新 handle 与审计原子保存，并保留原有输入认领和回执。
- Herdr 每条请求单独连接。适配器在每次连接上核对服务进程，操作前检查快照。协议 22 将快照与实际操作分成两个请求，同一账户的管理操作须与调度器协调。`WFC_HERDR_TIMEOUT` 指定每组 RPC 的秒数，默认 8，有效范围为大于 0 到 60。

## 独立复验

在仓库根目录运行以下命令。测试驱动把一个私有临时目录当作一个 attempt，逐次启动新的客户端进程，并在目录锁内保存检查点、输入认领和审计。

```sh
cargo test --locked -p wfc-herdr
cargo build --locked -p wfc-herdr --example herdr_unit
python3 crates/herdr/tests/live.py /absolute/path/to/herdr target/debug/examples/herdr_unit
```

实测脚本使用独立 HOME、XDG、配置、socket 和受控 worker。设置 `WFC_HERDR_TEST_CODEX` 为实际 Codex Linux 可执行文件即可一同探测隔离账户的首次交互画面。
