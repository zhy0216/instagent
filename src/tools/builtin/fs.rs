//! read / write / edit 工具（第二版 §2.4）。
//!
//! edit 的匹配语义与相近上下文从 goose `developer/edit.rs:157~286`
//! （commit `4ad43df`）的 `string_replace` / `count_lines_before` /
//! `get_line_context` / `find_similar_context` / `build_file_preview`
//! 与 `resolve_path` 精简搬运（本文件标注处注明）。

use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;

use crate::tools::ToolCtx;
use crate::tools::ToolOutput;

/// read 默认最多 2000 行。
pub const READ_DEFAULT_LIMIT: u32 = 2000;

/// read 拒绝超过此字节数的文件。
pub const MAX_READ_BYTES: u64 = 32 * 1024 * 1024;

/// 无匹配时给的文件预览行数（goose edit.rs:8）。
const NO_MATCH_PREVIEW_LINES: usize = 20;

/// 相对路径按会话目录解析（搬运 goose developer/edit.rs:215 `resolve_path`）。
pub fn resolve_path(path: &Path, working_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    }
}

/// 带行号输出（read 标 read_only = true）。
///
/// 流式按行读取：只把窗口行留在内存，窗口之后的行仅计数，
/// 保证 "(N more lines)" 与旧行为一致而不整读文件。
pub async fn read_file(
    path: &Path,
    line: Option<u32>,
    limit: Option<u32>,
    ctx: &ToolCtx,
) -> ToolOutput {
    let full = resolve_path(path, &ctx.cwd);
    let fail =
        |e: std::io::Error| ToolOutput::err(format!("Failed to read {}: {e}", full.display()));

    if line == Some(0) {
        return ToolOutput::err(
            "`line` is 1-based, got line=0; use line=1 for the first line".to_string(),
        );
    }
    if limit == Some(0) {
        return ToolOutput::err(
            "`limit` must be positive, got limit=0; omit it for the default window".to_string(),
        );
    }

    let meta = match std::fs::metadata(&full) {
        Ok(meta) => meta,
        Err(e) => return fail(e),
    };
    if meta.len() > MAX_READ_BYTES {
        return ToolOutput::err(format!(
            "Failed to read {}: file too large ({} bytes, limit {} bytes); use shell tools (head/grep) to inspect it",
            full.display(),
            meta.len(),
            MAX_READ_BYTES
        ));
    }
    let file = match std::fs::File::open(&full) {
        Ok(file) => file,
        Err(e) => return fail(e),
    };

    let start = line.unwrap_or(1) as usize - 1;
    let count = limit.unwrap_or(READ_DEFAULT_LIMIT) as usize;
    let mut text = String::new();
    let mut total = 0usize;
    for (i, line) in BufReader::new(file).lines().enumerate() {
        total = i + 1;
        let file_line = match line {
            Ok(line) => line,
            Err(e) => return fail(e),
        };
        if i >= start && i - start < count {
            text.push_str(&format!("{:>4}: {}\n", i + 1, file_line));
        }
    }

    if total == 0 {
        return ToolOutput::ok(format!("{}: (file is empty)", full.display()));
    }
    if start >= total {
        return ToolOutput::err(format!(
            "Line {} is past end of file ({total} lines total)",
            start + 1
        ));
    }
    if total > start + count {
        text.push_str(&format!("... ({} more lines)\n", total - start - count));
    }
    ToolOutput::ok(text)
}

/// 写路径符号链接防护：目标是符号链接时拒绝（不跟随、不写穿）。
fn reject_symlink(full: &Path) -> Option<ToolOutput> {
    match std::fs::symlink_metadata(full) {
        Ok(meta) if meta.file_type().is_symlink() => Some(ToolOutput::err(format!(
            "Refusing to write through symlink: {} (write the link target directly instead)",
            full.display()
        ))),
        _ => None,
    }
}

/// 同目录临时文件 + rename 原子替换，失败路径清理临时文件。
/// 跨设备 rename 会失败，所以临时文件不放系统临时目录。
fn atomic_write(full: &Path, content: &str) -> std::io::Result<()> {
    let Some(name) = full.file_name() else {
        return std::fs::write(full, content);
    };
    let tmp = full.with_file_name(format!(
        "{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    match std::fs::write(&tmp, content).and_then(|()| std::fs::rename(&tmp, full)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 建父目录，原子覆盖写。
pub async fn write_file(path: &Path, content: &str, ctx: &ToolCtx) -> ToolOutput {
    let full = resolve_path(path, &ctx.cwd);
    if let Some(out) = reject_symlink(&full) {
        return out;
    }
    if let Some(parent) = full.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return ToolOutput::err(format!(
                "Failed to create parent directory {}: {e}",
                parent.display()
            ));
        }
    }
    match atomic_write(&full, content) {
        Ok(()) => ToolOutput::ok(format!(
            "Wrote {} bytes to {}",
            content.len(),
            full.display()
        )),
        Err(e) => ToolOutput::err(format!("Failed to write {}: {e}", full.display())),
    }
}

/// `before` 必须唯一精确匹配，否则报错并给出匹配次数和相近上下文；
/// `after` 为空即删除。写回走原子替换。
pub async fn edit_file(path: &Path, before: &str, after: &str, ctx: &ToolCtx) -> ToolOutput {
    let full = resolve_path(path, &ctx.cwd);
    if let Some(out) = reject_symlink(&full) {
        return out;
    }
    let content = match std::fs::read_to_string(&full) {
        Ok(content) => content,
        Err(e) => return ToolOutput::err(format!("Failed to read {}: {e}", full.display())),
    };

    match string_replace(&content, before, after) {
        Ok(new_content) => match atomic_write(&full, &new_content) {
            Ok(()) => {
                let old_lines = content.lines().count();
                let new_lines = new_content.lines().count();
                ToolOutput::ok(format!(
                    "Successfully replaced content in {} ({} lines -> {} lines)",
                    full.display(),
                    old_lines,
                    new_lines
                ))
            }
            Err(e) => ToolOutput::err(format!("Failed to write {}: {e}", full.display())),
        },
        Err(message) => ToolOutput::err(message),
    }
}

/// 唯一精确匹配才替换（搬运 goose developer/edit.rs:157 `string_replace`，
/// 加了 before 为空的显式拒绝）。
pub fn string_replace(content: &str, before: &str, after: &str) -> Result<String, String> {
    if before.is_empty() {
        return Err("`before` must not be empty (use write to create/replace files)".to_string());
    }

    let matches: Vec<_> = content.match_indices(before).collect();

    match matches.len() {
        0 => {
            let suggestion = find_similar_context(content, before);
            let mut msg = "No match found for the specified text.".to_string();
            if let Some(hint) = suggestion {
                msg.push_str(&format!("\n\nDid you mean:\n```\n{hint}\n```"));
            }
            let preview = build_file_preview(content, NO_MATCH_PREVIEW_LINES);
            msg.push_str(&format!("\n\nFile preview:\n```\n{preview}\n```"));
            Err(msg)
        }
        1 => Ok(content.replacen(before, after, 1)),
        n => {
            let mut msg = format!(
                "Found {n} matches. Please provide more context to identify a unique match:\n"
            );

            for (i, (pos, _)) in matches.iter().enumerate().take(2) {
                let line_num = count_lines_before(content, *pos);
                let context = get_line_context(content, line_num, 1);
                msg.push_str(&format!(
                    "\nMatch {} (line {line_num}):\n```\n{context}\n```",
                    i + 1
                ));
            }

            if n > 2 {
                msg.push_str(&format!("\n\n...and {} more", n - 2));
            }

            Err(msg)
        }
    }
}

/// 搬运 goose developer/edit.rs `count_lines_before`。
fn count_lines_before(content: &str, byte_pos: usize) -> usize {
    content
        .char_indices()
        .take_while(|(i, _)| *i < byte_pos)
        .filter(|(_, c)| *c == '\n')
        .count()
        + 1
}

/// 搬运 goose developer/edit.rs `get_line_context`。
fn get_line_context(content: &str, target_line: usize, context: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = target_line.saturating_sub(context + 1);
    let end = (target_line + context).min(lines.len());

    lines[start..end].join("\n")
}

/// 搬运 goose developer/edit.rs `find_similar_context`。
fn find_similar_context(content: &str, search: &str) -> Option<String> {
    let first_line = search.lines().next()?.trim();
    if first_line.is_empty() {
        return None;
    }

    for (i, line) in content.lines().enumerate() {
        if line.contains(first_line) || first_line.contains(line.trim()) {
            return Some(get_line_context(content, i + 1, 2));
        }
    }

    None
}

/// 搬运 goose developer/edit.rs `build_file_preview`。
fn build_file_preview(content: &str, max_lines: usize) -> String {
    if content.is_empty() {
        return "(file is empty)".to_string();
    }

    let lines: Vec<&str> = content.lines().collect();
    let preview_end = lines.len().min(max_lines);
    let mut preview = lines[..preview_end]
        .iter()
        .enumerate()
        .map(|(index, line)| format!("{:>4}: {}", index + 1, line))
        .collect::<Vec<_>>()
        .join("\n");

    if lines.len() > preview_end {
        preview.push_str(&format!("\n... ({} more lines)", lines.len() - preview_end));
    }

    preview
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
    async fn read_returns_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

        let out = read_file(Path::new("a.txt"), None, None, &ctx(dir.path())).await;
        assert!(!out.is_error);
        assert_eq!(out.text, "   1: alpha\n   2: beta\n   3: gamma\n");
    }

    #[tokio::test]
    async fn read_honors_line_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let content: String = (1..=10).map(|n| format!("line{n}\n")).collect();
        std::fs::write(&file, content).unwrap();

        let out = read_file(&file, Some(3), Some(2), &ctx(dir.path())).await;
        assert_eq!(out.text, "   3: line3\n   4: line4\n... (6 more lines)\n");

        let out = read_file(&file, Some(11), None, &ctx(dir.path())).await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn read_defaults_to_2000_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        let content: String = (1..=2500).map(|n| format!("{n}\n")).collect();
        std::fs::write(&file, content).unwrap();

        let out = read_file(&file, None, None, &ctx(dir.path())).await;
        assert!(out.text.contains("   1: 1\n"));
        assert!(out.text.contains(&format!("{:>4}: 2000\n", 2000)));
        assert!(!out.text.contains("   2001:"));
        assert!(out.text.contains("... (500 more lines)"));
    }

    #[tokio::test]
    async fn write_creates_parent_dirs_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let nested = Path::new("x/y/z.txt");

        let out = write_file(nested, "first", &ctx(dir.path())).await;
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x/y/z.txt")).unwrap(),
            "first"
        );

        let out = write_file(nested, "second", &ctx(dir.path())).await;
        assert!(!out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x/y/z.txt")).unwrap(),
            "second"
        );
    }

    #[tokio::test]
    async fn edit_unique_match_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();

        let out = edit_file(&file, "two", "TWO!", &ctx(dir.path())).await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "one\nTWO!\nthree\n"
        );
    }

    #[tokio::test]
    async fn edit_ambiguous_match_errors_with_count() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "dup\nx\ndup\ny\ndup\n").unwrap();

        let out = edit_file(&file, "dup", "new", &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("Found 3 matches"));
        assert!(out.text.contains("Match 1 (line 1)"));
        assert!(out.text.contains("Match 2 (line 3)"));
        assert!(out.text.contains("...and 1 more"));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "dup\nx\ndup\ny\ndup\n"
        );
    }

    #[tokio::test]
    async fn edit_no_match_gives_similar_context_and_preview() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello world\nsecond line\n").unwrap();

        let out = edit_file(&file, "hello world!", "x", &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("No match found"));
        assert!(out.text.contains("Did you mean:"));
        assert!(out.text.contains("File preview:"));
        assert!(out.text.contains("   1: hello world"));
    }

    #[tokio::test]
    async fn edit_empty_after_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "keep\ndrop\nkeep2\n").unwrap();

        let out = edit_file(&file, "drop\n", "", &ctx(dir.path())).await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep\nkeep2\n");
    }

    #[tokio::test]
    async fn edit_empty_before_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "abc\n").unwrap();

        let out = edit_file(&file, "", "x", &ctx(dir.path())).await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn write_atomic_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = write_file(Path::new("a.txt"), "content", &ctx(dir.path())).await;
        assert!(!out.is_error, "{}", out.text);
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["a.txt".to_string()]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_and_edit_fail_on_readonly_dir_without_touching_original() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("ro");
        std::fs::create_dir(&sub).unwrap();
        let file = sub.join("a.txt");
        std::fs::write(&file, "orig\n").unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o500)).unwrap();

        let out = write_file(&file, "new", &ctx(dir.path())).await;
        assert!(out.is_error);
        let out = edit_file(&file, "orig", "new", &ctx(dir.path())).await;
        assert!(out.is_error);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "orig\n");
        let leftovers: Vec<_> = std::fs::read_dir(&sub)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        assert_eq!(leftovers.len(), 1);

        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_and_edit_reject_symlink_targets() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        std::fs::write(&real, "secret\n").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let out = write_file(&link, "owned", &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("symlink"), "{}", out.text);
        let out = edit_file(&link, "secret", "owned", &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("symlink"), "{}", out.text);
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "secret\n");
    }

    #[tokio::test]
    async fn write_creates_new_file_through_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let out = write_file(Path::new("brand/new.txt"), "x", &ctx(dir.path())).await;
        assert!(!out.is_error, "{}", out.text);
    }

    #[tokio::test]
    async fn read_rejects_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("huge.bin");
        let f = std::fs::File::create(&file).unwrap();
        // 稀疏文件：metadata 报告超大长度，读取前就被字节上限拦截。
        f.set_len(MAX_READ_BYTES + 1).unwrap();

        let out = read_file(&file, None, None, &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("too large"), "{}", out.text);
    }

    #[tokio::test]
    async fn read_streams_window_of_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("many.txt");
        let content: String = (1..=50_000).map(|n| format!("line{n}\n")).collect();
        std::fs::write(&file, content).unwrap();

        let out = read_file(&file, Some(2), Some(3), &ctx(dir.path())).await;
        assert!(!out.is_error);
        assert_eq!(
            out.text,
            "   2: line2\n   3: line3\n   4: line4\n... (49996 more lines)\n"
        );
    }

    #[tokio::test]
    async fn read_rejects_zero_line_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x\n").unwrap();

        let out = read_file(&file, Some(0), None, &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("1-based"), "{}", out.text);

        let out = read_file(&file, None, Some(0), &ctx(dir.path())).await;
        assert!(out.is_error);
        assert!(out.text.contains("positive"), "{}", out.text);
    }

    #[test]
    fn resolve_path_joins_relative() {
        assert_eq!(
            resolve_path(Path::new("b/c"), Path::new("/a")),
            PathBuf::from("/a/b/c")
        );
        assert_eq!(
            resolve_path(Path::new("/x/y"), Path::new("/a")),
            PathBuf::from("/x/y")
        );
    }
}
