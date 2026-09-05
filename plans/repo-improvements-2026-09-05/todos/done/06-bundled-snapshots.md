difficulty: hard

# 06 · Bundled 完整快照

优先级：P1。模型：`bailian-token-plan/qwen3.8-max`。方案发现：I03。前置依赖：无。

## 涉及文件

- `src/plugin/bundled.rs`（实现与模块内测试）

## T1 · 按资源集合发布完整版本目录

要做：materialize_at 以 `<base>/bundled/` 为缓存父目录，生成与内嵌文件路径/内容关联的快照身份；在本次私有临时目录写完并核验后发布完整目录。返回的 Plugin.root 指向该快照。不能继续对运行中的共同根目录逐文件覆盖；不能只凭 manifest.version 判缓存有效，因为资源改变可能没有 bump 版本。

预计修改文件：`src/plugin/bundled.rs`。

验收：返回根的文件集合与嵌入集合一致，内容逐字节一致；旧 `<base>/bundled/dev.instagent/providers/ghost.json`、旧 hooks 和其他额外文件不会被新 Plugin.root 加载；正常 bundled provider/启用/优先级测试保持。只用当前依赖与标准库，缓存身份不作密码学承诺。

前置依赖：无。

## T2 · 并发、损坏缓存与失败回收

要做：并发发布相同快照时复用已经核验完成的目录；检查缓存缺失/多余/损坏文件，生成完整替代而不让调用者读取部分写入。失败只清理本次创建的临时目录，不删除旧快照、未知目录或正在使用的内容。旧布局可保留在缓存父目录，但不能参与运行时发现。

预计修改文件：`src/plugin/bundled.rs`。

验收：同一 base 上多线程/多进程 materialize 与 manifest/provider 读取始终成功；预置半份 JSON、额外文件、被修改的合法 JSON 均不能冒充正确快照；重复正常加载不重写有效文件；注入写入/发布失败后旧快照可读且不遗留本次 tmp。测试要核对具体文件集合，不只检查目录存在。

前置依赖：本文件 T1。

## 校验与完成

```bash
cargo test plugin::bundled
cargo test provider::registry::tests::bundled
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

一个本地 commit。最终目录布局写完成记录，README/usage/architecture 的同步交给 09；不修改 bundled provider 内容本身。

## 完成记录

状态：完成。只改 `src/plugin/bundled.rs`（实现 + 模块内测试），未动依赖、锁文件与模块树。

最终目录布局（`<base>/bundled/` 为缓存父目录）：

```text
<base>/bundled/
├── v1-<fnv1a64>/                   # 完整快照 = Plugin.root（身份 = 全部内嵌文件"路径+内容"的 FNV-1a 64，非密码学承诺）
│   ├── plugin.json
│   └── dev.instagent/providers/*.json
├── .staging-v1-<id>-<pid>-<uuid>/  # 私有临时目录：写满 + 读回核验后原子 rename 发布；失败即删
├── .retired-v1-<id>-<pid>-<uuid>/  # 被整体替换的损坏快照中转（替换成功后尽力删除）
└── <旧布局/未知目录>                 # 历史残留保留在盘，不参与运行时发现
```

关键行为：命中有效快照直接复用（零重写，mtime 不变）；损坏（缺失/多余/被改/半份/符号链接）整体替换——先 `.retired-*` 移走再换入，不原地覆盖在读目录；并发发布经 fast-path 复验 + rename 竞争，败者复用胜者并清理自己的 staging；失败只回收本次 `.staging-*`/`.retired-*`。

验收证据（`cargo test` lib 403 passed；cli_e2e 15、mcp_e2e 14、provider_proxy 7 全过；fmt/clippy 干净）：T1 由 `snapshot_matches_embedded_set_and_bytes`（集合 + 逐字节）与 `legacy_layout_files_remain_but_are_not_loaded`（旧 ghost/hooks 留盘但 registry 只见 deepseek/groq/ollama/openai/openrouter）覆盖；T2 由 `repeat_load_reuses_snapshot_without_rewriting`、`corrupted_snapshots_are_fully_replaced`（半份/额外/篡改三例）、`concurrent_materialize_publishes_one_snapshot`（8 线程 ×5 轮）、`multi_process_materialize_and_reads_succeed`（3 子进程进程组 + kill_on_drop + 父线程竞态）、`injected_failures_keep_old_snapshot_and_leave_no_tmp` 覆盖；原有 bundled 启用/优先级测试保持。
