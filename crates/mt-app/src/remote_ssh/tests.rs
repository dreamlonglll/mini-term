use std::collections::HashSet;
use std::path::PathBuf;

use mt_ai::sessions::AiSessionMessage;
use mt_ssh::sftp::SftpBoundedFileRead;

use super::*;

fn conn(id: &str) -> SshConnection {
    SshConnection {
        id: id.to_string(),
        name: format!("conn-{id}"),
        host: "h".into(),
        port: 22,
        user: "u".into(),
        password: None,
        identity_file: None,
        group: None,
    }
}

fn remote_baseline(conn: &SshConnection, bytes: &[u8]) -> RemoteFileBaseline {
    RemoteFileBaseline {
        connection_id: conn.id.clone(),
        connection_fingerprint: connection_fingerprint(conn),
        canonical_root: "/srv/project".into(),
        canonical_path: "/srv/project/src/main.rs".into(),
        bytes: Arc::from(bytes),
    }
}

// --- 断链查找 ---

#[test]
fn find_connection_hits_by_id() {
    let list = vec![conn("a"), conn("b")];
    assert_eq!(find_connection(&list, "b").unwrap().id, "b");
}

#[test]
fn find_connection_reports_broken_link_with_id() {
    let err = find_connection(&[conn("a")], "gone").unwrap_err();
    assert!(err.contains("gone"), "错误文案要带 id 便于排查: {err}");
    assert!(err.contains("SSH 连接不存在或已被删除"));
    // 空表同样是断链而不是 panic
    assert!(find_connection(&[], "x").is_err());
}

#[test]
fn remote_document_connection_fingerprint_tracks_endpoint_and_credentials() {
    let base = conn("a");
    let fingerprint = connection_fingerprint(&base);

    let mut changed = base.clone();
    changed.host = "other-host".into();
    assert_ne!(connection_fingerprint(&changed), fingerprint);
    changed = base.clone();
    changed.port = 2222;
    assert_ne!(connection_fingerprint(&changed), fingerprint);
    changed = base.clone();
    changed.user = "other-user".into();
    assert_ne!(connection_fingerprint(&changed), fingerprint);
    changed = base.clone();
    changed.password = Some("new-password".into());
    assert_ne!(connection_fingerprint(&changed), fingerprint);
    changed = base.clone();
    changed.identity_file = Some("/keys/new".into());
    assert_ne!(connection_fingerprint(&changed), fingerprint);

    // Display-only edits must not invalidate an otherwise identical remote
    // filesystem identity.
    changed = base.clone();
    changed.name = "renamed".into();
    changed.group = Some("other group".into());
    assert_eq!(connection_fingerprint(&changed), fingerprint);
}

#[test]
fn remote_document_read_classifies_text_binary_and_oversize() {
    let connection = conn("a");
    let text = build_remote_file_read_result(
        &connection,
        "/srv/project".into(),
        "/srv/project/notes.md".into(),
        SftpBoundedFileRead::Complete(b"# title\n".to_vec()),
    );
    assert_eq!(text.content.content, "# title\n");
    assert!(!text.content.is_binary);
    assert!(!text.content.too_large);
    assert_eq!(
        text.baseline.as_ref().map(|value| value.byte_len()),
        Some(8)
    );

    let binary = build_remote_file_read_result(
        &connection,
        "/srv/project".into(),
        "/srv/project/image.bin".into(),
        SftpBoundedFileRead::Complete(vec![0xff, 0xfe]),
    );
    assert!(binary.content.is_binary);
    assert!(!binary.content.too_large);
    assert!(binary.baseline.is_none());

    let oversize = build_remote_file_read_result(
        &connection,
        "/srv/project".into(),
        "/srv/project/large.txt".into(),
        SftpBoundedFileRead::TooLarge,
    );
    assert!(oversize.content.too_large);
    assert!(!oversize.content.is_binary);
    assert!(oversize.baseline.is_none());
}

#[test]
fn remote_document_save_conflict_requires_explicit_force() {
    let connection = conn("a");
    let baseline = remote_baseline(&connection, b"original");
    assert!(!should_block_remote_save(
        &SftpBoundedFileRead::Complete(b"original".to_vec()),
        &baseline,
        false
    ));
    assert!(should_block_remote_save(
        &SftpBoundedFileRead::Complete(b"changed".to_vec()),
        &baseline,
        false
    ));
    assert!(should_block_remote_save(
        &SftpBoundedFileRead::TooLarge,
        &baseline,
        false
    ));
    assert!(!should_block_remote_save(
        &SftpBoundedFileRead::Complete(b"changed".to_vec()),
        &baseline,
        true
    ));
    assert!(should_block_remote_save(
        &SftpBoundedFileRead::TooLarge,
        &baseline,
        true
    ));
}

#[test]
fn remote_document_baseline_rejects_connection_and_path_changes() {
    let connection = conn("a");
    let baseline = remote_baseline(&connection, b"original");
    assert!(validate_remote_file_baseline_connection(&connection, &baseline).is_ok());
    assert!(
        validate_remote_file_baseline_path(&baseline, "/srv/project", "/srv/project/src/main.rs")
            .is_ok()
    );

    let mut changed_connection = connection.clone();
    changed_connection.host = "new-host".into();
    assert!(validate_remote_file_baseline_connection(&changed_connection, &baseline).is_err());
    assert!(
        validate_remote_file_baseline_path(&baseline, "/srv/other", "/srv/other/src/main.rs")
            .is_err()
    );
    assert!(
        validate_remote_file_baseline_path(&baseline, "/srv/project", "/srv/project/src/other.rs")
            .is_err()
    );
}

// --- POSIX 路径拼接 / 相对化 ---

#[test]
fn join_posix_handles_root_and_trailing_slash() {
    assert_eq!(join_posix("/", "home"), "/home");
    assert_eq!(join_posix("/home/u", "proj"), "/home/u/proj");
    assert_eq!(join_posix("/home/u/", "proj"), "/home/u/proj");
}

#[test]
fn posix_relative_computes_relative_paths() {
    assert_eq!(
        posix_relative("/home/u/proj", "/home/u/proj/src/main.rs").as_deref(),
        Some("src/main.rs")
    );
    assert_eq!(
        posix_relative("/home/u/proj", "/home/u/proj").as_deref(),
        Some("")
    );
    // 尾部斜杠不影响
    assert_eq!(
        posix_relative("/home/u/proj/", "/home/u/proj/a").as_deref(),
        Some("a")
    );
    // 根目录项目
    assert_eq!(
        posix_relative("/", "/etc/hosts").as_deref(),
        Some("etc/hosts")
    );
}

#[test]
fn posix_relative_rejects_sibling_prefix() {
    // `/home/u/proj2` 不在 `/home/u/proj` 之下,不能误判
    assert!(posix_relative("/home/u/proj", "/home/u/proj2/file").is_none());
    assert!(posix_relative("/home/u/proj", "/other/place").is_none());
}

#[test]
fn parent_posix_handles_root_and_trailing_slashes() {
    assert_eq!(parent_posix("/home/u/project"), Some("/home/u".into()));
    assert_eq!(parent_posix("/home/u/project/"), Some("/home/u".into()));
    assert_eq!(parent_posix("/home"), Some("/".into()));
    assert_eq!(parent_posix("/"), None);
    assert_eq!(parent_posix(""), None);
}

#[test]
fn keep_both_names_preserve_extensions_and_dotfiles() {
    assert_eq!(keep_both_name("notes.txt", 1), "notes copy.txt");
    assert_eq!(keep_both_name("notes.txt", 2), "notes copy 2.txt");
    assert_eq!(keep_both_name("archive.tar.gz", 1), "archive.tar copy.gz");
    assert_eq!(keep_both_name("folder", 1), "folder copy");
    assert_eq!(keep_both_name(".env", 1), ".env copy");
}

#[test]
fn remote_path_validation_rejects_escape_and_host_separator_names() {
    assert_eq!(
        normalize_absolute_posix("/work/src/./main").unwrap(),
        "/work/src/main"
    );
    assert!(normalize_absolute_posix("/work/../etc").is_err());
    assert!(!valid_remote_name("a/b"));
    assert!(!valid_remote_name("a\\b"));
    assert!(!valid_remote_name("C:evil.exe"));
    assert!(!valid_remote_name("file:stream"));
    assert!(!valid_remote_name(".."));
}

#[test]
fn local_download_targets_stay_inside_root() {
    let root = if cfg!(windows) {
        PathBuf::from(r"C:\downloads")
    } else {
        PathBuf::from("/downloads")
    };
    let outside = if cfg!(windows) {
        PathBuf::from(r"D:\outside")
    } else {
        PathBuf::from("/outside")
    };

    assert_eq!(
        checked_local_download_child(&root, &root, "safe.txt").unwrap(),
        root.join("safe.txt")
    );
    assert!(checked_local_download_child(&root, &root, "C:evil.exe").is_err());
    assert!(checked_local_download_child(&root, &root, "a\\b").is_err());
    assert!(checked_local_download_child(&root, &outside, "safe.txt").is_err());
    assert!(ensure_local_download_target(&root, &outside).is_err());
    assert!(download_conflicts(&root, &[PathBuf::from("/remote/C:evil.exe")]).is_err());
}

#[test]
fn delete_child_validation_allows_remote_backslashes_but_rejects_separators() {
    assert!(valid_sftp_child_name("a\\b"));
    assert!(!valid_sftp_child_name("a/b"));
    assert!(!valid_sftp_child_name("."));
    assert!(!valid_sftp_child_name(".."));
    assert!(!valid_sftp_child_name("a\0b"));
}

#[test]
fn delete_shell_command_quotes_parent_and_leaf() {
    assert_eq!(shell_quote_posix("a'b"), "'a'\\''b'");
    let command = remote_delete_command(
        "/srv/project/a'b",
        "/srv/project/.proof'file",
        "nonce'value",
    )
    .unwrap();
    assert!(command.contains("cd -P '/srv/project'"));
    assert!(command.contains("[ ! -L './a'\\''b' ]"));
    assert!(command.contains("[ \"$(cat -- './.proof'\\''file')\" = 'nonce'\\''value' ]"));
    assert!(command.contains("rm -f -- './.proof'\\''file'"));
    assert!(command.contains("rm -rf -- './a'\\''b'"));
    assert!(!command.contains("rm -rf -- '/srv/project"));
}

// --- ~ 展开 ---

#[test]
fn expand_tilde_expands_home_forms() {
    assert_eq!(expand_tilde("~", "/home/u"), "/home/u");
    assert_eq!(expand_tilde("", "/home/u"), "/home/u");
    assert_eq!(expand_tilde("  ~  ", "/home/u"), "/home/u");
    assert_eq!(expand_tilde("~/proj", "/home/u"), "/home/u/proj");
    assert_eq!(expand_tilde("~/a/b", "/home/u/"), "/home/u/a/b");
    assert_eq!(expand_tilde("~/", "/home/u"), "/home/u");
}

#[test]
fn expand_tilde_leaves_absolute_and_other_paths_alone() {
    assert_eq!(expand_tilde("/var/www", "/home/u"), "/var/www");
    // `~user` 形式不支持展开,原样交给 canonicalize 报错
    assert_eq!(expand_tilde("~other/x", "/home/u"), "~other/x");
    assert_eq!(expand_tilde("relative/dir", "/home/u"), "relative/dir");
}

// --- 粘贴落盘目录解析(issue #36) ---

#[test]
fn resolve_paste_dir_defaults_to_project_relative() {
    // 默认形态:相对项目根,图片落在项目内
    assert_eq!(
        resolve_paste_dir("/home/u/proj", "/home/u", ".mini-term/pasted").unwrap(),
        "/home/u/proj/.mini-term/pasted"
    );
    // 空配置回落到默认值,而不是把文件丢到项目根
    assert_eq!(
        resolve_paste_dir("/home/u/proj", "/home/u", "   ").unwrap(),
        "/home/u/proj/.mini-term/pasted"
    );
    // 项目根带尾斜杠不产生双斜杠
    assert_eq!(
        resolve_paste_dir("/home/u/proj/", "/home/u", "assets").unwrap(),
        "/home/u/proj/assets"
    );
}

#[test]
fn resolve_paste_dir_default_matches_config_default() {
    // 本模块的默认值常量与 mt-config 的那份必须同值,
    // 否则「设置里清空 → 落盘目录」两侧会漂。
    assert_eq!(
        DEFAULT_REMOTE_PASTE_DIR,
        mt_config::default_remote_paste_dir()
    );
}

#[test]
fn resolve_paste_dir_supports_absolute_and_tilde() {
    assert_eq!(
        resolve_paste_dir("/home/u/proj", "/home/u", "/tmp/mini-term").unwrap(),
        "/tmp/mini-term"
    );
    assert_eq!(
        resolve_paste_dir("/home/u/proj", "/home/u", "~/uploads").unwrap(),
        "/home/u/uploads"
    );
    assert_eq!(
        resolve_paste_dir("/home/u/proj", "/home/u", "~").unwrap(),
        "/home/u"
    );
    // 尾斜杠被归一,避免拼出 `//file`
    assert_eq!(
        resolve_paste_dir("/home/u/proj", "/home/u", "/tmp/x/").unwrap(),
        "/tmp/x"
    );
}

#[test]
fn resolve_paste_dir_rejects_parent_traversal() {
    // 这条路径会拼进 SFTP 写操作,`..` 逃逸必须挡在解析层
    assert!(resolve_paste_dir("/home/u/proj", "/home/u", "../outside").is_err());
    assert!(resolve_paste_dir("/home/u/proj", "/home/u", "a/../../b").is_err());
    assert!(resolve_paste_dir("/home/u/proj", "/home/u", "/tmp/../etc").is_err());
    assert!(resolve_paste_dir("/home/u/proj", "/home/u", "~/../root").is_err());
    // 反斜杠写法先归一再判,不能绕过
    assert!(resolve_paste_dir("/home/u/proj", "/home/u", r"..\outside").is_err());
}

#[test]
fn resolve_paste_dir_rejects_traversal_from_project_path_too() {
    // `..` 也可能来自 project_path(调用方传入,非用户在设置页填的那半)。
    // 判定放在归一之后就是为了一处覆盖两个来源 —— 返回值恒不含 `..`。
    assert!(resolve_paste_dir("/home/u/../etc", "/home/u", "assets").is_err());
    assert!(resolve_paste_dir("/home/u/proj/..", "/home/u", ".mini-term").is_err());
    // home 带 `..` 的 `~` 展开同样挡住
    assert!(resolve_paste_dir("/home/u/proj", "/home/../root", "~/x").is_err());
}

#[test]
fn resolve_paste_dir_normalizes_dot_segments_and_double_slash() {
    // `.` 段必须被吃掉:否则 `/proj/.` 会被下游当成「严格位于项目内」,
    // 而它其实就是项目根 —— 自忽略 .gitignore 会写到仓库根,忽略整个仓库。
    assert_eq!(
        resolve_paste_dir("/home/u/proj", "/home/u", ".").unwrap(),
        "/home/u/proj"
    );
    assert_eq!(
        resolve_paste_dir("/home/u/proj", "/home/u", "./assets").unwrap(),
        "/home/u/proj/assets"
    );
    assert_eq!(
        resolve_paste_dir("/home/u/proj", "/home/u", "a//b").unwrap(),
        "/home/u/proj/a/b"
    );
    // 点开头的目录名不是 `.` 段,不能被误删
    assert_eq!(
        resolve_paste_dir("/home/u/proj", "/home/u", ".mini-term").unwrap(),
        "/home/u/proj/.mini-term"
    );
}

#[test]
fn paste_dir_at_project_root_is_not_strictly_inside() {
    // 自忽略 .gitignore 的守卫条件:rel 非空才写。
    // 解析成项目根本身时 rel 为空 —— 绝不能在仓库根写下内容为 `*` 的 .gitignore。
    let dir = resolve_paste_dir("/home/u/proj", "/home/u", ".").unwrap();
    assert_eq!(posix_relative("/home/u/proj", &dir).as_deref(), Some(""));

    // 默认形态才是「严格位于项目内」,应当写
    let nested = resolve_paste_dir("/home/u/proj", "/home/u", ".mini-term/pasted").unwrap();
    assert_eq!(
        posix_relative("/home/u/proj", &nested).as_deref(),
        Some(".mini-term/pasted")
    );

    // 项目外的绝对路径不参与 .gitignore 逻辑
    let outside = resolve_paste_dir("/home/u/proj", "/home/u", "/tmp/mini-term").unwrap();
    assert!(posix_relative("/home/u/proj", &outside).is_none());
}

#[test]
fn resolve_paste_dir_normalizes_backslash_input() {
    // 用户顺手填了 Windows 风格分隔符,不该原样拼进远端路径
    assert_eq!(
        resolve_paste_dir("/home/u/proj", "/home/u", r".mini-term\pasted").unwrap(),
        "/home/u/proj/.mini-term/pasted"
    );
}

#[test]
fn resolve_paste_dir_rejects_relative_project_root() {
    // 相对目录 + 非绝对项目根 = 拼不出合法远端路径,明确报错而不是拼个怪路径
    assert!(resolve_paste_dir("proj", "/home/u", "assets").is_err());
    // 但绝对 dest_dir 不依赖项目根,仍应通过
    assert!(resolve_paste_dir("proj", "/home/u", "/tmp/x").is_ok());
}

// --- 粘贴文件名提取 ---

#[test]
fn paste_file_name_strips_both_separators() {
    assert_eq!(
        paste_file_name(r"C:\Users\u\AppData\Local\Temp\clip-123.png").unwrap(),
        "clip-123.png"
    );
    assert_eq!(paste_file_name("/tmp/paste-9.txt").unwrap(), "paste-9.txt");
    // 混合分隔符:不能让 `\` 残留进远端路径
    assert_eq!(
        paste_file_name(r"C:/Temp\clip-1.png").unwrap(),
        "clip-1.png"
    );
}

#[test]
fn paste_file_name_rejects_degenerate_input() {
    assert!(paste_file_name("").is_err());
    assert!(paste_file_name(r"C:\Temp\").is_err());
    assert!(paste_file_name("/tmp/.").is_err());
    assert!(paste_file_name("..").is_err());
}

// --- 时间戳兜底 ---

#[test]
fn unix_secs_to_iso_known_values() {
    assert_eq!(unix_secs_to_iso(0), "1970-01-01T00:00:00Z");
    assert_eq!(unix_secs_to_iso(86_399), "1970-01-01T23:59:59Z");
    assert_eq!(unix_secs_to_iso(86_400), "1970-01-02T00:00:00Z");
    // 2000-03-01(闰年 2 月 29 日之后)
    assert_eq!(unix_secs_to_iso(951_868_800), "2000-03-01T00:00:00Z");
    // 2026-07-05T12:34:56Z
    assert_eq!(unix_secs_to_iso(1_783_254_896), "2026-07-05T12:34:56Z");
}

// --- 增量读取的完整行切分 ---

#[test]
fn split_complete_lines_cuts_at_last_newline() {
    let bytes = b"{\"a\":1}\n{\"b\":2}\n{\"partial";
    let (consumed, complete) = split_complete_lines(bytes);
    assert_eq!(consumed, 16);
    assert_eq!(complete, b"{\"a\":1}\n{\"b\":2}\n");
}

#[test]
fn split_complete_lines_no_newline_consumes_nothing() {
    let (consumed, complete) = split_complete_lines(b"half a line");
    assert_eq!(consumed, 0);
    assert!(complete.is_empty());
}

#[test]
fn split_complete_lines_empty_input() {
    let (consumed, complete) = split_complete_lines(b"");
    assert_eq!(consumed, 0);
    assert!(complete.is_empty());
}

// --- codex 文件名匹配 ---

#[test]
fn codex_filename_matches_session_by_suffix() {
    let p = "/home/u/.codex/sessions/2026/07/05/rollout-2026-07-05T10-00-00-abc-123.jsonl";
    assert!(codex_filename_matches_session(p, "abc-123"));
    assert!(!codex_filename_matches_session(p, "def-456"));
    // 空 id 永不匹配(防 ends_with("") 恒真)
    assert!(!codex_filename_matches_session(p, ""));
    // 非 .jsonl 不匹配
    assert!(!codex_filename_matches_session(
        "/x/rollout-abc-123.txt",
        "abc-123"
    ));
}

// --- session_index 解析 ---

#[test]
fn parse_codex_thread_names_extracts_pairs() {
    let content = "\
{\"id\":\"s1\",\"thread_name\":\"重构池\"}\n\
not json\n\
{\"id\":\"s2\"}\n\
{\"id\":\"s3\",\"thread_name\":\"fix bug\"}\n";
    let map = parse_codex_thread_names(content);
    assert_eq!(map.len(), 2);
    assert_eq!(map.get("s1").map(String::as_str), Some("重构池"));
    assert_eq!(map.get("s3").map(String::as_str), Some("fix bug"));
}

// --- state 基本行为(不触网) ---

#[test]
fn remote_state_caches_are_isolated_per_key() {
    let st = RemoteSshState::new();
    remember_session_path(&st, "c1", "s1", "/p/a.jsonl");
    remember_session_path(&st, "c2", "s1", "/p/b.jsonl");
    assert_eq!(
        lock(&st.session_paths).get("c1|s1").map(String::as_str),
        Some("/p/a.jsonl")
    );
    assert_eq!(
        lock(&st.session_paths).get("c2|s1").map(String::as_str),
        Some("/p/b.jsonl")
    );
}

#[test]
fn invalidate_connection_clears_only_that_connections_caches() {
    let st = RemoteSshState::new();
    remember_session_path(&st, "c1", "s1", "/p/a.jsonl");
    remember_session_path(&st, "c2", "s1", "/p/b.jsonl");
    lock(&st.home_cache).insert("c1".into(), "/home/u1".into());
    lock(&st.home_cache).insert("c2".into(), "/home/u2".into());
    lock(&st.gitignore_cache).insert(
        "c1|/home/u1/proj".into(),
        Arc::new(TextGitignore::from_text("target/\n")),
    );
    lock(&st.gitignore_cache).insert(
        "c2|/home/u2/proj".into(),
        Arc::new(TextGitignore::from_text("target/\n")),
    );

    st.invalidate_connection("c1");

    // c1 的三张缓存全清。
    assert!(lock(&st.session_paths).get("c1|s1").is_none());
    assert!(lock(&st.home_cache).get("c1").is_none());
    assert!(lock(&st.gitignore_cache).get("c1|/home/u1/proj").is_none());
    // c2 一条都不许被误伤(前缀匹配必须带上分隔符)。
    assert_eq!(
        lock(&st.session_paths).get("c2|s1").map(String::as_str),
        Some("/p/b.jsonl")
    );
    assert_eq!(
        lock(&st.home_cache).get("c2").map(String::as_str),
        Some("/home/u2")
    );
    assert!(lock(&st.gitignore_cache).get("c2|/home/u2/proj").is_some());
    // 池没建过 → 不该为了 evict 现建一个运行时。
    assert!(lock(&st.runtime).is_none(), "不该为了 evict 现建运行时");
}

#[test]
fn shutdown_without_pool_is_noop() {
    // 从未用过远程能力时退出:池与运行时都没建,不该起运行时、更不该 panic。
    let st = RemoteSshState::new();
    st.shutdown_pool_blocking();
    assert!(lock(&st.runtime).is_none(), "不该为了关池现建运行时");
}

#[test]
fn upload_conflicts_include_existing_and_duplicate_batch_names_once() {
    let existing = HashSet::from(["existing.txt".to_string()]);
    let paths = vec![
        PathBuf::from("first/existing.txt"),
        PathBuf::from("first/new.txt"),
        PathBuf::from("second/new.txt"),
        PathBuf::from("third/new.txt"),
        PathBuf::from("second/existing.txt"),
    ];

    assert_eq!(
        collect_upload_conflicts(&existing, &paths),
        vec!["existing.txt".to_string(), "new.txt".to_string()]
    );
}

#[test]
fn session_id_guard_rejects_traversal_before_touching_network() {
    // 非法 id 必须在开 SFTP 之前就被挡下(否则 `../` 会拼进远端路径)。
    // 这条不触网:守卫在函数第一行。
    let c = conn("c1");
    let err = ai_session_content(&c, "claude", "../etc/passwd", "/p", 0).unwrap_err();
    assert_eq!(err, "非法会话 id");
    let err2 = ai_session_content(&c, "claude", "a/b", "/p", 0).unwrap_err();
    assert_eq!(err2, "非法会话 id");
    // 全量入口共用同一道守卫
    let err3 = ai_session_content_all(&c, "claude", "../etc/passwd", "/p").unwrap_err();
    assert_eq!(err3, "非法会话 id");
}

// --- 会话正文续读循环 ---

fn msg(text: &str) -> AiSessionMessage {
    AiSessionMessage {
        role: "user".into(),
        content: text.into(),
        timestamp: String::new(),
    }
}

#[test]
fn accumulate_session_content_concatenates_until_exhausted() {
    // 三段:每段推进偏移,最后一段偏移不再前进(EOF)→ 拼接全部消息
    let chunks = [
        (vec![msg("a"), msg("b")], 10u64),
        (vec![msg("c")], 20u64),
        (vec![], 20u64),
    ];
    let mut calls: Vec<u64> = Vec::new();
    let mut i = 0usize;
    let out = accumulate_session_content(|offset| {
        calls.push(offset);
        let (messages, next_offset) = chunks[i].clone();
        i += 1;
        Ok(RemoteSessionContent {
            messages,
            next_offset,
        })
    })
    .unwrap();

    assert_eq!(
        calls,
        vec![0, 10, 20],
        "每轮都应带上上次的 next_offset 续读"
    );
    let texts: Vec<&str> = out.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(texts, vec!["a", "b", "c"]);
}

#[test]
fn accumulate_session_content_stops_when_offset_does_not_advance() {
    // consumed == 0(整段没有换行,单行 ≥ 8MB 的病态会话):next_offset 原地
    // 不动。必须**只调一次**就收尾,否则是死循环。
    let mut calls = 0usize;
    let out = accumulate_session_content(|offset| {
        calls += 1;
        assert!(calls < 5, "偏移不前进却仍在续读 —— 死循环");
        Ok(RemoteSessionContent {
            messages: vec![msg("partial")],
            next_offset: offset, // 一步没走
        })
    })
    .unwrap();

    assert_eq!(calls, 1, "偏移不前进应立即停");
    assert_eq!(out.len(), 1, "已解析到的内容要保留,不能连带丢掉");
}

#[test]
fn accumulate_session_content_caps_total_bytes() {
    // 每轮都「读满」一整块:撞到总量护栏就停,不会无限吃内存
    let mut calls = 0usize;
    let out = accumulate_session_content(|offset| {
        calls += 1;
        assert!(calls < 1000, "总量护栏没生效");
        Ok(RemoteSessionContent {
            messages: vec![msg("chunk")],
            next_offset: offset + CONTENT_CHUNK_MAX_BYTES as u64,
        })
    })
    .unwrap();

    let expected = (CONTENT_TOTAL_MAX_BYTES / CONTENT_CHUNK_MAX_BYTES as u64) as usize;
    assert_eq!(calls, expected, "读满 64 MB 即止");
    assert_eq!(out.len(), expected);
}

#[test]
fn accumulate_session_content_error_policy() {
    // 首段就失败 → 报错(用户看得到原因)
    let err = accumulate_session_content(|_| Err("boom".to_string())).unwrap_err();
    assert_eq!(err, "boom");

    // 后续段失败 → 按截断处理,保留已拿到的内容
    let mut calls = 0usize;
    let out = accumulate_session_content(|offset| {
        calls += 1;
        if calls == 1 {
            Ok(RemoteSessionContent {
                messages: vec![msg("first")],
                next_offset: offset + 8,
            })
        } else {
            Err("网络断了".to_string())
        }
    })
    .unwrap();
    assert_eq!(out.len(), 1);
}
