# 11 · Provider：proxy engine

优先级：P2 · 依赖：00、08、03、10

目标：实现 `engine: "proxy"`：拉起插件自带的命令，得到本地 openai 兼容端点后当
openai engine 用。只填 `src/provider/proxy.rs`；在 `10` 合并后把
`src/provider/registry.rs` 里 `proxy` 占位分支接上（本任务对 registry 的修改仅限该分支）。

验收：`cargo test` 过；端口替换、就绪轮询、超时、退出清理、失败重启有测试（假 server）。

计划参考：第三版 §2.4（proxy 行）、§7 风险 4、6。

## L1 · proxy.rs {#l1}

- 选空闲端口 → 拉起 `proxy.command`：替换 `args` 里的 `${PORT}`、设环境变量 `INSTAGENT_PORT`。
- 轮询 `GET http://127.0.0.1:{port}{proxy.ready}` 到 200（`ready` 默认 `/v1/models`，
  超时默认 20s）→ 之后作为 openai engine 指向本地端点。
- 会话结束 kill 子进程（用 `03` 的进程组机制）；连接失败自动重启一次。
- 环境变量透传白名单：只传 `PATH` `HOME` `LANG` + 插件声明的 `env`。

## L2 · 测试（四种异常，第三版 §7 风险 4） {#l2}

- 用约 10 行的假 openai 兼容 server（脚本 fixture）覆盖：
  1. 端口替换与就绪轮询（含慢就绪）；2. 就绪超时的可读错误；
  3. 退出时子进程被 kill 无残留；4. 中途崩溃后自动重启一次。
