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

默认使用 tmux。此入口调用同一可执行文件中的 tmux 后端（#31）；仅构建本分支时会以非零退出码报告缺少后端。宿主可选 codex、opencode 或 claude，`--launch-config` 或 `WFC_LAUNCH_CONFIG` 指向带有 env、hosts 的 JSON 配置。每个宿主可指定 command、runtime 参数数组及 env。完整参数见 `wfc dispatch --help`。独立的 herdr、bundled 后端和控制器授权由对应工单接入。

回执包含 dispatch、round、attempt、host、backend、scope、role、label、tmux 和报告绝对路径。省略 scope 会分配新值，默认 role 来自报告目录名最后一个连字符后的部分。Codex 的 git/network 开关作用于宿主参数，OpenCode 和 Claude 的回执会说明这些开关的处理结果。tmux 服务地址在派发时绑定，后续查询和关闭沿用原服务。

点名查询 `wfc status --dispatch ID` 返回当前 worker 进程状态、各轮任务结果及消息。接入 #6/#7 后，首条完成回报固定该轮的报告和 Git 快照，后续领取或进程退出仍能查回。已完成且会话仍存活的任务在 `status --active` 的 reclaimable 数组中。长编号和旧版短编号均可查询、关闭。

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

Rust 默认使用用户主目录下的 `.local/state/agent-router`，可用 `WFC_STATE` 指定独立状态目录。帮助、版本和内核入口都不会读写调度状态。与 Python 版并排使用时，把两份 `wfc` 放在不同目录，分别使用显式路径，并为每次调用指定各自的 `WFC_STATE`。

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
