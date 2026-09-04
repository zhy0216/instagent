//! tree 工具（第二版 §2.4）。
//!
//! 从 goose `developer/tree.rs`（commit `4ad43df`）的 `DirectoryNode` /
//! `collect_tree` / `render_into` / `count_file_lines` / `format_lines` 精简
//! 搬运：`CallToolResult` 换成 [`ToolOutput`]，schemars 参数换成手写 schema，
//! 路径解析走 `fs::resolve_path`；遍历用 `ignore` crate 遵守 .gitignore。
//!
//! 遍历预算（计划 R2 / todo 04）：entries、行数统计字节、输出字节、深度
//! 与时间五个上限，任一达到即停止并在输出末尾追加结构化
//! `... (truncated: ...)` note，而不是无界增长；`depth=0`（不限）也受
//! [`MAX_TREE_DEPTH`] 兜底。遍历在每个条目与每个读取块上检查取消令牌，
//! 取消时返回带已收集条目数的错误。阻塞逻辑经 [`super::fs::spawn_tool`]
//! 移出 current-thread async 路径（计划 R1）。

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Component;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

/// 单文件行数统计的读取上限，超限显示占位 `[?]`。
const MAX_COUNTED_BYTES: u64 = 10 * 1024 * 1024;

/// 超限哨兵：文件行数未知时用它，目录汇总 saturating 累加后整目录也降级 `[?]`。
const OVER_CAP: usize = usize::MAX;

/// 单次遍历最多收录的条目数（目录 + 文件）。
pub const MAX_TREE_ENTRIES: usize = 10_000;

/// 整趟遍历用于行数统计的累计读取字节上限；耗尽后剩余文件显示 `[?]`。
pub const MAX_TREE_COUNTED_BYTES: u64 = 64 * 1024 * 1024;

/// 渲染输出字节上限。
pub const MAX_TREE_OUTPUT_BYTES: usize = 512 * 1024;

/// `depth=0`（不限）或超深请求时的内部深度上限（防御无界嵌套）。
pub const MAX_TREE_DEPTH: usize = 64;

/// 单次遍历的时间预算。
pub const TREE_TIME_BUDGET: Duration = Duration::from_secs(30);

use ignore::WalkBuilder;
use tokio_util::sync::CancellationToken;

use crate::tools::ToolCtx;
use crate::tools::ToolOutput;

use super::fs::resolve_path;
use super::fs::spawn_tool;

pub const DEFAULT_DEPTH: usize = 2;

/// 一次遍历的资源预算；默认值即上面的公开常量，测试可注入小预算。
#[derive(Debug, Clone)]
struct TreeBudget {
    max_entries: usize,
    max_counted_bytes: u64,
    max_output_bytes: usize,
    max_depth: usize,
    time_budget: Duration,
}

impl Default for TreeBudget {
    fn default() -> Self {
        Self {
            max_entries: MAX_TREE_ENTRIES,
            max_counted_bytes: MAX_TREE_COUNTED_BYTES,
            max_output_bytes: MAX_TREE_OUTPUT_BYTES,
            max_depth: MAX_TREE_DEPTH,
            time_budget: TREE_TIME_BUDGET,
        }
    }
}

/// 渲染 `path` 下最深 `depth` 层的目录树。`depth=0` 语义上表示不限深度，
/// 但受 [`MAX_TREE_DEPTH`] 与各资源预算兜底；达到上限时输出末尾带
/// 结构化 `... (truncated: ...)` note。
pub async fn build_tree(path: &Path, depth: usize, ctx: &ToolCtx) -> ToolOutput {
    let root = resolve_path(path, &ctx.cwd);
    let cancel = ctx.cancel.clone();
    spawn_tool(move || build_tree_sync(&root, depth, TreeBudget::default(), &cancel)).await
}

fn build_tree_sync(
    root: &Path,
    depth: usize,
    budget: TreeBudget,
    cancel: &CancellationToken,
) -> ToolOutput {
    if !root.exists() {
        return ToolOutput::err(format!("Path does not exist: {}", root.display()));
    }
    if !root.is_dir() {
        return ToolOutput::err(format!("Path is not a directory: {}", root.display()));
    }

    // `ignore` 的 max_depth 按"条目深度 ≤ 限制"放行：请求深度 D 对应 D+1，
    // 与既有语义一致；depth=0 或超过内部上限时钳到预算深度。
    let (walk_max_depth, depth_limited) = if depth == 0 || depth > budget.max_depth {
        (budget.max_depth, true)
    } else {
        (depth + 1, false)
    };
    let output_limit = budget.max_output_bytes;
    let mut walker = Walker::new(budget, walk_max_depth, depth_limited, cancel.clone());
    let mut tree = collect_tree(root, &mut walker);

    if walker.cancelled {
        return ToolOutput::err(format!(
            "cancelled: tree of {} ({} entries collected before cancel)",
            root.display(),
            walker.entries
        ));
    }
    tree.compute_total_lines();

    let mut output = String::new();
    if !tree.render_into(0, &mut output, output_limit) {
        walker
            .notes
            .push(format!("output limit of {output_limit} bytes reached"));
    }
    if output.is_empty() {
        output.push_str("(empty directory)");
    }
    if walker.depth_limited && walker.saw_dir_at_depth_cap {
        walker.notes.push(format!(
            "depth limit: entries below level {} omitted",
            walker.walk_max_depth
        ));
    }
    if !walker.notes.is_empty() {
        output.push_str(&format!("\n... (truncated: {})", walker.notes.join("; ")));
    }
    ToolOutput::ok(output)
}

#[derive(Default)]
struct DirectoryNode {
    dirs: BTreeMap<String, DirectoryNode>,
    files: BTreeMap<String, usize>,
    total_lines: usize,
}

impl DirectoryNode {
    fn insert_dir(&mut self, components: &[String]) {
        let mut node = self;
        for component in components {
            node = node.dirs.entry(component.clone()).or_default();
        }
    }

    fn insert_file(&mut self, components: &[String], line_count: usize) {
        if components.is_empty() {
            return;
        }

        let mut node = self;
        for component in &components[..components.len() - 1] {
            node = node.dirs.entry(component.clone()).or_default();
        }

        let filename = components[components.len() - 1].clone();
        node.files.insert(filename, line_count);
    }

    fn compute_total_lines(&mut self) -> usize {
        let dir_lines: usize = self
            .dirs
            .values_mut()
            .map(DirectoryNode::compute_total_lines)
            .fold(0usize, usize::saturating_add);
        let file_lines: usize = self
            .files
            .values()
            .copied()
            .fold(0usize, usize::saturating_add);
        self.total_lines = dir_lines.saturating_add(file_lines);
        self.total_lines
    }

    /// 渲染到 `out`；达到 `limit` 字节即停，返回 false 表示被截断。
    fn render_into(&self, depth: usize, out: &mut String, limit: usize) -> bool {
        let indent = "  ".repeat(depth);

        for (name, dir) in &self.dirs {
            if out.len() >= limit {
                return false;
            }
            out.push_str(&format!(
                "{indent}{name}/  {}\n",
                format_lines(dir.total_lines)
            ));
            if !dir.render_into(depth + 1, out, limit) {
                return false;
            }
        }

        for (name, line_count) in &self.files {
            if out.len() >= limit {
                return false;
            }
            out.push_str(&format!("{indent}{name}  {}\n", format_lines(*line_count)));
        }
        true
    }
}

/// 遍历状态机：预算、累计量、截断 note 与取消标志。
struct Walker {
    budget: TreeBudget,
    /// 条目深度上限（`ignore` max_depth 语义：深度 ≤ 该值的条目被放行）。
    walk_max_depth: usize,
    /// 本次深度是否被预算钳制（请求 0 或超过内部上限）。
    depth_limited: bool,
    /// 是否在深度上限处见过目录（其子层被截断）。
    saw_dir_at_depth_cap: bool,
    notes: Vec<String>,
    entries: usize,
    counted_bytes: u64,
    counting_exhausted: bool,
    start: Instant,
    cancelled: bool,
    stopped: bool,
    cancel: CancellationToken,
}

impl Walker {
    fn new(
        budget: TreeBudget,
        walk_max_depth: usize,
        depth_limited: bool,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            budget,
            walk_max_depth,
            depth_limited,
            saw_dir_at_depth_cap: false,
            notes: Vec::new(),
            entries: 0,
            counted_bytes: 0,
            counting_exhausted: false,
            start: Instant::now(),
            cancelled: false,
            stopped: false,
            cancel,
        }
    }

    /// 每条目检查一次：取消 > 时间 > 条目数。返回 true 表示停止遍历。
    fn should_stop(&mut self) -> bool {
        if self.cancelled || self.stopped {
            return true;
        }
        if self.cancel.is_cancelled() {
            self.cancelled = true;
            return true;
        }
        if self.start.elapsed() > self.budget.time_budget {
            self.notes.push(format!(
                "time budget of {:?} exhausted after {} entries",
                self.budget.time_budget, self.entries
            ));
            self.stopped = true;
            return true;
        }
        if self.entries >= self.budget.max_entries {
            self.notes.push(format!(
                "entry limit of {} reached",
                self.budget.max_entries
            ));
            self.stopped = true;
            return true;
        }
        false
    }

    /// 按字节块流式统计 `\n`，不整读文件；超过单文件 [`MAX_COUNTED_BYTES`]
    /// 或整趟字节预算时返回占位哨兵。每个读取块都检查取消与时间预算，
    /// 保证取消延迟有上界（≤ 一个 16KB 块的读取时间）。
    /// 行数语义与 `str::lines().count()` 一致（非空且末尾无换行符补 1）。
    fn count_file_lines(&mut self, path: &Path) -> usize {
        if self.counting_exhausted {
            return OVER_CAP;
        }
        let Ok(meta) = fs::metadata(path) else {
            return 0;
        };
        if meta.len() > MAX_COUNTED_BYTES {
            return OVER_CAP;
        }
        let Ok(file) = fs::File::open(path) else {
            return 0;
        };
        let mut reader = std::io::BufReader::new(file);
        let mut buf = [0u8; 16 * 1024];
        let mut newlines = 0usize;
        let mut bytes = 0u64;
        let mut last = b'\n';
        loop {
            if self.cancel.is_cancelled() {
                self.cancelled = true;
                return 0;
            }
            if !self.stopped && self.start.elapsed() > self.budget.time_budget {
                self.notes.push(format!(
                    "time budget of {:?} exhausted after {} entries",
                    self.budget.time_budget, self.entries
                ));
                self.stopped = true;
                return 0;
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    bytes += n as u64;
                    self.counted_bytes = self.counted_bytes.saturating_add(n as u64);
                    if self.counted_bytes > self.budget.max_counted_bytes {
                        self.counting_exhausted = true;
                        self.notes.push(format!(
                            "counted-byte budget of {} bytes exhausted; remaining files shown as [?]",
                            self.budget.max_counted_bytes
                        ));
                        return OVER_CAP;
                    }
                    let chunk = &buf[..n];
                    newlines += chunk.iter().filter(|&&b| b == b'\n').count();
                    last = chunk[n - 1];
                }
                Err(_) => return 0,
            }
        }
        if bytes == 0 {
            0
        } else {
            newlines + usize::from(last != b'\n')
        }
    }
}

fn collect_tree(root: &Path, walker: &mut Walker) -> DirectoryNode {
    let mut builder = WalkBuilder::new(root);
    builder.git_ignore(true);
    builder.git_exclude(true);
    builder.git_global(true);
    builder.require_git(false);
    builder.ignore(true);
    builder.hidden(true);
    builder.max_depth(Some(walker.walk_max_depth));

    let mut tree = DirectoryNode::default();
    for entry in builder.build().flatten() {
        let path = entry.path();
        if path == root {
            continue;
        }
        if walker.should_stop() {
            break;
        }

        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };

        let Some(components) = relative_components(rel) else {
            continue;
        };

        if entry.file_type().is_some_and(|t| t.is_dir()) {
            if walker.depth_limited && components.len() == walker.walk_max_depth {
                walker.saw_dir_at_depth_cap = true;
            }
            tree.insert_dir(&components);
            walker.entries += 1;
        } else if entry.file_type().is_some_and(|t| t.is_file()) {
            let line_count = walker.count_file_lines(path);
            tree.insert_file(&components, line_count);
            walker.entries += 1;
        }
        if walker.cancelled || walker.stopped {
            break;
        }
    }

    tree
}

fn relative_components(path: &Path) -> Option<Vec<String>> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => components.push(value.to_string_lossy().into_owned()),
            _ => return None,
        }
    }

    if components.is_empty() {
        None
    } else {
        Some(components)
    }
}

fn format_lines(lines: usize) -> String {
    if lines == OVER_CAP {
        "[?]".to_string()
    } else if lines >= 1000 {
        format!("[{}K]", lines / 1000)
    } else {
        format!("[{lines}]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn tree_respects_gitignore_without_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "skip.txt\nskipped_dir/\n").unwrap();
        std::fs::write(dir.path().join("keep.txt"), "a\nb\nc\n").unwrap();
        std::fs::write(dir.path().join("skip.txt"), "hidden by gitignore\n").unwrap();
        std::fs::create_dir(dir.path().join("skipped_dir")).unwrap();
        std::fs::write(dir.path().join("skipped_dir/inner.txt"), "x\n").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        let out = build_tree(Path::new("."), DEFAULT_DEPTH, &ctx(dir.path())).await;
        assert!(!out.is_error);
        assert!(out.text.contains("keep.txt  [3]"));
        assert!(out.text.contains("src/"));
        assert!(out.text.contains("main.rs  [1]"));
        assert!(!out.text.contains("skip.txt"), "被忽略: {}", out.text);
        assert!(!out.text.contains("skipped_dir"), "被忽略: {}", out.text);
    }

    #[tokio::test]
    async fn tree_depth_limit_and_total_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        std::fs::write(dir.path().join("a/b/c/deep.txt"), "1\n2\n").unwrap();
        std::fs::write(dir.path().join("top.txt"), "1\n").unwrap();

        let limited = build_tree(dir.path(), 1, &ctx(dir.path())).await;
        assert!(!limited.is_error);
        assert!(limited.text.contains("a/  [0]"));
        assert!(limited.text.contains("top.txt  [1]"));
        assert!(
            !limited.text.contains("deep.txt"),
            "深度 1 不展示更深的文件: {}",
            limited.text
        );

        let full = build_tree(dir.path(), 0, &ctx(dir.path())).await;
        assert!(!full.is_error);
        assert!(
            full.text.contains("a/  [2]"),
            "目录汇总子孙行数: {}",
            full.text
        );
        assert!(full.text.contains("deep.txt  [2]"));
    }

    #[tokio::test]
    async fn tree_errors_on_missing_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = build_tree(Path::new("nope"), 2, &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("does not exist"));

        std::fs::write(dir.path().join("f.txt"), "x\n").unwrap();
        let out = build_tree(Path::new("f.txt"), 2, &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("not a directory"));
    }

    #[tokio::test]
    async fn tree_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let out = build_tree(Path::new("."), 2, &ctx(dir.path())).await;
        assert!(!out.is_error);
        assert_eq!(out.text, "(empty directory)");
    }

    #[tokio::test]
    async fn tree_marks_oversized_files_and_keeps_no_trailing_newline_semantics() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        std::fs::write(
            dir.path().join("d/big.bin"),
            vec![b'a'; MAX_COUNTED_BYTES as usize + 1],
        )
        .unwrap();
        std::fs::write(dir.path().join("no-newline.txt"), "a\nb").unwrap();

        let out = build_tree(Path::new("."), 2, &ctx(dir.path())).await;
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("big.bin  [?]"), "{}", out.text);
        assert!(
            out.text.contains("d/  [?]"),
            "超限文件应让所在目录降级: {}",
            out.text
        );
        assert!(out.text.contains("no-newline.txt  [2]"), "{}", out.text);
    }

    #[tokio::test]
    async fn tree_empty_file_counts_zero() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("e.txt"), "").unwrap();
        let out = build_tree(Path::new("."), 2, &ctx(dir.path())).await;
        assert!(out.text.contains("e.txt  [0]"), "{}", out.text);
    }

    /// 条目预算：超过 `max_entries` 即停，输出带结构化截断 note。
    #[tokio::test]
    async fn tree_entry_budget_truncates_with_note() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("f{i:02}.txt")), "x\n").unwrap();
        }
        let budget = TreeBudget {
            max_entries: 3,
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let out = build_tree_sync(dir.path(), 0, budget, &cancel);
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("(truncated:"), "{}", out.text);
        assert!(
            out.text.contains("entry limit of 3 reached"),
            "{}",
            out.text
        );
        // 只收录 3 个条目。
        let listed = out.text.matches(".txt").count();
        assert_eq!(listed, 3, "{}", out.text);
    }

    /// 行数统计字节预算：耗尽后剩余文件降级 `[?]`。
    #[tokio::test]
    async fn tree_counted_byte_budget_degrades_remaining() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\nb\nc\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "a\nb\nc\n").unwrap();
        let budget = TreeBudget {
            max_counted_bytes: 6,
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let out = build_tree_sync(dir.path(), 0, budget, &cancel);
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("counted-byte budget"), "{}", out.text);
        assert!(out.text.contains("[?]"), "{}", out.text);
    }

    /// 输出预算：渲染超限即截断并给出 note。
    #[tokio::test]
    async fn tree_output_budget_truncates_render() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            std::fs::write(dir.path().join(format!("file{i:02}.txt")), "x\n").unwrap();
        }
        let budget = TreeBudget {
            max_output_bytes: 40,
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let out = build_tree_sync(dir.path(), 0, budget, &cancel);
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.contains("output limit of 40 bytes reached"),
            "{}",
            out.text
        );
    }

    /// 时间预算：`Duration::ZERO` 下立即停止，证明遍历不会无限运行。
    #[tokio::test]
    async fn tree_time_budget_stops_traversal() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), "x\n").unwrap();
        }
        let budget = TreeBudget {
            time_budget: Duration::ZERO,
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let out = build_tree_sync(dir.path(), 0, budget, &cancel);
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("time budget"), "{}", out.text);
    }

    /// 深度预算：`depth=0` 也受 `max_depth` 兜底，超深处给出 note 并不再下探。
    #[tokio::test]
    async fn tree_depth_budget_caps_unlimited_depth() {
        let dir = tempfile::tempdir().unwrap();
        let mut chain = dir.path().to_path_buf();
        for i in 0..6 {
            chain = chain.join(format!("d{i}"));
        }
        std::fs::create_dir_all(&chain).unwrap();
        std::fs::write(chain.join("bottom.txt"), "x\n").unwrap();

        let budget = TreeBudget {
            max_depth: 2,
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let out = build_tree_sync(dir.path(), 0, budget, &cancel);
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("depth limit"), "{}", out.text);
        assert!(
            !out.text.contains("bottom.txt"),
            "超深文件不应出现: {}",
            out.text
        );
    }

    /// 集成：默认预算下 `depth=0` 对 66 层深目录有界（`MAX_TREE_DEPTH=64` 兜底）。
    #[tokio::test]
    async fn tree_depth_zero_is_bounded_by_default_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut chain = dir.path().to_path_buf();
        for i in 0..66 {
            chain = chain.join(format!("lvl{i:02}"));
        }
        std::fs::create_dir_all(&chain).unwrap();
        std::fs::write(chain.join("marker.txt"), "x\n").unwrap();

        let out = build_tree(dir.path(), 0, &ctx(dir.path())).await;
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("depth limit"), "{}", out.text);
        assert!(!out.text.contains("marker.txt"), "{}", out.text);
    }

    /// 预取消：立即返回带 `cancelled` 的可诊断错误（确定性）。
    #[tokio::test]
    async fn tree_precancelled_returns_cancelled_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            cancel: token,
        };
        let out = build_tree(Path::new("."), 2, &ctx).await;
        assert!(out.is_error);
        assert!(out.text.contains("cancelled"), "{}", out.text);
    }

    /// 取消上界：大目录中途取消，必须在可测上界内返回并记录耗时。
    #[tokio::test]
    async fn tree_cancellation_returns_within_bound() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..4000 {
            std::fs::write(dir.path().join(format!("f{i:04}.txt")), "x\n").unwrap();
        }
        let token = CancellationToken::new();
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            cancel: token.clone(),
        };
        let handle = tokio::spawn(async move { build_tree(Path::new("."), 0, &ctx).await });
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        token.cancel();
        let start = Instant::now();
        let out = handle.await.unwrap();
        let elapsed = start.elapsed();
        eprintln!("tree cancellation took {elapsed:?}");

        assert!(out.is_error);
        assert!(out.text.contains("cancelled"), "{}", out.text);
        assert!(
            elapsed < Duration::from_secs(5),
            "取消应在可测上界内生效: {elapsed:?}"
        );
    }

    /// symlink：tree 不跟随目录符号链接，链接目录里的文件不会逃出。
    #[cfg(unix)]
    #[tokio::test]
    async fn tree_does_not_follow_directory_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        std::fs::write(dir.path().join("keep.txt"), "x\n").unwrap();

        let out = build_tree(Path::new("."), 0, &ctx(dir.path())).await;
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("keep.txt"), "{}", out.text);
        assert!(
            !out.text.contains("secret.txt"),
            "不得跟随符号链接: {}",
            out.text
        );
    }
}
