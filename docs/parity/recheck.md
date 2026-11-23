# 从固定源码复验，以及 #44 冻结后的更新

## 取得两份独立参照

以下 Bash 命令从官方 Git 源取完整提交，在新建临时目录操作。允许比较的机器需要 Git、Python 3 和 Linux 用户态；纯 Rust 下载件的运行环境另见主表。保留 `$trial` 供查证，两个临时工作树各用自己的数据库、用户目录和宿主配置。

```bash
set -euo pipefail
trial=$(mktemp -d)
p39=2409c40b469e0e40bd5c55326df3d5ebb86ca81b
p40=60d7d76e4bb861ac9e0bbb043aa1a36946578397
git init -q "$trial/ref"
git -C "$trial/ref" fetch --no-tags https://github.com/xmxxc/workflow-core.git "$p39" "$p40"
git -C "$trial/ref" merge-base --is-ancestor "$p39" "$p40"
for rev in "$p39" "$p40"; do
    git -C "$trial/ref" worktree add -q --detach "$trial/$rev" "$rev"
    git -C "$trial/$rev" rev-parse HEAD 'HEAD^{tree}'
    mkdir -p "$trial/home-$rev"
done
git -C "$trial/ref" diff --stat "$p39" "$p40"
```

预期 tree 依次为 `53e19402226184be9d9dc4d788287e534e349f3b`、`a401334e1525b236b654e30385f4ee216c2f9c5e`。两份 `wfc.py` 均报 `0.4.7-rc.4`。[#39 历史](https://github.com/xmxxc/workflow-core/commit/2409c40b469e0e40bd5c55326df3d5ebb86ca81b) · [#39 到 #40 差异](https://github.com/xmxxc/workflow-core/compare/2409c40b469e0e40bd5c55326df3d5ebb86ca81b..60d7d76e4bb861ac9e0bbb043aa1a36946578397)

## 列举入口、参数和可运行用例

在同一个 Bash 会话中执行。顶层帮助只有摘要，从实际子解析器取全表。以下快照包括参数、默认值、类型、选项个数、选项互斥关系、枚举值和公开测试名。函数在解析完定义时停止，避免触发产品动作。入口架构发生变化时先检查新的解析器，再更新采集方式。

```bash
inventory() (
    cd "$trial/$1"
    env -i PATH=/usr/bin:/bin HOME="$trial/home-$1" LC_ALL=C.UTF-8 \
      /usr/bin/python3 -B - <<'PY'
import argparse
import ast
import json
from pathlib import Path
from unittest.mock import patch
import router

class Captured(Exception):
    pass

def capture(parser, *_):
    subs = next(a for a in parser._actions if isinstance(a, argparse._SubParsersAction))
    for name, command in sorted(subs.choices.items()):
        options = []
        for a in command._actions:
            options.append([a.option_strings, a.dest, a.required, a.nargs, a.default,
                            getattr(a.type, "__name__", None), a.choices, type(a).__name__])
        groups = [[a.dest for a in g._group_actions] for g in command._mutually_exclusive_groups]
        print(json.dumps([name, options, groups], sort_keys=True))
    raise Captured

with patch.object(argparse.ArgumentParser, 'parse_args', capture):
    try:
        router.main([])
    except Captured:
        pass
for base in (Path('tests'), Path('tools')):
    for f in sorted(base.rglob('test*.py')):
        for cls in ast.parse(f.read_bytes()).body:
            if isinstance(cls, ast.ClassDef):
                for item in cls.body:
                    if getattr(item, 'name', '').startswith('test_'):
                        print(f'{f}::{cls.name}.{item.name}')
PY
)
inventory "$p39" > "$trial/39.inventory"
inventory "$p40" > "$trial/40.inventory"
diff -u "$trial/39.inventory" "$trial/40.inventory" || test "$?" -eq 1
```

两个版本应各出现 24 个子解析器（含供 Herdr 子进程使用的 `_herdr-call`），23 个公开命令，公开子解析器定义一致。去重后有 46 个业务长选项，另有自动添加的 --help。测试名快照在 tests/ 下由 444 项变为 445 项，另在 tools/ 下增加 20 项 Windows 采集工具用例；这些是源码中的测试定义数。所有权的四个 action 共用 `owner` 子解析器，另有四个顶层别名。内建 `_host-run`、`_bundled-guard`、`_bundled-runtime` 在解析器前分派。另核 `wfc.py` 自身的 help/version 短路、`owner_action` 的 claim 转换、handler 内对默认值/成对参数/身份的检查；它们不会全部出现在帮助里。[CLI 表](python-cli.md)

## 在临时状态中复跑本次抽样

```bash
for rev in "$p39" "$p40"; do
    (
        cd "$trial/$rev"
        env -i PATH=/usr/bin:/bin HOME="$trial/home-$rev" LC_ALL=C.UTF-8 \
          PYTHONDONTWRITEBYTECODE=1 PYTHONPATH=.:tests /usr/bin/python3 -m unittest -v \
          test_router.Status.test_status_reads_an_old_state_database_without_upgrading_it \
          test_router.ParallelState.test_parallel_first_use_of_an_old_state_database_upgrades_it_once \
          test_router.MessageRoundTrip.test_stable_done_receipt_is_idempotent_and_ack_is_versioned \
          test_router.MessageRoundTrip.test_unknown_receipt_has_explicit_resolve_path \
          test_router.InterruptHandoff.test_handoff_changes_host_and_revokes_the_old_attempt \
          test_router.Reclaim.test_a_session_of_another_state_directory_is_refused \
          test_router.RoundIdentity.test_resume_retry_keeps_round_and_closes_old_attempt \
          test_router.WorkerIdentity test_bundled.BundledSelection
    )
done
```

#39 共 9 项、#40 共 10 项，均通过。要重现主表的 CLI 样本，分别以 `env -i PATH=/usr/bin:/bin HOME="$trial/home-$rev" WFC_STATE="$trial/state-$rev" /usr/bin/python3 -B "$trial/$rev/wfc.py"` 和独立 `WFC_STATE` 下的绝对路径 Rust 文件执行主表各行，保存两个输出流和退出码。参数错误和超时是预期非零；包身份通过 `sha256sum` 与 `wfc --version` 检查。Rust 终端往返从固定 Rust 源码目录执行 `python3 tools/smoke.py /absolute/path/to/wfc --kernel`，测试会结束临时子进程和内核。[Rust 冒烟](https://github.com/xmxxc/agent-router/blob/2e7a6963c53b35834309b8dd553d7edad879d955/tools/smoke.py)

## 上游测试覆盖到哪一层

以下文件均在 [#40 的 tests 目录](https://github.com/xmxxc/workflow-core/tree/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tests)。对源码运行 `python3 -m unittest discover -s tests -v` 可跑常规套件；看日志中的实际执行、skip 与条件。在专用目录中隔离宿主配置和状态。Windows 构建/采集附加用例位于 [tools/windows-desktop-short](https://github.com/xmxxc/workflow-core/tree/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tools/windows-desktop-short)，不在该 discover 路径。

| 文件（省略 test_ 前缀和 .py 后缀） | 观测和前提 | Rust 验收归属 |
| --- | --- | --- |
| router、backend | 真实 CLI 子进程/SQLite 与 fake tmux、宿主命令、屏幕、HTTP 或 FakeBackend 等；并发、状态、消息、所有权、重放、恢复与故障。有 Unix 专用夹具。 | #4–#18、续派/回收/跨宿主接替和旧库草稿；按协议子表选具体类 |
| host_launch、platform | argv/环境、退出、收据和诊断，使用受控进程及平台分支/模拟；含真文件锁/进程和仅 Windows 可跑项 | #5/#8/#13/#14/#16，Windows #3/#10 |
| herdr、herdr_cleanup | Codex 输入框可模型化；HerdrContract 需固定外置服务+受控宿主；服务代际、同 pane/异 pane、断线、工作进程与后代清理，平台限定项另报 | 外置 Herdr 草稿、#12/#18 |
| bundled、bundled_guard、bundled_completion、bundled_protocol、bundled_fixture_input | 模拟收据、键输入、协议边界；BundledNative 需真实内核和本机隔离能力。Linux 运行在可写委派的 cgroup v2 中，Windows 采用 Job Object。比较签名收据、身份、执行树和活动 waiter；注明 Linux 专用信号/IO 压力与 Windows console 分支。 | Linux bundled 草稿、#5/#12/#13/#17/#18；Windows #3/#10 |
| bundled_experiment_package、distribution、wfc | 清单/文件哈希/署名、移动目录、Python 单文件入口及 Windows 启动器。Rust 包另验真实下载来源和无 Python 运行环境 | #2/#20/#21；Windows #3/#10 |
| foreground_evidence、tools 下 startup/desktop_evidence | 模拟窗口证据、Windows 启动与会话投影计数。真实 Windows Desktop 测试另需交互桌面和两条已登录聊天 | #10，Linux 侧在 #12/#17 复用“一个正式输入”的判据 |
| notify_controller | 历史 hook 的分类/投递回归；标准 wfc 打包入口中无此模块。Claude 控制端可走公开 wait；relay 公开目标见宿主表 | #21 记录接口边界 |

上游 CI 将常规替身、Windows core、真实 Herdr 配受控 worker 分开。Linux 整树回收的复验脚本为 `tools/verify_bundled_linux.sh REPO KERNEL LOG_DIR`，在独占的 `Delegate=yes` 测试单元执行，结束时检查本轮子 cgroup 并释放本轮单元。对真实宿主逐个注明 CLI/模型、已登录账号是否可用、终端后端、控制端传输及会话；保留失败类别和可重跑条件。[CI 定义](https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/.github/workflows/ci.yml) · [Linux 实机脚本](https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tools/verify_bundled_linux.sh)

## #44 冻结后复查

1. 从 Python #44 的交接正文或所链接的仓内记录取**最终组合提交的完整 SHA**、与 main 的关系、包及审查证据、已验平台/宿主。若仍未给出最终 SHA，把 #21 的“Python 冻结差异”保留为待验。版本字符串、PR merge ref 和当日默认分支可能对应不同源码。[冻结票](https://github.com/xmxxc/workflow-core/issues/44)
2. 在上面的 Bash 会话执行下列命令；`frozen` 填交接 SHA。即使它与 #40 的祖先关系改变，也直接比较两个提交的**整棵树**，包含新增入口/配置、文档、fixture、打包和 CI。
3. 把每个变化定位到 CLI/协议/后端行，列出旧值→新值、兼容影响、涉及平台、实际对照用例和负责票。新能力新增行，删除/变更的能力标记最后有效版本。两棵树相同也记录 SHA、tree、空 diff、查到的包/实测差异。重跑变化的 handler、旧数据回归和受影响的真实路径；在同一集成下载件上取得 #21 结果，将差异链接回 #24。

```bash
# frozen 已由 #44 的固定交接记录赋值
test "${#frozen}" = 40
git -C "$trial/ref" fetch --no-tags https://github.com/xmxxc/workflow-core.git "$frozen"
git -C "$trial/ref" worktree add -q --detach "$trial/frozen" "$frozen"
mkdir -p "$trial/home-frozen"
git -C "$trial/frozen" rev-parse HEAD 'HEAD^{tree}'
git -C "$trial/ref" diff --find-renames --name-status "$p40" "$frozen"
git -C "$trial/ref" diff --stat "$p40" "$frozen"
inventory frozen > "$trial/frozen.inventory"
diff -u "$trial/40.inventory" "$trial/frozen.inventory" || test "$?" -eq 1
```

每条复查结果至少填下面一行。工作树/包 SHA、观察到的状态与身份、实际驱动的宿主/后端、skip 的前提和对应 issue 足以让接手人复验。

| Python 原 SHA → 冻结 SHA | 功能/行为变化 | 平台、实物/替身和环境条件 | Rust 工单、集成 SHA/包 SHA-256 | 命令、结果、证据 |
| --- | --- | --- | --- | --- |
| 60d7d76e → 等 #44 最终 SHA | 待按完整 diff 填写 | 待 #44 列明 | #21/#24，待同一候选 | 待冻结复验 |
