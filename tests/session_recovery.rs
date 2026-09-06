//! 会话 IO 恢复集成测试（读取、写入预算与失败后恢复的端到端验收）。
//!
//! 只用公开 API（`Session` / `Message` / `Content`），临时目录 +
//! `INSTAGENT_DATA_DIR` 隔离；文件内串行（静态锁），不碰真实目录、不改
//! 真实权限、不用 `/dev/full`。小预算注入由 `src/session.rs` 模块内测试
//! 覆盖，这里不断言私有 helper。

use std::fs;
use std::path::PathBuf;

use instagent::message::Content;
use instagent::message::Message;
use instagent::message::Role;
use instagent::session::Session;
use instagent::session::SessionHeader;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Sandbox {
    _guard: std::sync::MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: 持有文件内静态锁期间独占进程 env，不与其他测试交错。
        unsafe {
            std::env::set_var("INSTAGENT_DATA_DIR", dir.path());
        }
        Self { _guard: guard, dir }
    }

    fn sessions_dir(&self) -> PathBuf {
        self.dir.path().join("sessions")
    }

    fn session_file(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{id}.jsonl"))
    }
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![Content::Text(text.into())],
        ts: 1,
        usage: None,
    }
}

fn assistant(text: &str) -> Message {
    Message::assistant(vec![Content::Text(text.into())], None)
}

fn text_of(message: &Message) -> &str {
    match &message.content[0] {
        Content::Text(text) => text,
        other => panic!("expected text content, got {other:?}"),
    }
}

fn hand_session(sandbox: &Sandbox, id: &str) -> Session {
    let header = SessionHeader {
        id: id.into(),
        created: 1,
        cwd: sandbox.dir.path().to_path_buf(),
        provider: "p".into(),
        model: "m".into(),
    };
    let path = sandbox.session_file(id);
    fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&header).unwrap()),
    )
    .unwrap();
    Session {
        header,
        messages: Vec::new(),
        path,
    }
}

fn snapshot_files(sandbox: &Sandbox) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fs::read_dir(sandbox.sessions_dir())
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let raw = fs::read(&path).unwrap();
            (path, raw)
        })
        .collect()
}

#[test]
fn default_header_write_budget_rejects_create_and_rewrite_then_allows_retry() {
    let sandbox = Sandbox::new();
    // 11 KiB 的 NUL 文本经 JSON 转义成为 66 KiB，超过默认 64 KiB header。
    let oversized = format!("private-budget-marker{}", "\0".repeat(11 * 1024));
    let err = Session::create(sandbox.dir.path(), &oversized, "m").unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("line 1") && err.contains("budget"), "{err}");
    assert!(!err.contains("private-budget-marker"), "{err}");
    assert!(Session::list().unwrap().is_empty());

    let mut session = Session::create(sandbox.dir.path(), "p", "m").unwrap();
    session.append(user("old")).unwrap();
    session.rewrite(vec![user("kept")]).unwrap();
    let old_header = session.header.clone();
    let old_messages = session.messages.clone();
    let before = snapshot_files(&sandbox);
    session.header.provider = oversized;
    let err = session.rewrite(vec![user("rejected")]).unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("line 1") && err.contains("budget"), "{err}");
    assert!(!err.contains("private-budget-marker"), "{err}");
    assert_eq!(snapshot_files(&sandbox), before);
    assert_eq!(session.messages, old_messages);
    let resumed = Session::resume(&session.header.id).unwrap();
    assert_eq!(resumed.header, old_header);
    assert_eq!(resumed.messages, old_messages);

    session.header = old_header;
    session.rewrite(vec![user("retry")]).unwrap();
    session.append(assistant("ok")).unwrap();
    let resumed = Session::resume(&session.header.id).unwrap();
    assert_eq!(resumed.header, session.header);
    assert_eq!(resumed.messages, session.messages);
}

#[test]
fn create_append_batch_rewrite_and_append_round_trip_escaped_utf8() {
    let sandbox = Sandbox::new();
    let text = "文本😀\"\\\n\r\t\0";
    let mut session = Session::create(sandbox.dir.path(), "provider-文本", "m").unwrap();
    session.append(user(text)).unwrap();
    session
        .append_batch(vec![
            Message::assistant(
                vec![Content::ToolUse {
                    id: "call".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"command": text}),
                }],
                None,
            ),
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "call".into(),
                    content: text.into(),
                    is_error: false,
                }],
                ts: 2,
                usage: None,
            },
            assistant(text),
        ])
        .unwrap();
    let old_raw = fs::read(&session.path).unwrap();
    session = Session::resume(&session.header.id).unwrap();
    assert_eq!(fs::read(&session.path).unwrap(), old_raw);
    assert_eq!(session.messages.len(), 4);
    assert_eq!(text_of(&session.messages[0]), text);

    session
        .rewrite(vec![user(text), assistant("summary accepted")])
        .unwrap();
    let backup = fs::read_dir(sandbox.sessions_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().ends_with(".bak.jsonl"))
        .unwrap();
    assert_eq!(fs::read(backup).unwrap(), old_raw);
    session
        .append_batch(vec![user(text), assistant(text)])
        .unwrap();
    let before = snapshot_files(&sandbox);
    let resumed = Session::resume(&session.header.id).unwrap();
    assert_eq!(resumed.header, session.header);
    assert_eq!(resumed.messages, session.messages);
    assert_eq!(snapshot_files(&sandbox), before);
}

// T1：含假密钥的 header 错误只带路径/行号/约束，不回显原文。
#[test]
fn header_error_is_bounded_and_hides_secret() {
    let sandbox = Sandbox::new();
    let mut session = Session::create(sandbox.dir.path(), "openai", "gpt-test").unwrap();
    session.append(user("hi")).unwrap();
    let fake_secret = "AKIAFAKESECRET123456";
    fs::write(
        &session.path,
        format!("{{\"id\":\"x\",\"token\":\"{fake_secret}\"\n"),
    )
    .unwrap();
    let err = format!("{:#}", Session::resume(&session.header.id).unwrap_err());
    assert!(err.contains("line 1"), "{err}");
    assert!(!err.contains(fake_secret), "{err}");
    assert!(!err.contains("token"), "{err}");
}

// T1：损坏正文恢复 → 追加合法 assistant → 再次恢复不丢新消息。
#[test]
fn corrupt_body_recovers_then_keeps_appended_message() {
    let sandbox = Sandbox::new();
    let mut session = Session::create(sandbox.dir.path(), "openai", "gpt-test").unwrap();
    session.append(user("hi")).unwrap();
    session.append(assistant("yo")).unwrap();
    let good_raw = fs::read_to_string(&session.path).unwrap();
    fs::write(&session.path, format!("{good_raw}garbage-not-json\n")).unwrap();

    let mut resumed = Session::resume(&session.header.id).unwrap();
    assert_eq!(resumed.messages.len(), 2);
    assert_eq!(text_of(&resumed.messages[1]), "yo");
    // 物理修复：坏行不再落盘。
    let raw = fs::read_to_string(&session.path).unwrap();
    assert!(!raw.contains("garbage"), "{raw}");

    resumed.append(user("after")).unwrap();
    let again = Session::resume(&session.header.id).unwrap();
    assert_eq!(again.messages.len(), 3);
    assert_eq!(text_of(&again.messages[2]), "after");
}

// T1：坏 UTF-8 尾部同样纳入恢复（旧实现直接读失败），追加后可再次恢复。
#[test]
fn corrupt_utf8_tail_repairs_and_stays_recoverable() {
    let sandbox = Sandbox::new();
    let mut session = Session::create(sandbox.dir.path(), "openai", "gpt-test").unwrap();
    session.append(user("hi")).unwrap();
    let good_raw = fs::read(&session.path).unwrap();
    let mut corrupt = good_raw.clone();
    corrupt.extend_from_slice(b"\xff\xfe broken\n");
    fs::write(&session.path, &corrupt).unwrap();

    let mut resumed = Session::resume(&session.header.id).unwrap();
    assert_eq!(resumed.messages.len(), 1);
    assert_eq!(fs::read(&session.path).unwrap(), good_raw);

    resumed.append(assistant("yo")).unwrap();
    let again = Session::resume(&session.header.id).unwrap();
    assert_eq!(again.messages.len(), 2);
    assert_eq!(text_of(&again.messages[1]), "yo");
}

// T1：无末尾换行的 header/消息被规范化，追加不粘连。
#[test]
fn unterminated_tail_is_normalized_before_append() {
    let sandbox = Sandbox::new();
    let session = Session::create(sandbox.dir.path(), "openai", "gpt-test").unwrap();
    fs::write(
        &session.path,
        serde_json::to_string(&session.header).unwrap(),
    )
    .unwrap();
    let mut resumed = Session::resume(&session.header.id).unwrap();
    assert!(resumed.messages.is_empty());
    assert!(fs::read_to_string(&session.path).unwrap().ends_with('\n'));
    resumed.append(user("first")).unwrap();
    let again = Session::resume(&session.header.id).unwrap();
    assert_eq!(again.messages.len(), 1);
    assert_eq!(text_of(&again.messages[0]), "first");
}

// T1：健康文件 resume 不改字节；图片会话可恢复。
#[test]
fn healthy_and_image_sessions_resume_intact() {
    let sandbox = Sandbox::new();
    let mut session = Session::create(sandbox.dir.path(), "openai", "gpt-test").unwrap();
    session.append(user("look")).unwrap();
    session.append(assistant("see")).unwrap();
    session
        .append(Message {
            role: Role::User,
            content: vec![Content::Image(instagent::tools::ImageData {
                data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
                    .into(),
                media_type: "image/png".into(),
            })],
            ts: 2,
            usage: None,
        })
        .unwrap();
    let before = fs::read(&session.path).unwrap();
    let resumed = Session::resume(&session.header.id).unwrap();
    assert_eq!(resumed.messages, session.messages);
    assert_eq!(fs::read(&session.path).unwrap(), before);
}

// T2：合法 batch（assistant + tool results）可恢复；非法 batch 零落盘。
#[test]
fn batch_commits_together_and_rejects_invalid_without_writes() {
    let sandbox = Sandbox::new();
    let mut session = Session::create(sandbox.dir.path(), "openai", "gpt-test").unwrap();
    session.append(user("run")).unwrap();
    session
        .append_batch(vec![
            Message::assistant(
                vec![
                    Content::ToolUse {
                        id: "a".into(),
                        name: "shell".into(),
                        input: serde_json::json!({}),
                    },
                    Content::ToolUse {
                        id: "b".into(),
                        name: "shell".into(),
                        input: serde_json::json!({}),
                    },
                ],
                None,
            ),
            Message {
                role: Role::User,
                content: vec![
                    Content::ToolResult {
                        tool_use_id: "a".into(),
                        content: "ok".into(),
                        is_error: false,
                    },
                    Content::ToolResult {
                        tool_use_id: "b".into(),
                        content: "ok".into(),
                        is_error: false,
                    },
                ],
                ts: 2,
                usage: None,
            },
        ])
        .unwrap();
    assert_eq!(session.messages.len(), 3);
    let resumed = Session::resume(&session.header.id).unwrap();
    assert_eq!(resumed.messages, session.messages);

    // 非法 batch：连续 assistant，零落盘、内存不变。
    let before = fs::read(&session.path).unwrap();
    let err = format!(
        "{:#}",
        session
            .append_batch(vec![assistant("x"), assistant("y")])
            .unwrap_err()
    );
    assert!(err.contains("invariant 1"), "{err}");
    assert_eq!(fs::read(&session.path).unwrap(), before);
    assert_eq!(session.messages.len(), 3);
}

// T2：主文件 symlink 拒绝追加，目标不被写，内存保持旧状态。
#[test]
fn append_refuses_symlinked_main_file() {
    let sandbox = Sandbox::new();
    let mut session = Session::create(sandbox.dir.path(), "openai", "gpt-test").unwrap();
    session.append(user("hi")).unwrap();
    let victim = sandbox.dir.path().join("victim.jsonl");
    fs::write(&victim, b"sentinel\n").unwrap();
    fs::remove_file(&session.path).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&victim, &session.path).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&victim, &session.path).unwrap();
    let err = session.append(assistant("x")).unwrap_err().to_string();
    assert!(err.contains("symlink"), "{err}");
    assert_eq!(fs::read_to_string(&victim).unwrap(), "sentinel\n");
    assert_eq!(session.messages.len(), 1);
}

// T2：文件/目录权限保持 0600/0700（unix）。
#[test]
#[cfg(unix)]
fn session_files_and_dirs_stay_private() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::new();
    let mut session = Session::create(sandbox.dir.path(), "openai", "gpt-test").unwrap();
    session.append(user("one")).unwrap();
    session.rewrite(vec![user("two")]).unwrap();
    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&sandbox.sessions_dir()), 0o700);
    assert_eq!(mode(&session.path), 0o600);
}

// T3：`a` 与 `a.b` 备份相互独立；成功重写无 tmp 残留。
#[test]
fn dotted_session_backups_stay_independent() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.sessions_dir()).unwrap();
    let mut a = hand_session(&sandbox, "a");
    let mut ab = hand_session(&sandbox, "a.b");
    for ts in 100..105 {
        fs::write(
            sandbox.sessions_dir().join(format!("a.{ts}.bak.jsonl")),
            b"old-a\n",
        )
        .unwrap();
        fs::write(
            sandbox.sessions_dir().join(format!("a.b.{ts}.bak.jsonl")),
            b"old-ab\n",
        )
        .unwrap();
    }
    for i in 0..3 {
        ab.rewrite(vec![user(&format!("ab{i}"))]).unwrap();
        a.rewrite(vec![user(&format!("a{i}"))]).unwrap();
    }
    let count = |id: &str| {
        fs::read_dir(sandbox.sessions_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| {
                n.strip_suffix(".bak.jsonl").is_some_and(|rest| {
                    rest.strip_prefix(id).is_some_and(|r| {
                        r.strip_prefix('.').is_some_and(|v| {
                            let (ts, counter) = match v.split_once('.') {
                                None => (v, None),
                                Some((t, n)) => (t, Some(n)),
                            };
                            !ts.is_empty()
                                && ts.bytes().all(|b| b.is_ascii_digit())
                                && counter.is_none_or(|n| {
                                    !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                                })
                        })
                    })
                })
            })
            .count()
    };
    assert_eq!(count("a"), 5);
    assert_eq!(count("a.b"), 5);
    assert_eq!(
        text_of(Session::resume("a").unwrap().messages.last().unwrap()),
        "a2"
    );
    assert_eq!(
        text_of(Session::resume("a.b").unwrap().messages.last().unwrap()),
        "ab2"
    );
    let leftovers: Vec<String> = fs::read_dir(sandbox.sessions_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".jsonl.tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}
