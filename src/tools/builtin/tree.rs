//! tree 工具（第二版 §2.4）。
//!
//! 从 goose `developer/tree.rs`（commit `4ad43df`）的 `DirectoryNode` /
//! `collect_tree` / `render_into` / `count_file_lines` / `format_lines` 精简
//! 搬运：`CallToolResult` 换成 [`ToolOutput`]，schemars 参数换成手写 schema，
//! 路径解析走 `fs::resolve_path`；遍历用 `ignore` crate 遵守 .gitignore。

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Component;
use std::path::Path;

/// 单文件行数统计的读取上限，超限显示占位 `[?]`。
const MAX_COUNTED_BYTES: u64 = 10 * 1024 * 1024;

/// 超限哨兵：文件行数未知时用它，目录汇总 saturating 累加后整目录也降级 `[?]`。
const OVER_CAP: usize = usize::MAX;

use ignore::WalkBuilder;

use crate::tools::ToolCtx;
use crate::tools::ToolOutput;

use super::fs::resolve_path;

pub const DEFAULT_DEPTH: usize = 2;

/// 渲染 `path` 下最深 `depth` 层的目录树（depth 为 0 表示不限深度）。
pub async fn build_tree(path: &Path, depth: usize, ctx: &ToolCtx) -> ToolOutput {
    let root = resolve_path(path, &ctx.cwd);
    if !root.exists() {
        return ToolOutput::err(format!("Path does not exist: {}", root.display()));
    }
    if !root.is_dir() {
        return ToolOutput::err(format!("Path is not a directory: {}", root.display()));
    }

    let mut tree = collect_tree(&root, if depth == 0 { None } else { Some(depth) });
    tree.compute_total_lines();

    let mut output = String::new();
    tree.render_into(0, &mut output);
    if output.is_empty() {
        output.push_str("(empty directory)");
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

    fn render_into(&self, depth: usize, out: &mut String) {
        let indent = "  ".repeat(depth);

        for (name, dir) in &self.dirs {
            out.push_str(&format!(
                "{indent}{name}/  {}\n",
                format_lines(dir.total_lines)
            ));
            dir.render_into(depth + 1, out);
        }

        for (name, line_count) in &self.files {
            out.push_str(&format!("{indent}{name}  {}\n", format_lines(*line_count)));
        }
    }
}

fn collect_tree(root: &Path, max_depth: Option<usize>) -> DirectoryNode {
    let mut builder = WalkBuilder::new(root);
    builder.git_ignore(true);
    builder.git_exclude(true);
    builder.git_global(true);
    builder.require_git(false);
    builder.ignore(true);
    builder.hidden(true);

    if let Some(depth) = max_depth {
        builder.max_depth(Some(depth + 1));
    }

    let mut tree = DirectoryNode::default();
    for entry in builder.build().flatten() {
        let path = entry.path();
        if path == root {
            continue;
        }

        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };

        let Some(components) = relative_components(rel) else {
            continue;
        };

        if entry.file_type().is_some_and(|t| t.is_dir()) {
            tree.insert_dir(&components);
        } else if entry.file_type().is_some_and(|t| t.is_file()) {
            tree.insert_file(&components, count_file_lines(path));
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

/// 按字节块流式统计 `\n`，不整读文件；超过 `MAX_COUNTED_BYTES` 直接返回占位哨兵。
/// 行数语义与 `str::lines().count()` 一致（非空且末尾无换行符补 1）。
fn count_file_lines(path: &Path) -> usize {
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
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                bytes += n as u64;
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
}
