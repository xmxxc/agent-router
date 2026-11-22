# agent-router

WFC 的独立终端执行内核。上层通过标准输入输出请求内核启动终端、发送输入和读取画面。任务派发、消息回执和总控唤醒由 workflow-core 负责。

初始 Rust 入口及 Cargo 清单来自 xmxxc/workflow-core 提交 3c06c0905faaf9e61de49404c33e70720409607f 的 experiments/herdr-kernel-prototype，保留原项目 MIT 许可（LICENSE-WFC）。上游终端组件以 herdrdev/herdr 的 0.9.5 提交 ee5bfb5041bb3e4a49d591c72308aae3c81ecdde 为基线。独立构建所需的组件、各自许可证及跨平台验证正在迁入。
