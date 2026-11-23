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

直接运行 `wfc` 或 `wfc --help` 会列出熟悉的工作流命令名。当前版本提供帮助、版本和内核入口，预留命令返回退出码 2。`wfc --version` 显示 Rust 标识、包版本、目标平台、构建类型和编译时的 `WFC_BUILD_ID`（省略时使用本地构建时间的 Unix 纳秒数）。

Rust 的这三个入口都不读写调度状态。与 Python 版并排使用时，把两份 `wfc` 放在不同目录，分别使用显式路径，并为每次调用指定各自的 `WFC_STATE`。

启动 `wfc --kernel` 会收到一行 JSON 握手，其中含协议版本 1 和本次启动的 generation。保持标准输入打开，每行发送一个 JSON 请求。spawn 返回 nonce，后续 read、input、resize 和 close 使用同一个 unit、generation 和 nonce。关闭标准输入会结束内核。smoke 启动真实子进程，等待它就绪，调整窗口，输入随机文本并核对它回写的反转文本。

WFC 的 `build.py --bundled-kernel <内核文件> --kernel-notices <许可证目录>` 接收以上两项输出，将内核放在 WFC 旁边，命名为 `wfc-kernel` 或 `wfc-kernel.exe`。这份同名兼容副本支持 Python 的无参数内核调用。运行时也可将 `WFC_BUNDLED_KERNEL` 指向该副本。Windows 默认使用系统 ConPTY（Windows 的伪终端接口）。

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
