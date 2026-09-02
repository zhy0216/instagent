# 19 · 加固：clippy、错误路径、README、CI 收口

优先级：P4 · 依赖：全部（`12` anthropic 除外，可选任务不阻塞本项）

目标：整仓加固与文档收口。可触碰任意文件，但只做修正与文档，不加新功能。

验收：`cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` 全绿
（含 `--features anthropic-engine` 若 12 已合并）；四种错误路径提示可读；
`bash scripts/ci.sh`（或 CI workflow）本地可复现。

计划参考：第三版 §5 P7；第二版 §5 P6。

## T1 · 静态检查零警告 {#t1}

- `cargo fmt` 全仓；`cargo clippy --all-targets -- -D warnings` 零警告（含测试代码）。

## T2 · 错误路径走查 {#t2}

四种情况提示可读、进程无残留（第三版 §5 P7），每种补一个测试或验证脚本：

1. 启用的插件目录被删 → 启动警告并跳过，不 panic；
2. MCP server 起不来（命令不存在）→ 错误指出插件与 server 名；
3. proxy 就绪超时 → 错误含 ready 路径与超时秒数；
4. provider 重名 / 不存在 → 提示用 `plugin/name` 或列出可用项。

## T3 · 残留检查 {#t3}

- shell 超时、MCP 卡住、proxy 退出三种场景确认无子进程残留（进程组 + kill_on_drop 全覆盖）。

## T4 · README 与 CI {#t4}

- `README.md`：安装构建、快速上手（配置示例）、插件开发指南（用第三版 §8 的完整示例目录）、
  Agent Plugins 规范链接、命名对照说明。示例命令必须真实可跑。
- `.github/workflows/ci.yml` 收口（`00` 已建）：fmt → clippy -D warnings → test
  （12 已合并时加 `--features anthropic-engine` 步骤）。
