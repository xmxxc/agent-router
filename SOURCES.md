# 内核来源与复建

| 组件 | 固定来源及保留范围 |
| --- | --- |
| WFC | xmxxc/workflow-core 提交 `3c06c0905faaf9e61de49404c33e70720409607f` 的 experiments/herdr-kernel-prototype。内核来自上游 src/main.rs，位于本仓 src/kernel.rs，入口改为由 wfc CLI 调用的 run。根 Cargo.toml 使用本仓的程序名和版本。tools/smoke.py 从 native_probe.py 增加协议、resize 和限时退出检查。tools/collect_notices.py 从同名脚本适配本仓目录和来源记录。原始 MIT 署名见 LICENSE-WFC。 |
| WFC 外置 Herdr 适配器 | crates/herdr 是本仓维护的 Linux 客户端。运行单元身份、RPC、写者观察和输入框识别对照 xmxxc/workflow-core 提交 `60d7d76e4bb861ac9e0bbb043aa1a36946578397` 的 herdr_backend.py 和 router.py。适配器使用仓内 LICENSE-WFC 的 MIT 许可。 |
| Linux bundled 后端 | `src/bundled/` 复用本仓 `4ec8699363027fb9757bb33e86ebab25f41de500` 的 Linux 运行时和已验证清理协议，沿用本仓 PTY 内核。hmac、sha2 和依赖校验和由 Cargo.lock 固定。 |
| Python 账本兼容组件 | `crates/legacy-state` 的表结构和历史字段来自 workflow-core #40 提交 `60d7d76e4bb861ac9e0bbb043aa1a36946578397` 的 `router.py`。组件许可为该目录的 LICENSE-WFC。独立二进制通过 rusqlite 0.37.0、libsqlite3-sys 0.35.0 编译 SQLite 3.50.2。其余依赖版本和 registry 源包校验和见 Cargo.lock，目标构建的许可清单位于收集产物 sources.json 的 cargo 条目。 |
| Herdr | 外置 Herdr v0.9.5 的发布提交为 `2ecf19a92fc3a7fff75b6ce3655ef04f4625ec01`。仓内提取的源码固定在 `ee5bfb5041bb3e4a49d591c72308aae3c81ecdde`。原始 Apache-2.0 文本在 licenses/LICENSE-Herdr。crates/ghostty-vt/build.rs 与 bindgen 0.72.1 生成的 src/bindings.rs 逐字保留。 |
| Ghostty Rust 封装 | crates/ghostty-vt/src/lib.rs 是从 Herdr 同路径提取并在本仓维护的文件。保留初始化、禁用图像存储、字素模式、窗口大小与写回调、剪贴板协议回复、活动屏幕与视口格式化。内核没有消费的图像解码、绘制、鼠标、铃声计数和目录/剪贴板事件存储已裁去。crate 清单移除无调用者的依赖。Cargo.lock 从 WFC 锁文件裁去相应条目，保留条目的版本与 registry 校验和均来自原锁。 |
| portable-pty | Wez Furlong 的 MIT crate 0.9.0。registry 原包 SHA-256 为 `b4a596a2b3d2752d94f51fac2d4a96737b8705dddd311a32b9af47211f08671e`。基于 Herdr 已应用四项补丁的 Git tree `2d925f8f0bcdbf1e74ff3be5ab59013a3649d7ed`。本仓 0005 补丁来自 `80a43312928a23f9ad7ef9248892a261839b1e71`，先等 Windows 进程退出再回报关闭。0006 补丁让可取消的异步等待线程独立持有进程句柄。见 vendor/portable-pty/LICENSE.md、vendor/portable-pty.patches.md 和 vendor/patches/portable-pty。 |
| Ghostty 原生库 | Ghostty 提交 `44f2a44df7e8c4a0c6df3f7d872ef3d7ead88e51` 的 libghostty-vt 源码发行包，经 Herdr 应用六项补丁，保存于它的 vendor/libghostty-vt 子树 `e5ec12a4a8178f5f32f60407760e5c2d2a4883b9`。Mitchell Hashimoto 和 Ghostty contributors 的 MIT 文本在 licenses/LICENSE-Ghostty。原始发行包名、六项补丁说明和补丁原文分别在 vendor/libghostty-vt.vendor.json、vendor/libghostty-vt.patches.md、vendor/patches/libghostty-vt。 |
| Microsoft ConPTY | packaging/windows 下保留 Herdr 固定的 Microsoft.Windows.Console.ConPTY 包元数据和 Microsoft 原始 LICENSE、NOTICE。可选组件的下载 URL、包级和各文件的 SHA-256 均在 conpty.json；本仓编译和 Windows CI 使用系统 ConPTY。 |

[WFC 固定源码](https://github.com/xmxxc/workflow-core/tree/3c06c0905faaf9e61de49404c33e70720409607f/experiments/herdr-kernel-prototype) · [Herdr v0.9.5 发布源码](https://github.com/herdrdev/herdr/tree/2ecf19a92fc3a7fff75b6ce3655ef04f4625ec01) · [Herdr 提取源码](https://github.com/herdrdev/herdr/tree/ee5bfb5041bb3e4a49d591c72308aae3c81ecdde) · [Ghostty 基线](https://github.com/ghostty-org/ghostty/tree/44f2a44df7e8c4a0c6df3f7d872ef3d7ead88e51)

外置 Herdr v0.9.5 的 `pane.read` 固定返回 `revision: 0`。公开回复核对绑定 pane 的聊天区后缀文字，并参考随后读取的同文 ANSI 样式。两次读取分别观察当时的画面，输入前仍可能发生终端变化。[发布版读屏实现](https://github.com/herdrdev/herdr/blob/2ecf19a92fc3a7fff75b6ce3655ef04f4625ec01/src/app/api/panes.rs#L1512-L1552)

## Ghostty 源码输入

vendor/libghostty-vt-source.zip 是已打补丁的第三方源码。固定其中的 Zig 构建图、C ABI 头文件、包定义和平台源码，可以在各平台继续使用同一 build.rs。ZIP 中有 1,249 个文件，解压为 19,187,498 字节；构建图会引用多平台路径，这个计数描述源码输入。每次构建只链接目标平台的 VT 静态库。

下面是从 Herdr Git 对象重建该 ZIP 的维护命令。只需在比对或更新来源时运行，普通 checkout 的构建使用仓内 ZIP。Git 2.43.0 生成的 ZIP 为 4,366,678 字节，SHA-256 为 `8631810d25448eadc598350b9522aa8622c1c7d356935249ab88b5a13bc5ba6d`。解包后已含补丁，后续直接交给 Zig。

```sh
git -C /path/to/herdr archive --format=zip -9 --mtime=2000-01-01T00:00:00Z \
  -o /tmp/libghostty-vt-source.zip \
  ee5bfb5041bb3e4a49d591c72308aae3c81ecdde:vendor/libghostty-vt \
  LICENSE VERSION build.zig build.zig.zon src pkg include dist/cmake
sha256sum /tmp/libghostty-vt-source.zip
```

## 依赖与署名

Rust 1.96.1 由 rust-toolchain.toml 固定，Zig 0.16.0 由准备脚本核对。Linux x64 的 Zig 官方工具包 zig-x86_64-linux-0.16.0.tar.xz 的 SHA-256 为 `70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00`。Windows 工作流沿用 WFC release 验证过的固定 setup-zig action 和 zlib 准备步骤。zlib 下载归档的 SHA-256 为 `17e88863f3600672ab49182f217281b6fc4d3c762bde361935e436a95214d05c`，Zig 校验解包内容后返回 `N-V-__8AAB0eQwD-0MdOEBmz7intriBReIsIDNlukNVoNu6o`。[WFC 原生构建流程](https://github.com/xmxxc/workflow-core/blob/3c06c0905faaf9e61de49404c33e70720409607f/.github/workflows/bundled-kernel-experiment.yml)

vendor/INPUTS.sha256 记录本仓编译源码、清单、输入 ZIP、补丁与许可的 SHA-256。从仓库根目录运行 `sha256sum -c vendor/INPUTS.sha256` 可比对。源码包中的 build.zig.zon 和 pkg 下的 .zon 文件固定 Zig 下载 URL 与内容哈希，包括 translate-c 的传递依赖。许可证收集脚本按目标平台读取 Cargo 元数据，拷贝每个包的许可证、Cargo.lock、实际物化的 Zig 包许可证、Herdr 和 Ghostty 许可、补丁说明、ConPTY 元数据与本仓来源记录。生成的 sources.json 保留各 Zig 包的内容身份。Ghostty 构建图在配置阶段读取的主题包没有顶层许可，VT 模式不安装主题资源。此计数包括构建期输入。[WFC 的主题输入核查](https://github.com/xmxxc/workflow-core/blob/3c06c0905faaf9e61de49404c33e70720409607f/docs/verification/bundled-kernel-experiment.md)

Ghostty 源码包中的 pkg/README.md 带有单独的 MIT 声明。pkg/simdutf 的包定义标为 5.2.8，实际 C++ 头文件声明 9.0.0。许可证输出附带 simdutf 作者的 MIT 文本及该头文件 isadetection.h 段的原始版权声明。其他源码内的声明随固定 ZIP 保留。[simdutf 9.0.0 MIT 文本](https://raw.githubusercontent.com/simdutf/simdutf/v9.0.0/LICENSE-MIT)

uucode 0.2.0 的固定提交为 `2826a37a4562284fdacd8fa029d49509cc9bffcd`。Zig 按该包的文件清单裁掉了根 LICENSE.md 引用的两份署名。本仓在 licenses/uucode/licenses 中保留两份原文，收集器按 Zig 包哈希将它们还原到 LICENSE.md 引用的相对路径，并列入 sources.json 的 zig_materialized 条目。[Hoehrmann 署名](https://raw.githubusercontent.com/jacobsandlund/uucode/2826a37a4562284fdacd8fa029d49509cc9bffcd/licenses/LICENSE_Bjoern_Hoehrmann) · [Unicode 署名](https://raw.githubusercontent.com/jacobsandlund/uucode/2826a37a4562284fdacd8fa029d49509cc9bffcd/licenses/LICENSE_unicode)

Ghostty 的 src/terminal/x11_color.zig 将 src/terminal/res/rgb.txt 嵌入终端。X.Org rgb 1.1.0 发行归档的 SHA-256 为 `fc03d7f56e5b2a617668167f8927948cce54f93097e7ccd9f056077f479ed37b`。归档内 rgb.txt 与嵌入表的空白不同，逐行拆分后的全部 782 行字段一致。licenses/xorg-rgb/COPYING 保留归档中 The Open Group 的原始文本，交付路径记在 sources.json 的 embedded_data 条目。三份署名的 SHA-256 见 vendor/INPUTS.sha256。[X.Org rgb 1.1.0 原始归档](https://www.x.org/releases/individual/app/rgb-1.1.0.tar.xz)
