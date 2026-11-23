# Linux tmux 后端接口

调度层可调用 `tmux::execute(state, request)`，也可启动一次 `wfc --tmux-backend`，将一个 UTF-8 JSON 对象写入标准输入后关闭输入，读取标准输出的一个 JSON 对象。后者用于跨进程集成和独立复验。每个请求带 `dispatch` 和 `op`。Linux 的新请求与旧记录中缺省、null、空字符串的 `backend` 都选 tmux，也接受显式的 `"tmux"`。其他后端由上层路由到对应实现。

| op | 输入和返回证据 |
| --- | --- |
| create | 必填 `cwd`、非空 `argv` 字符串数组，可选窗口名 `label` 和字符串映射 `env`。以 detached 方式启动 `wfc-<dispatch>`。启动结果引用私有 `receipt` 文件，记录宿主已启动、启动失败或已退出。 |
| inspect | 返回 `alive`、`owned`。已确认归属时还返回原生 tmux 的 `attach` 参数数组，可在交互终端直接执行，用 tmux 的 detach 键离开。 |
| read | 返回当前 pane 的 `screen`。 |
| submit | 必填 `text`，可选 `host`。按字面发送一次文字。Codex 有稳定匹配草稿时，最多补两次 Enter。 |
| interrupt | 向确认归属的 pane 发送 C-c。 |
| resume | 接收与 create 相同的启动数据。调用方提供恢复原宿主会话的 argv。OpenCode 另取 `text`，在画面稳定后提交。Codex 可传 `probe_read_only: true` 检查恢复后的只读提示。 |
| close | 仅关闭目标 session。已确认缺席时也返回 completed。 |

例如，已创建会话后可以在另一个进程读屏。

```sh
printf '%s\n' '{"op":"read","dispatch":"d-abc12345"}' | WFC_STATE=/path/to/state wfc --tmux-backend
```

用同一 `WFC_STATE` 与同一 tmux 服务访问会话。独立服务可用 `WFC_TMUX_SOCKET` 指定绝对 socket 路径，缺省沿用 tmux 的服务选择。调度层负责持久化服务选择、串行化同一 attempt 的业务操作，并向当前 worker 提供专用的 `env.WFC_WORKER_CALLBACK`。

请求格式错误或选择了其他后端时，进程非零退出。有效操作从 JSON 的 `result` 取结果。`accepted` 表示后端确认操作，`completed` 表示完成关闭，`definitely-not-sent` 表示确认未送，`refused` 表示归属或宿主拒绝，`uncertain` 需检查现场。文字写入后丢失 Enter 回执，归入 `input_uncertain`，调用方须保留未决输入，避免重放。宿主实际产生几条正式消息、当前 attempt 是否完成，由调度层查宿主记录和业务回执。

真实后端测试使用新建的临时 socket、state 和受控终端程序。先构建 Rust 的 wfc，再运行 `python3 tools/check_tmux.py target/debug/wfc -v`。
