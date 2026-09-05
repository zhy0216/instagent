difficulty: medium

# 09 · Python、CI 与文档收口

优先级：P2。模型：`bailian-token-plan/qwen3.8-flash`。方案发现：E01–E03。

前置依赖：01-sse-stream-integrity、02-session-io-recovery、03-agent-turn-continuation、04-plugin-settings-recovery、05-proxy-lifecycle、06-bundled-snapshots、07-tool-inventory-io、08-cli-diagnostics-validation 全部合入。

## 涉及文件

- `tests/test_convert_providers.py`
- `scripts/ci.sh`
- `.github/workflows/ci.yml`
- `.gitignore`
- `README.md`
- `docs/usage.md`
- `docs/architecture.md`
- `docs/release.md`

## T1 · Python 回归纳入 CI 并清除句柄警告

要做：test_convert_providers.py 中直接 open().read/json.load(open()) 改成明确关闭句柄。ci.sh 与 CI 都运行 Python 测试，明确 Python 解释器设置；用 PYTHONDONTWRITEBYTECODE 防止子进程生成缓存，并精确忽略 `__pycache__/` 与 pyc。保持测试结果可见，ResourceWarning 不得靠全局屏蔽消失。

预计修改文件：`tests/test_convert_providers.py`、`scripts/ci.sh`、`.github/workflows/ci.yml`、`.gitignore`。

验收：11 个现有 Python 测试和全部新测试通过，warning 检查无未关闭句柄；本地脚本与 CI 命令对应；运行后工作区不因缓存变脏。不得安装新 Python 业务依赖或改 converter 输出契约。

前置依赖：无（执行本文件仍须等待全部任务合入）。

## T2 · 持续验证已声明 MSRV

要做：CI 增加固定 1.93.1 的 MSRV job，执行 `cargo +1.93.1 check --locked --all-targets`；主工具链仍由 rust-toolchain.toml 的 1.94.0 控制。不改 Cargo.toml、Cargo.lock 或 feature。保留现有 audit 的 schedule/PR 豁免政策。

预计修改文件：`.github/workflows/ci.yml`、`docs/release.md`。

验收：检查 YAML 与 job 命令，MSRV 本地验证通过；未安装时允许安装文档中已经采用的指定 toolchain，然后执行检查，不静默换成较新版本。如果前序代码不支持 MSRV，报告具体代码/编译错误给协调器，不提升 rust-version 掩盖失败。

前置依赖：本文件 T1；01–08。

## T3 · 文档反映最终代码与限制

要做：修正 README 的项目级 config.yaml 覆盖描述，与 usage 单用户层一致；architecture 补配置 extra plugins 发现层。更新 bundled 快照实际路径、warning 默认值、白名单启用/禁用、会话续轮/恢复预算、SSE 截断行为和 proxy 重试期限。release 保留历史端口竞态采样出处，追加 05 的本次验证及仍存在的端口交接限制。

预计修改文件：`README.md`、`docs/usage.md`、`docs/architecture.md`、`docs/release.md`。

验收：文档路径/配置键/CLI 参数与实现一致；README 的手动检查清单不再与自动覆盖状态或恢复行为冲突；不称全部依赖无漏洞、不宣称应用层 containment/完整图片解码/端口竞争被彻底根除。不修改历史计划和归档任务，不添加发布流程或 LICENSE 的新授权决定。

前置依赖：01–08，及本文件 T1/T2。

## 校验与完成

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py'
cargo +1.93.1 check --locked --all-targets
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo rustdoc --lib -- -D warnings
cargo check --release --all-targets
cargo run -q -- --help
```

最后运行更新后的 `bash scripts/ci.sh` 验证脚本接线（可用这次运行完成相同的 fmt/clippy/test/doc/release/help 检查，避免机械重复）。整理计划执行结果：真实完成项、未解决项、验证结果、最终队列状态。一个本地 commit，不发布或 push。

## 完成记录

状态：完成（单个本地 commit，不 push）。只改涉及文件内的 8 个文件，未动
`Cargo.toml` / `Cargo.lock` / `src/lib.rs` / converter 输出契约，未装新依赖。

- T1：`test_repeatable_runs`（2 处 `open().read()`）与
  `test_duplicate_definition_names_rejected`（`json.load(open())`）改显式
  `with open` 关闭句柄；`ci.sh` 新增 python 回归段
  （`PYTHONDONTWRITEBYTECODE=1 "${PYTHON:-python3}" -W error::ResourceWarning
  -m unittest discover -s tests -p 'test_*.py'`）；CI `check` job 新增
  `actions/setup-python@v5`（3.12）+ 同命令步骤；`.gitignore` 精确追加
  `__pycache__/` 与 `*.py[cod]`。
- T2：CI 新增 `msrv` job（ubuntu、固定 1.93.1，
  `cargo +1.93.1 check --locked --all-targets`，阻断）；audit 的
  schedule/PR 豁免原样保留；`release.md` 的 MSRV 证据行补 `--locked` 并
  记录 CI `msrv` job，主工具链仍 1.94.0。
- T3：README 修正项目级 config 覆盖描述（项目目录仅 settings，不读项目级
  config.yaml）、日志默认值（warning 到 stderr）、bundled 快照路径与开发
  命令；usage 更新 warning 默认、白名单 enable/disable 语义、会话续轮/
  读取预算（header 64 KiB / 单消息 96 MiB / 总文件 256 MiB）、bundled
  `v1-<fnv1a64>` 快照、新增 §9.5 SSE 预算与截断、proxy 总期限 + 额外 2 次
  重试 + TOCTOU 未根除；architecture 补配置 `plugins` 额外路径发现层
  （五层）与快照布局；release 保留历史采样并追加 05 的 24 run 0 失败验证
  及端口交接限制（RM06），措辞无新增漏洞/隔离/根除宣称。

验收证据：`PYTHONDONTWRITEBYTECODE=1 python3 -W error::ResourceWarning
-m unittest discover -s tests -p 'test_*.py'` 11 passed 且零 ResourceWarning、
无 `__pycache__` 残留；`cargo +1.93.1 check --locked --all-targets` 通过
（1.93.1 本机已装，未换新版）；`cargo fmt --check` / `cargo clippy
--all-targets -- -D warnings` 通过；`env -u TOKEN_PLAN_API_KEY cargo test`
全绿（lib 457 + 各集成 target，doc-tests 0）；`cargo check --release
--all-targets` 与 `cargo run -q -- --help` 通过；`ci.yml` 经 ruby YAML 解析
校验（jobs: check/msrv/audit）；更新后 `ci.sh` 接线验证：fmt/clippy/test/
python（11 OK）依次通过，止于 rustdoc（见未解决项）。

未解决项（`src/` 文件面之外，本任务按原约束未动手，需协调器裁决）：
1. 【协调器裁决后已收口修复】`cargo rustdoc --lib -- -D warnings` 在基线失败：
   `src/provider/proxy.rs:14` `[MAX_PORT_RETRIES]`、`src/session.rs:18,151`
   `[DEFAULT_LIMITS]`、`src/settings.rs:12` `[SETTINGS_FILE_MAX_BYTES]` 均为
   公开文档链私有项（分属 05/02/04 引入）。经授权做最小 doc-only 修复
   （4 行注释：`[…]` 链接改纯代码引号，零逻辑改动），rustdoc 已通过
   （EXIT=0），amend 进本任务唯一 commit；rebase 到 origin/main 为 no-op。
2. 全并行 `cargo test` 下 `tests/provider_proxy.rs` 的
   `cancelled_restart_candidate_is_reaped` 偶发失败 1 次（`left: 2, right: 1`），
   单测二进制重跑 15/15 通过、全量重跑全绿，属已知负载偶发模式（与历史记录一致）。
