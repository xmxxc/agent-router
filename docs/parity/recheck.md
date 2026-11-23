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
| router、backend | 真实 CLI 子进程/SQLite 与 fake tmux、宿主命令、屏幕、HTTP 或 FakeBackend 等；并发、状态、消息、所有权、重放、恢复与故障。有 Unix 专用夹具。 | #4–#18、#30/#34/#35/#36；按协议子表选具体类 |
| host_launch、platform | argv/环境、退出、收据和诊断，使用受控进程及平台分支/模拟；含真文件锁/进程和仅 Windows 可跑项 | #5/#8/#13/#14/#16，Windows #3/#10 |
| herdr、herdr_cleanup | Codex 输入框可模型化；HerdrContract 需固定外置服务+受控宿主；服务代际、同 pane/异 pane、断线、工作进程与后代清理，平台限定项另报 | 外置 Herdr #32、#12/#18 |
| bundled、bundled_guard、bundled_completion、bundled_protocol、bundled_fixture_input | 模拟收据、键输入、协议边界；BundledNative 需真实内核和本机隔离能力。Linux 运行在可写委派的 cgroup v2 中，Windows 采用 Job Object。比较签名收据、身份、执行树和活动 waiter；注明 Linux 专用信号/IO 压力与 Windows console 分支。 | Linux bundled #33、#5/#12/#13/#17/#18；Windows #3/#10 |
| bundled_experiment_package、distribution、wfc | 清单/文件哈希/署名、移动目录、Python 单文件入口及 Windows 启动器。Rust 包另验真实下载来源和无 Python 运行环境 | #2/#20/#21；Windows #3/#10 |
| foreground_evidence、tools 下 startup/desktop_evidence | 模拟窗口证据、Windows 启动与会话投影计数。真实 Windows Desktop 测试另需交互桌面和两条已登录聊天 | #10，Linux 侧在 #12/#17 复用“一个正式输入”的判据 |
| notify_controller | 历史 hook 的分类/投递回归；标准 wfc 打包入口中无此模块。Claude 控制端可走公开 wait；relay 公开目标见宿主表 | #21 记录接口边界 |

上游 CI 将常规替身、Windows core、真实 Herdr 配受控 worker 分开。Linux 整树回收的复验脚本为 `tools/verify_bundled_linux.sh REPO KERNEL LOG_DIR`，在独占的 `Delegate=yes` 测试单元执行，结束时检查本轮子 cgroup 并释放本轮单元。对真实宿主逐个注明 CLI/模型、已登录账号是否可用、终端后端、控制端传输及会话；保留失败类别和可重跑条件。[CI 定义](https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/.github/workflows/ci.yml) · [Linux 实机脚本](https://github.com/xmxxc/workflow-core/blob/60d7d76e4bb861ac9e0bbb043aa1a36946578397/tools/verify_bundled_linux.sh)

## 独立审查的可重复样例

审查对象是文档提交 `323fc448`。受控探针在独立状态中比较 #39、#40 与 PR #28，Rust 审查二进制自报 build `503f6cdd2b17e36be99743414c4f3efac4462ff3`，SHA-256 为 `18593fe4ff4e49855a8e06035b7ae95158fd15997c2f26475cbc0a05065ea897`。下列文件名对应独立报告的 `parity-review/evidence-r1/` 归档。两份 `spec-*.log` 的 SHA-256 均为 `714199a4223fa0c3d213d8619ba45475b6c4480bc3623df3ea46219604fe109a`。[独立审查摘要](https://github.com/xmxxc/agent-router/pull/29#issuecomment-7092264384)

| 复验输入 | 观察及归属 | 原始记录 |
| --- | --- | --- |
| 分派 A/B/C 到三个 scope，仅对 B 执行 owner acquire。无 token 对三项依次执行 reclaim --yes --dispatch A --dispatch B --dispatch C | 返回 1，stdout 为空，stderr 为 stale_controller_owner。A 已关闭，B/C 仍活，逐项 status 查实际进度。#36/#16/#21 | spec-probes.py、spec-pr39.log、spec-python.log |
| 受控 tmux 在 interrupt 的 send-keys 处返回失败。取 audit，以固定 actor/evidence 裁决 unknown，成功 handoff 到新 attempt 后原参调用 reconcile | 已裁决旧 audit 返回 0、duplicate=true，同一结果可重取。#18/#21 | 同上 |
| 在临时 SQLite 中写入 Herdr dispatch、有效 nonce 和服务身份，令受控 RPC 返回同一服务、absent=true、空 snapshot。依次不带、带旧写者停止证据进行 herdr-reconcile | 首次拒绝。第二次 accepted、inputReplayed=false，handle 持久保存 not-started 和空 writerProcesses。#32/#18/#21 | 同上 |
| 各用全新 HOME 和不同的调用 cwd，分别不设 WFC_STATE、设为空串、按字面传入 '~/custom'。Python 用空 wait --timeout 0 触发写入，Rust 用受控 dispatch 触发写入 | Python 三例均超时码 3，建库在 HOME/.local/state/workflow-core、同一默认、HOME/custom。Rust 三例分别在 HOME/.local/state/agent-router 写记录（0）、empty WFC_STATE（1，无记录）、在 cwd/~/custom 写记录（0）。#30/#4/#21 | discovery.py、discovery.log |
| 每例独立状态，分别构造普通文件、task 名或 cwd 名末尾含 0xff、缺失 task、task 是目录。用 Python 的 test_router.Env 记录 new-session，用 Rust 子进程写标记，再开 CLI 查询 | 普通路径两端启动。0xff 的两例均失败且无可见派发，诊断不同；缺失与目录两例的校验时点不同。详见[路径实测](python-cli.md#linux-派发路径实测)。#4/#21/#24/#26 | p3.py、p3.jsonl、task-check.py、extra-paths.jsonl |
| #40 的 HerdrContract.test_read_only_host_refuses_input 和两版的真实 tmux 缺任务试验 | Herdr 0.9.5 + 受控 worker 验只读拒绝及关闭，服务已回收。tmux 3.4 首派和 close 退出 1，status 有记录且 worker 已启动，已清理测试 server。#31/#32/#21 | herdr.log、tmux-observe.log |

复跑归档探针时，用独立的空 HOME，并令 SRC 指向上面检出的 #39 或 #40，RUST_WFC 指向已核对哈希的 #28 文件。在归档目录执行 `PYTHONPATH="$SRC:$SRC/tests" python3 -B spec-probes.py`，再分别执行 `python3 -B p3.py "$SRC" "$RUST_WFC"` 和 `python3 -B task-check.py "$SRC" "$RUST_WFC"`。discovery.py 包含审查机的绝对路径，在临时副本中将 p 和 rpath 指向含两个完整 SHA 检出目录的 trial 和该二进制；三种 WFC_STATE 输入由脚本逐例设置。

独立审查的协议组为 53/54，详见[协议组与单项超时](protocol.md#复验方法与证据层级)。宿主组两次均为 26/27，失败单项独跑通过，详见[宿主组与后端范围](hosts.md#复验取证)。归档分别为 protocol.log、hosts.log、hosts-retry.log 与两份 solo 日志。作者首轮的 54/54、27/27 保留为独立观察。两轮均未重跑 Python 全套、登录模型的 Linux 双聊天、Windows 或 macOS 原生流程。审查机器上的 cgroup v2 为只读，#33 的 Linux 委派清理及 #20/#21 的无 Python 下载件实测待完成。

## #44 冻结后复查

Python #44 已关闭。冻结产品 `60d7d76e4bb861ac9e0bbb043aa1a36946578397` 正是 #40，tree 为 `a401334e1525b236b654e30385f4ee216c2f9c5e`。相对交接时 main `3c06c0905faaf9e61de49404c33e70720409607f` 领先 7、落后 0。产品组合在 PR #45，文档 PR #46 的 `0ecbe94c95d79b4528d69323fd393181d719c920` 仅增加交接文档及其索引。独立审查时以 #40 做过同树演练；本次按 #44 指定的产品 SHA 复核整树，产品和验收项没有新增差异。[冻结回执](https://github.com/xmxxc/workflow-core/issues/44#issuecomment-7090349752) · [固定交接文档](https://github.com/xmxxc/workflow-core/blob/0ecbe94c95d79b4528d69323fd393181d719c920/docs/verification/python-router-closeout.zh-CN.md)

**Windows 上游证据。** 内层 ZIP SHA-256 为 `d9b9e609b5bd949694c51241fef5b4262c8754e8531ae4a12282fa18298f3adf`。Win11 在两个解压路径各跑 22 项通过、8 项跳过，并用下载件完成一次真实 Codex 单聊天。双聊天沿用与下载件逐件对照过的历史现场。产品 CI 8/8，两个独立审查均无产品阻塞。私有状态删除须查 cleanup.private_state_removed。[下载件与适用范围](https://github.com/xmxxc/workflow-core/issues/43#issuecomment-7090155489) · [产品审查](https://github.com/xmxxc/workflow-core/issues/42#issuecomment-7090084977)

**Linux 待验。** #21/#24 在可写委派的 cgroup v2、已登录 Codex 和两个真实聊天中核对六项：派发、原聊天唤醒、一次问答、接替、重复 ACK、运行时清理。A 先 acquire 并注册同 scope 的 relay，再派发。接替试验中 B takeover 后注册自己的 relay，确认 B waiter 出现且 A 已退出才放行原 worker。核对旧 A 的 ACK 被拒后仍为 received，B 首次 ACK 到 processed，重复 ACK 版本稳定。由当前 owner 关闭，核对签名收据、PID 与创建身份退出、waiter 为零、子 cgroup 移除。Rust 另在同一无 Python 下载件上进行这些测试。[六项通过条件](https://github.com/xmxxc/workflow-core/blob/0ecbe94c95d79b4528d69323fd393181d719c920/docs/verification/python-router-closeout.zh-CN.md)

在本页最前面创建的新临时目录运行以下命令。以后若上游另给产品 SHA，替换 frozen，将非空变化逐行归入 CLI、协议、宿主或旧数据表，并记录受影响的 Rust 工单和包哈希。

```bash
frozen=60d7d76e4bb861ac9e0bbb043aa1a36946578397
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
| #40 60d7d76e → #44 60d7d76e | 同一提交，整树及入口快照 diff 为空 | Windows 范围按上游下载件与双聊天记录。Linux 六项待真机 | #21/#24，待集成包 SHA-256 | 上述固定 SHA 的 git diff、inventory 对照；Rust 下载件证据待补 |
