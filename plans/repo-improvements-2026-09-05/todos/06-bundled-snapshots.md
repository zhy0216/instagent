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
