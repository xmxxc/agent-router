# agent-router

Rust 版 `wfc` 在同一个可执行文件中提供命令行和终端内核。上层通过私有标准输入输出协议启动终端、写入按键、调整大小、读取当前画面并关闭进程。编译需要本仓、Rust 1.96.1、Zig 0.16.0、Python 3.12 和平台链接器。首次构建会取回 Cargo 和 Zig 的固定依赖。

## Linux x64

在带 C 链接器的 GNU/Linux 上，从仓库根目录运行下列命令。先将 Zig 0.16.0 放入 PATH，或用 ZIG 指定可执行文件的绝对路径。

```sh
rustup toolchain install 1.96.1 --profile minimal
python3 tools/prepare.py
export WFC_BUILD_ID="$(git rev-parse HEAD)"
cargo build --release --locked --target x86_64-unknown-linux-gnu
cargo test --workspace --release --locked --target x86_64-unknown-linux-gnu
./target/x86_64-unknown-linux-gnu/release/wfc --version
python3 tools/smoke.py target/x86_64-unknown-linux-gnu/release/wfc --kernel
python3 tools/collect_notices.py x86_64-unknown-linux-gnu target/bundled-notices
```

## Windows x64

在带 Visual Studio 2022 C++ Build Tools 和 Windows SDK 的 x64 Developer PowerShell 中运行。使用 MSVC 版 Rust，PATH 中应有 Python 3.12 和 Zig 0.16.0。原生 CI 在 windows-2022 上从本仓构建，复制产物到含空格的目录，再运行相同的终端往返测试。通过后另行检出固定的 WFC 测试树，执行六项原生生命周期验证。[Windows 验证工作流](.github/workflows/kernel.yml)

```powershell
rustup toolchain install 1.96.1-x86_64-pc-windows-msvc --profile minimal
python tools/prepare.py
$env:WFC_BUILD_ID = git rev-parse HEAD
cargo +1.96.1-x86_64-pc-windows-msvc build --release --locked --target x86_64-pc-windows-msvc
cargo +1.96.1-x86_64-pc-windows-msvc test --workspace --release --locked --target x86_64-pc-windows-msvc
./target/x86_64-pc-windows-msvc/release/wfc.exe --version
python tools/smoke.py target/x86_64-pc-windows-msvc/release/wfc.exe --kernel
python tools/collect_notices.py x86_64-pc-windows-msvc target/bundled-notices
```

prepare 校验并解出仓内的 Ghostty 源码包，校验 zlib 下载字节和 Zig 包哈希。已有原始 zlib 源包时可传入 `--zlib-archive <文件>`。其他 Zig 依赖按 Ghostty 的 .zon 清单校验内容，Rust 依赖按 Cargo.lock 校验。许可证输出目录须为新目录。

## 运行和接入 WFC

直接运行 `wfc` 或 `wfc --help` 会列出熟悉的工作流命令名。当前版本支持派发和状态查询，Linux 还支持后台运行与按编号关闭，预留命令返回退出码 2。`wfc --version` 显示 Rust 标识、包版本、目标平台、构建类型和编译时的 `WFC_BUILD_ID`（未设置、为空或仅含空白时，使用本地构建时间的 Unix 纳秒数）。

Linux 公开入口按宿主和后端派发，例如：

```sh
wfc dispatch --host codex --task task.md --cwd . --report reports/04-review/r1.md
wfc status --active
```

默认使用 tmux。此组合调用同一可执行文件中的 tmux 后端（#31）。宿主可选 codex、opencode 或 claude，`--launch-config` 或 `WFC_LAUNCH_CONFIG` 指向带有 env、hosts 的 JSON 配置。每个宿主可指定 command、runtime 参数数组及 env。完整参数见 `wfc dispatch --help`。bundled 后端由对应工单接入。

Linux 也支持 Herdr 0.9.5（协议 22）。先启动自己的 Herdr 服务，再把仅当前用户可访问的 socket 绝对路径交给派发命令。也可用 `WFC_HERDR_ENDPOINT` 指定，命令行优先。

```sh
wfc dispatch --host codex --backend herdr --herdr-endpoint "$socket" \
  --task task.md --cwd . --report reports/review/r1.md
wfc status --dispatch "$dispatch"
wfc herdr-reconcile "$dispatch" --pane "$pane" --terminal "$terminal" \
  --actor operator --evidence "已核对目标工作区和终端"
wfc close "$dispatch"
```

`status` 的 `backendHandle` 保存单元标识、原服务和写者的进程身份。服务失联时，`backendAlive` 为 null。显式补绑可附 `--workspace`；服务、映射或根写者改变时还需 `--old-writer-evidence`。成功回执中的 `audit` 对应该 dispatch 的审计事件编号。`status` 可查到经办人、证据和本轮 attempt。补绑保留输入认领，关闭只操作绑定的 pane，并核实已记录写者均已退出。服务重启或仍有写者存活时，Herdr 的关闭回执会给出 `result: uncertain` 及原因。若首次投递回执丢失，进程可能仍在线，本轮状态保持 unknown，等待生命周期裁决。

实际请求关闭前，`wfc` 会在该派发的 `audit` 中持久保存关闭意图、目标 pane 和当时已知的写者。`status --dispatch ID` 中的 `closePending: true` 表示这次关闭仍待确认。此时输入与补绑会被拒绝；再次运行 `close ID` 只查询原单元，确认原 pane 消失且已记录写者全部退出后返回 completed。若连接在任何请求字节写入前失败，该次操作记为 definitely-not-sent，允许下一次关闭重新发送。

关闭意图与新的 `done` 消息串行提交。首次回报按当前 dispatch、round 和 attempt 核对关闭记录。关闭待确认或已经确认时，同一键和报告可重取原消息号，新完成记录会被拒绝。待确认的派发始终列在 `status --active` 的 `dispatches` 中。已确认的任务结果（包括原失败退出码）继续显示，`closePending` 单独表示资源关闭进度。启动回执读取出错时，状态查询在 `launchDiagnostic` 中保留原因；已核实的关闭归档继续提供本轮结果和资源状态。对有根进程的绑定，归档须与当前绑定的根 PID 和创建标识一致，当前绑定可以记录更多后来发现的写者。归档身份异常时，`backendDiagnostic` 说明原因，资源关闭保持待确认。可信的启动回执仍能提供 worker 退出码。停机证明须为非空白文本，且绑定中已无任何写者。旧归档的 worker 结果损坏时，`closeWorkerDiagnostic` 保留原因，已核实的资源状态仍为已关闭。

Herdr 首轮派发、输入和补绑使用独立 Rust 状态目录中的无主 scope。若随后认领该 scope，关闭须使用当前 owner 凭据。状态目录中出现 Python `router.db` 时，须先集成旧库和 owner 身份。受控服务联验可用 `python3 tests/herdr_public.py /path/to/herdr target/debug/wfc -v`。通过 `--herdr-backend` 向同一 `wfc` 的标准输入提供 `{"op":"submit","dispatch":"…","round":1,"attempt":"…","text":"…"}`，可验证投递认领与回执丢失后的处理。正式的 `reply`、跨轮复用和旧库身份迁移分别由对应工单集成。

`--launch-config` 优先于 `WFC_LAUNCH_CONFIG`。启动参数依次为宿主的 `runtime` 数组、`command` 数组、wfc 为本次任务生成的 Codex 参数。路径含空格时直接写在同一个 JSON 字符串中。例如 npm 安装的 Codex 可以指定 Node 和脚本各自的绝对路径。

```json
{
  "hosts": {
    "codex": {
      "runtime": ["/opt/Node bin/node"],
      "command": ["/opt/Codex CLI/bin/codex.js"],
      "env": {"CODEX_HOME": "/srv/worker profile"}
    }
  }
}
```

使用原生 Codex 可执行文件时，在 `command` 中填入该文件并省略 `runtime`。worker 先继承调用进程的环境，再依次叠加顶层 `env`、宿主 `env` 和 wfc 的本轮身份、状态及报告路径。wfc 会过滤继承环境和启动配置中的 `WFC_CONTROLLER_TOKEN`、`WFC_OWNER_TOKEN` 和旧 `WFC_WORKER_CALLBACK`（名称不区分大小写）。专用 worker 回调由调度层提供。tmux 会使用当前终端的 `TMUX`、`TMUX_PANE` 和 `TERM`。Codex 默认以 `workspace-write` 和 `never` 审批策略启动。

Linux 真实宿主验收需要 tmux、已登录的 Codex 或可用的 `OPENAI_API_KEY`，以及能执行 Codex 沙箱命令的主机。下面的命令使用私有临时 HOME、状态目录和 tmux socket，启动模型执行一项写报告的任务，从 Rust `wait` 返回的路径读回报告，并核对 `status` 中的同一条回执。把路径和模型替换成当前机器的值。传入 `--auth-file` 时使用已有登录文件的临时副本。仅设置 `OPENAI_API_KEY` 时省略该选项，脚本通过标准输入运行 `codex login --with-api-key`，把登录信息写入私有 profile。外部发送 `SIGTERM` 取消验收时，脚本回收自建进程并删除该临时目录，随后以同一信号退出。

```sh
python3 tools/check_codex.py target/x86_64-unknown-linux-gnu/release/wfc \
  --codex "/path/to/codex" --model "your-model" \
  --auth-file "$HOME/.codex/auth.json"
```

回执包含 dispatch、round、attempt、host、backend、scope、role、label、tmux 和报告绝对路径。省略 scope 会分配新值，默认 role 来自报告目录名最后一个连字符后的部分。Codex 的 git/network 开关作用于宿主参数，OpenCode 和 Claude 的回执会说明这些开关的处理结果。tmux 服务地址在派发时绑定，后续查询和关闭沿用原服务。

点名查询 `wfc status --dispatch ID` 返回当前 worker 进程状态、各轮任务结果及消息。接入 #6/#7 后，首条完成回报固定该轮的报告和 Git 快照，后续领取或进程退出仍能查回。已完成且会话仍存活的任务在 `status --active` 的 reclaimable 数组中。长编号和旧版短编号均可查询、关闭。

tmux 会话的首次关闭动作记录在 `closeEffect` 中。若同一 worker 在会话关闭后才写出退出码，后续查询和重复关闭都会返回该退出码，并保留首次动作。创建响应丢失时按原 attempt 查询启动登记；关闭归档写入失败时，重试会按原 attempt 重放同一次关闭。无主 scope 的已确认关闭回执可从只读历史副本重取。证据不足时，关闭返回未决。

`dispatch` 接收调用方指定的角色、任务文件和工作目录。`--` 后依次放受控 worker 的程序和参数。下面的 Bash 示例让 worker 在工作目录中写出它实际读到的任务，再由另一个 CLI 查回编号。

```sh
export WFC_STATE="$(mktemp -d)"
work="$(mktemp -d)"
printf '检查发布说明\n' > "$work/task.md"
wfc dispatch --role reviewer --task "$work/task.md" --cwd "$work" -- \
  python3 -c 'import os, pathlib; pathlib.Path("received.md").write_bytes(pathlib.Path(os.environ["WFC_TASK"]).read_bytes())'
wfc status
# 将 ID 换成 dispatch 输出中的 d-… 值
wfc status --dispatch ID
```

任务文件、工作目录和显式指定的程序路径都可以相对于调用方。程序也可使用 PATH 中的命令名。worker 以指定目录为当前目录，从 `WFC_TASK` 取得任务文件的绝对路径，并从 `WFC_REPORT`、`WFC_SCOPE`、`WFC_ROLE`、`WFC_DISPATCH`、`WFC_ROUND`、`WFC_ATTEMPT` 和 `WFC_STATE` 取得本次派发上下文。任务文件和工作目录的绝对路径需能用 UTF-8 表示。CLI 在标准输出返回一行 JSON，含 `dispatch`、`round`（首次为 1）、`attempt`、`role`、`task` 和 `cwd`。`status` 的 `dispatches` 数组列出已交付记录，可用 `--dispatch` 按编号查询。启动失败或查询未知编号时，CLI 返回非零退出码并打印诊断。

在 Linux 上，受控入口用同一个 `wfc` 启动独立会话。worker 成功启动且记录可查后，派发命令即返回，终端可以关闭。worker 的三个标准流连接到空设备，需要保留的输出应写入文件。使用 `wfc status --dispatch ID` 查询本次进程的结果。回执中的 `worker.state` 初始为 `running`，任务进程树退出后变为 `exited`，附带根进程的 `exit_code` 或 `signal`。例如 worker 退出 7 时，派发回执仍有效，稍后可查到 `exit_code: 7`。

`wfc close ID` 通知该任务的后台进程回收其 worker 和后代。后台进程只向自己尚未回收的子进程发送信号。成功回执含 `operation`、`result`、`dispatch`、`round` 和 `attempt`。重复调用安全，已结束的任务保留原退出结果。根进程被本次关闭终止时，`worker.state` 为 `cancelled`。回收超过 5 秒会返回非零，可再次用同一编号查询或关闭。若后台进程意外退出且未留下结果，状态为 `unknown`，close 会报告无法确认。本版 Linux 实现使用本机状态目录上的文件锁和可读的 procfs 子进程列表。

其他平台沿用前台派发，等待 worker 成功退出后发布记录。worker 的标准错误沿用调用方的标准错误。

派发时用 `--report FILE` 绑定本轮报告路径。相对路径从派发命令所在目录解析，省略时使用 worker 工作目录下的 `report-<dispatch>.md`，每次派发各有一个文件。用 `--scope NAME` 把多项派发分到同一个总控组，省略时会生成独立的组名。worker 从 `WFC_REPORT` 和 `WFC_SCOPE` 读取这两项。报告父目录会在启动前创建。

```sh
# worker 写好报告后提交，可用同一条命令安全重试
wfc done --report "$WFC_REPORT" --attempt "$WFC_ATTEMPT"
# 总控领取派发到 team-a 的消息
wfc wait --scope team-a --timeout 30
```

`done` 核对 `WFC_DISPATCH`、已提供的轮次和 attempt，以及已写好的报告文件。提交其他文件会退出 1。首次提交记录派发工作目录的 Git 提交号 `head` 和有无未提交变更的 `dirty`（含未跟踪文件）。目录不在 Git 仓库中、Git 失败或单条命令超过 5 秒时，两项均为 null。

显式传入 `--attempt ID` 时，默认重试键为 `done:<dispatch>:<attempt>`；也可自定 `--message-id KEY`。同一派发、同一键与相同报告重试，返回原数字编号 `message` 和稳定编号 `messageId`，标记 `duplicate: true`，保留该消息首次采集的 Git 快照。不同键分别创建消息。旧式调用可只设置 `WFC_DISPATCH`，新派发同时省略两个选项时，每次调用各产生一条消息。升级前尚无报告绑定和 scope 的 R1 派发，worker 保留原启动环境并重试原命令时，会取回原编号和领取状态。跨轮复用报告路径或轮次历史不全时，应传入本轮的 `--attempt`。

`wait` 默认等待 3600 秒，`--timeout` 接受小数秒，设为 0 可立即查询。它按提交顺序返回 `messages`，包含派发、轮次、attempt、角色、scope、报告路径、Git 快照、数字编号 `id` 和 `messageId`。可按 `--scope NAME`、`--dispatch ID` 或两者共同筛选。首次提交为 `unsent`（版本 1），领取为 `received`（版本 2）。消息在输出前标记为已收取，其他等待者可领取剩余消息。超时返回 `{"messages":[],"timedOut":true}`，退出码为 3。

总控重启后，运行 `wfc status --dispatch ID`，从 `dispatches[0].messages` 找回原消息。历史按提交顺序排列，查询不会改变回执。处理完报告后，用消息的数字编号或稳定编号运行 `wfc ack MESSAGE`。回执变为 `processed`，版本增加一次。并发确认时只有一个调用实际推进状态，其余调用和稍后的重试返回同一版本，并标记 `duplicate: true`。

用 `wfc owner acquire --scope team-a --owner controller-a` 为该组认领默认 60 秒的租约。将返回的 `token` 保存在总控环境的 `WFC_CONTROLLER_TOKEN` 中，或在单次操作时用 `--owner-token` 指定。用 `wfc owner renew --scope team-a` 续租。另一位总控可用 `wfc owner takeover --scope team-a` 接管并领取新凭据。接管完成后，旧凭据的 `wait`、`ack` 和下一批 `relay` 会报 `stale_controller_owner`。

状态目录一旦有租约记录，`wait` 和 `relay` 都需要指定 `--scope`。尚未认领的组仍可用原命令。普通总控可用 `status --dispatch ID` 查看组的 owner、代数和租约状态。

外部投递结果为 `unknown` 时，先核实对方确已收到，再用下面的命令记录证据。

```sh
wfc resolve MESSAGE --actor reviewer --reason "核对外部送达记录" \
  --evidence "外部回执编号" --body-hash SHA256
```

`--source` 默认为 `operator`。成功裁决后状态为 `received`，之后仍可单独 `ack`。同一声明可重试，改动证据或哈希会报冲突。`--body-hash` 是消息正文的 SHA-256。当前完成消息的正文字段为 `attempt`、`report`、`role`、`round`，带 `scope`、`head`、`dirty` 的快照还要保留这些字段及其原值。按键排序，以 UTF-8 编码 JSON，在冒号和逗号后各留一个空格，相当于 Python 的 `json.dumps(body, ensure_ascii=False, sort_keys=True).encode("utf-8")`。如需确认处理的是同一份正文，`ack --body BODY` 接受这段 JSON 原文。

本版首次领取缺少正文哈希的旧消息时，会将实际交付正文的 SHA-256 和 `received` 状态一起保存。升级前已领取的旧消息可能在后来写入收件箱时被补上两个值为 null 的 Git 字段。同一派发、同一 attempt 的第一条记录若带有请求键，缺少创建时间、显式 scope 和正文哈希，且两个 Git 字段均为 null，`ack --body` 也接受旧版领取时的四字段原文。判断时还会查看整份收件箱中排在它之前的记录。旧程序追加回报时需要读写整个收件箱；较早记录中的空请求键会使它读取失败，而保留至今的显式 scope、非空 Git 快照或创建时间也能排除旧程序曾在其后写入。其他派发的普通旧记录可以排在它前面。

提供正文的 `ack --body` 和提供摘要的 `resolve --body-hash` 都核对当前记录的正文；若记录已有正文哈希，还会核对已存哈希。省略正文的 `ack MESSAGE` 按消息编号和交付状态确认。在 Unix 上，确认或裁决因目录同步失败而报错时，可重试同一命令。重试会补做同步，并保留已有版本与裁决证据。

Rust 默认使用用户主目录下的 `.local/state/agent-router`，可用 `WFC_STATE` 指定独立状态目录。帮助、版本和内核入口都不会读写调度状态。与 Python 版并排使用时，把两份 `wfc` 放在不同目录，分别使用显式路径，并为每次调用指定各自的 `WFC_STATE`。

### 在 Linux 预览和回收 worker

```sh
wfc reclaim
wfc reclaim --yes
wfc reclaim --dispatch ID --dispatch OTHER_ID --yes
```

默认预览只列出任务已完成且资源仍在的原生派工。可重复使用 `--dispatch` 点名，包含未完成任务。命令会合并重复编号，并在开始回收前检查所有点名编号。预览只读取状态。加 `--yes` 后，命令会逐项在 scope 锁内复核当前执行、任务状态、资源归属和授权，确认账本可以发布回收记录后，再使用 `close` 的核验逻辑回收资源。受管理的 scope 使用 `--owner-token` 或现有的控制者令牌环境变量。

回收通常在标准输出返回每项结果。已消失或执行已变更的项会跳过。拒绝、失败或证据未决的项也有标准错误诊断，并使退出码为 1。中途失去授权时，命令以授权错误结束，之前成功的回收仍可通过 `status --dispatch ID` 核对。只有确认关闭的执行才会记录 `reclaimed`，完成回报、报告文件、工作目录和相邻会话会保留。

### 在 Linux 查看 Python 历史

```sh
WFC_STATE=/path/to/copied-state wfc status
WFC_STATE=/path/to/copied-state wfc status --dispatch ID
WFC_STATE=/path/to/copied-state wfc reclaim --dispatch ID
```

Linux 的 `status` 优先使用原 Rust 目录中的派工、消息账本或仍持有注册锁的 relay。只留空目录时，命令按 Python 规则查找 `router.db` 并显示旧库历史。可通过绝对路径选择独立的旧库副本。两种记录共处一处时，`wfc-legacy-state status` 可直接查看 SQLite 历史。

通过编号查询原生派工时，目录可遍历且该记录可读即可。列表查询缺少目录读取权限时会报告错误。发现旧库时，若原生 `messages.json` 路径指向目录等非普通文件，`status` 会报告路径类型错误。

Python 状态目录在 `WFC_STATE` 缺省或为空字符串时为 `~/.local/state/workflow-core`，字面值 `~/custom` 按用户主目录展开。空字符串让 Linux `status` 直接选择 Python 目录。缺省或非空设置在两处都没有记录时，`status` 返回原 Rust 路径的空视图。带账号的路径无法展开时，`status` 保留原 Rust 字面路径。选中旧目录时，循环链接等文件系统错误会显示诊断。`reclaim` 优先使用原生记录，选中旧库时提供只读预览。旧库的会话存活情况显示为未知。原生消息账本的路径查询失败时，可读的旧库仍可用于预览。`status` 和 `reclaim --yes` 继续报告路径错误。旧 schema 缺项会提示，WAL 模式需先用 SQLite 导出私有副本再查看。

Linux 组合验证可用 `python3 -B tests/legacy_public.py target/release/wfc`。停写、核对原回执、后端查证和副本还原见 [Python 状态组件](crates/legacy-state/README.md)。

启动 `wfc --kernel` 会收到一行 JSON 握手，其中含协议版本 1 和本次启动的 generation。保持标准输入打开，每行发送一个 JSON 请求。spawn 返回 nonce，后续 read、input、resize 和 close 使用同一个 unit、generation 和 nonce。关闭标准输入会结束内核。smoke 启动真实子进程，等待它就绪，调整窗口，输入随机文本并核对它回写的反转文本。

WFC 的 `build.py --bundled-kernel <内核文件> --kernel-notices <许可证目录>` 接收以上两项输出，将内核放在 WFC 旁边，命名为 `wfc-kernel` 或 `wfc-kernel.exe`。这份同名兼容副本支持 Python 的无参数内核调用。运行时也可将 `WFC_BUNDLED_KERNEL` 指向该副本。Windows 默认使用系统 ConPTY（Windows 的伪终端接口）。

旧安装若已将 `WFC_BUNDLED_KERNEL` 指向 `herdr-kernel-prototype`（Windows 为 `herdr-kernel-prototype.exe`），可保留原路径和文件名，用新产物替换该文件。Python 会以无参数方式启动它并读取握手。若原来使用其他自定义文件名，把新产物复制为 `wfc-kernel`（Windows 为 `wfc-kernel.exe`），将 `WFC_BUNDLED_KERNEL` 设为这份副本的绝对路径。能传入启动参数的调用方也可保留自定义文件名，显式传入 `--kernel`。

**可选的 Microsoft ConPTY 包**需要四个运行时文件。先按清单校验 nupkg 和三个二进制文件的 SHA-256，再按各自的 destination 放到内核可执行文件旁的 `conpty/` 目录内。按 notices 中的 destination 把两份署名放到同级 `THIRD-PARTY-NOTICES/`。第四个运行时文件是 `conpty/herdr-conpty.json`，与 `conpty.dll` 同目录。[Microsoft 包清单](packaging/windows/conpty.json)

从本仓根目录运行下面的命令，将最后一个参数换成内核可执行文件所在目录。它从同一份包清单生成标记文件，字节格式与 Herdr 的 `marker_data` 一致。[固定版本的生成逻辑](https://github.com/herdrdev/herdr/blob/ee5bfb5041bb3e4a49d591c72308aae3c81ecdde/scripts/package_windows_conpty.py#L237-L248)

```powershell
@'
import json, sys
from pathlib import Path
m = json.loads(Path("packaging/windows/conpty.json").read_bytes())
files = m["bundles"]["x86_64"]["files"]
marker = dict(schema_version=1, package=m["package"]["id"], version=m["package"]["version"],
              architecture="x86_64", files={f["destination"]: f["sha256"] for f in files})
(Path(sys.argv[1]) / "conpty/herdr-conpty.json").write_bytes(
    (json.dumps(marker, indent=2, sort_keys=True) + "\n").encode("utf-8"))
'@ | python - "C:\path\to\kernel-directory"
```

Unix 内核关闭仍存活的子进程时检查并终止其进程组。Windows 内核关闭直接子进程。WFC 负责整棵执行树，包括根进程先退出、后代另建会话、运行时意外退出，Linux 使用委派的 cgroup v2，Windows 使用 Job Object。普通终端往返可单独运行。验证整棵树时，另备 WFC 提交 `3c06c0905faaf9e61de49404c33e70720409607f` 的干净测试树。Linux 先将 `WFC_BUNDLED_CGROUP_ROOT` 指向测试专属且可写的委派目录，再运行下列命令。Windows 在原生 PowerShell 中用 python 运行同一脚本，传入 .exe。

```sh
cp target/x86_64-unknown-linux-gnu/release/wfc target/wfc-kernel
timeout 240s python3 tools/check_wfc.py /path/to/workflow-core target/wfc-kernel
```

脚本运行 WFC 已有的六个真实进程用例，并检查每个用例都实际执行。首次候选的 Linux x64 独立编译、终端测试和 stdio 往返已实测。Windows 原生和 Linux 委派进程树的验收由总控在冻结候选上执行。[来源、补丁与复建方法](SOURCES.md)

## 在 Linux 将完成回执送回 Codex 聊天

在发起派发的 Codex 会话内，使用同一份 `WFC_STATE` 和派发回执中的 scope 注册 relay。例如，下面的转发进程独立运行 90 秒，启动命令返回它的 PID 和日志路径。

```sh
wfc relay --codex --scope review --for 90 --detach
wfc status
```

`status.waiters` 显示正在运行的 relay、scope 和目标线程。也可以用 `--codex-thread UUID` 指定线程。wfc 在 Codex 的 sessions 目录读取该 UUID 的会话记录，验证它属于顶层聊天；有 `CODEX_THREAD_ID` 时还会与当前会话核对。省略 `--for` 或设为 0 时持续运行。无人认领的 scope 可直接使用。省略 scope 时，当前状态目录内的 scope 须全部无人认领，relay 才会领取各 scope 的回执。

同一批完成回执会随一次 `codex queue --thread … --message … --resume` 发送。聊天收到后按原 `messageId` 执行 `wfc ack MESSAGE`。队列返回成功后回执为 `received`。程序启动失败会恢复为 `unsent`，下次轮询可重试。队列明确报告找不到目标时恢复为 `unsent` 并退出。超时或其他非零退出保留 `unknown`；重启 relay 后仍可查询原消息，核对聊天后使用上述 `resolve` 和 `ack`。队列确认和聊天处理结果分别记录为 `received` 和 `processed`。

每次队列调用都有独立的进程组。队列程序和继承该组的临时辅助进程由同一监督进程清理；队列返回、调用超时、relay 意外退出都会触发清理。已有 Desktop 服务保有自己的进程组。需要在调用结束后继续运行的宿主，应在独立会话或进程组中启动。
