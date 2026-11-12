use super::*;

#[test]
fn 远程刷新失败仅在没有已加载内容时进入致命错误页() {
    assert_eq!(
        remote_refresh_failure_presentation(false, false),
        RemoteRefreshFailurePresentation::Fatal
    );
    assert_eq!(
        remote_refresh_failure_presentation(true, false),
        RemoteRefreshFailurePresentation::Warning
    );
    assert_eq!(
        remote_refresh_failure_presentation(false, true),
        RemoteRefreshFailurePresentation::Warning
    );
}

#[test]
fn 远程保存只有成功才清除刷新警告() {
    let warning = Some("refresh failed".to_string());
    assert_eq!(
        refresh_warning_after_remote_save(warning.clone(), false),
        warning
    );
    assert_eq!(
        refresh_warning_after_remote_save(Some("refresh failed".to_string()), true),
        None
    );
}

/// **本批的钉子测试**:CRLF 文件改一个字保存,行尾一个都不许变。
#[test]
fn crlf_文件往返不改行尾() {
    let disk = "line1\r\nline2\r\nline3\r\n";
    assert_eq!(LineEnding::detect(disk), LineEnding::Crlf);

    // 读入:归一成 \n 喂编辑器
    let in_editor = normalize_to_lf(disk);
    assert_eq!(in_editor, "line1\nline2\nline3\n");
    assert!(!in_editor.contains('\r'), "编辑器里不留 \\r");

    // 编辑:改一个字 + 敲一次回车(gpui-component 插的是 "\n")
    let edited = in_editor.replace("line2", "LINE2") + "line4\n";

    // 写回:还原成 CRLF —— 新增的那一行也是 CRLF
    let back = restore_line_ending(&edited, LineEnding::Crlf);
    assert_eq!(back, "line1\r\nLINE2\r\nline3\r\nline4\r\n");
    assert_eq!(back.matches('\n').count(), back.matches("\r\n").count());
}

#[test]
fn lf_文件不会被写成_crlf() {
    let disk = "a\nb\n";
    assert_eq!(LineEnding::detect(disk), LineEnding::Lf);
    let in_editor = normalize_to_lf(disk);
    assert_eq!(in_editor, disk);
    assert_eq!(restore_line_ending(&in_editor, LineEnding::Lf), disk);
    // 空文件 / 无换行的单行文件都算 LF
    assert_eq!(LineEnding::detect(""), LineEnding::Lf);
    assert_eq!(LineEnding::detect("no newline"), LineEnding::Lf);
}

#[test]
fn 行尾还原是幂等的() {
    // 万一有 \r\n 混进编辑器,还原两次也不该变成 \r\r\n
    let once = restore_line_ending("a\r\nb", LineEnding::Crlf);
    let twice = restore_line_ending(&once, LineEnding::Crlf);
    assert_eq!(once, "a\r\nb");
    assert_eq!(twice, once);
}

/// 保存路径语义:走 `mt_project::fs::write_file_content`(内部原子写),
/// 且 CRLF 文件读→改→写一整圈之后磁盘字节里的行尾一个都没变。
#[test]
fn 保存走原子写且_crlf_全程不变() {
    let dir = std::env::temp_dir().join(format!("mt-fv-save-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("crlf.txt");
    std::fs::write(&file, b"alpha\r\nbeta\r\n").unwrap();

    // 读:后端给的是原文(带 \r\n)
    let res = mt_project::fs::read_file_content(&dir, &file).unwrap();
    assert!(
        res.content.contains("\r\n"),
        "后端不做行尾归一,归一在 UI 侧"
    );
    let ending = LineEnding::detect(&res.content);
    let editor_text = normalize_to_lf(&res.content);

    // 改 + 敲回车
    let edited = editor_text.replace("beta", "BETA") + "gamma\n";

    // 写
    mt_project::fs::write_file_content(&dir, &file, &restore_line_ending(&edited, ending)).unwrap();

    let on_disk = std::fs::read(&file).unwrap();
    assert_eq!(on_disk, b"alpha\r\nBETA\r\ngamma\r\n");
    // 原子写不留临时文件
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "原子写的临时文件必须已经被 rename 掉");

    let _ = std::fs::remove_dir_all(&dir);
}

// ─── DocumentSession:模块注释那张状态转换表逐行锁住 ─────────────

/// 假远程内容:基线用一个数字代表 —— 真基线出了 `mt_remote` 造不出来(见
/// [`RemoteContent`])。
#[derive(Debug, Clone)]
struct FakeRemote {
    content: FileContentResult,
    baseline: Option<u32>,
}

impl RemoteContent for FakeRemote {
    type Baseline = u32;

    fn content(&self) -> &FileContentResult {
        &self.content
    }

    fn into_parts(self) -> (FileContentResult, Option<u32>) {
        (self.content, self.baseline)
    }
}

type Session = DocumentSession<FakeRemote>;

fn text(content: &str) -> FileContentResult {
    FileContentResult::text(content.to_string(), TextEncoding::Utf8)
}

fn remote(content: &str, baseline: u32) -> FakeRemote {
    FakeRemote {
        content: text(content),
        baseline: Some(baseline),
    }
}

/// 走完一次「发起载入 → 结果对上代次」,停在落内容之前。
fn settled_load(doc: &mut Session) {
    let Some(LoadStart::Read { generation }) = doc.begin_load(false) else {
        panic!("文本文档应去读盘");
    };
    assert!(doc.settle_load(generation));
}

fn loaded_local(content: &str) -> Session {
    let mut doc = Session::new();
    settled_load(&mut doc);
    doc.apply_local_content(text(content));
    doc
}

fn loaded_remote(content: &str, baseline: u32) -> Session {
    let mut doc = Session::new();
    settled_load(&mut doc);
    assert!(
        doc.apply_remote_content(remote(content, baseline))
            .is_some()
    );
    doc
}

/// 保存前两步走完(视图在两步之间复核连接身份),拿到落盘起点。
fn start_save(doc: &mut Session, draft: &str) -> SaveStart {
    assert!(
        doc.prepare_save(draft),
        "草稿与基线不同且不在保存中,应开始保存"
    );
    doc.begin_save(draft).expect("连接身份有效时应开始保存")
}

/// 挂一条刷新警告(有内容时刷新失败只挂警告)。
fn with_refresh_warning(doc: &mut Session) {
    let generation = doc.begin_remote_refresh();
    assert!(doc.settle_remote_refresh(generation));
    let shown = doc.on_refresh_failed("刷新失败".into(), true);
    assert_eq!(shown, RemoteRefreshFailurePresentation::Warning);
    assert_eq!(doc.refresh_warning(), Some("刷新失败"));
}

#[test]
fn 会话_载入_代次递增并清掉上一代的远程基线_冲突与刷新警告() {
    let mut doc = loaded_remote("a", 1);
    // 刷新途中开始打字 → 挂冲突;再来一次刷新失败 → 挂刷新警告
    doc.on_edit("a!");
    let generation = doc.begin_remote_refresh();
    assert!(doc.settle_remote_refresh(generation));
    assert_eq!(
        doc.on_refresh_loaded(remote("b", 2), true),
        RefreshLoaded::Conflict
    );
    with_refresh_warning(&mut doc);
    assert!(doc.has_remote_conflict());

    let Some(LoadStart::Read { .. }) = doc.begin_load(false) else {
        panic!("文本文档应去读盘");
    };
    assert!(doc.loading());
    assert!(doc.result().is_none());
    assert!(doc.error().is_none());
    assert!(doc.refresh_warning().is_none());
    assert!(doc.remote_baseline().is_none());
    assert!(!doc.has_remote_conflict());

    // 在飞的激活刷新也被这次载入顶掉:迟到的结果对不上代次,刷新中标记已撤
    let mut doc = loaded_remote("a", 1);
    let refresh = doc.begin_remote_refresh();
    let Some(LoadStart::Read { generation }) = doc.begin_load(false) else {
        panic!("文本文档应去读盘");
    };
    assert_eq!(generation, refresh + 1, "每次载入代次 +1");
    assert!(!doc.settle_remote_refresh(refresh));
    assert!(doc.settle_load(generation));
    assert!(doc.apply_remote_content(remote("a", 2)).is_some());
    assert!(doc.should_refresh_on_activation(true, false));
}

#[test]
fn 会话_载入_保存中不重建() {
    let mut doc = loaded_local("a");
    let save = start_save(&mut doc, "b");
    assert_eq!(doc.begin_load(false), None, "旧写入还没收口,不许重建");
    assert!(doc.is_saving());
    assert_eq!(doc.saved(), "a");
    // 代次没动:迟到的写盘结果照样对得上
    assert!(doc.settle_save(save.generation));
}

#[test]
fn 会话_载入_看图不读盘_也不清错误() {
    let mut doc = Session::new();
    settled_load(&mut doc);
    doc.fail_load("读不到".into());
    assert_eq!(doc.begin_load(true), Some(LoadStart::Image));
    assert!(!doc.loading(), "看图页签不读盘,没有载入态");
    assert!(doc.result().is_none());
    // 原样照抄:看图分支不清 error(渲染分支里图片先于错误,不影响显示)
    assert_eq!(doc.error(), Some("读不到"));
}

#[test]
fn 会话_载入_过期结果整条丢弃_失败落错误() {
    let mut doc = Session::new();
    let Some(LoadStart::Read { generation: first }) = doc.begin_load(false) else {
        panic!();
    };
    let Some(LoadStart::Read { generation: second }) = doc.begin_load(false) else {
        panic!();
    };
    assert!(!doc.settle_load(first), "被新一轮载入作废的结果不收");
    assert!(doc.loading(), "丢弃的结果不结束载入态");
    assert!(doc.settle_load(second));
    assert!(!doc.loading());
    doc.fail_load("拒绝访问".into());
    assert_eq!(doc.error(), Some("拒绝访问"));
}

#[test]
fn 会话_落基线_归一行尾展开_tab_并清脏与各类提示() {
    let mut doc = loaded_local("a");
    // 先弄出脏、外部改动、保存错误三样
    doc.on_edit("a!");
    assert_eq!(
        doc.on_fs_change(Instant::now(), || Some("a!".into())),
        FsChange::Flagged
    );
    let save = start_save(&mut doc, "a!");
    assert!(doc.settle_save(save.generation));
    doc.on_local_save_result("a!".into(), Err("磁盘满".into()), Instant::now(), || {
        None
    });
    assert!(doc.is_dirty() && doc.ext_changed() && doc.save_error().is_some());

    let Some(LoadStart::Read { generation }) = doc.begin_load(false) else {
        panic!();
    };
    assert!(doc.settle_load(generation));
    let editor = doc.apply_local_content(text("x\r\n\ty\r\n"));
    assert_eq!(editor, "x\n    y\n", "先归一行尾再展开 Tab");
    assert_eq!(doc.saved(), editor);
    assert_eq!(doc.disk(), editor);
    assert!(doc.indents_with_tabs());
    assert!(!doc.is_dirty());
    assert!(!doc.ext_changed());
    assert!(doc.save_error().is_none() && doc.save_warning().is_none());
    assert!(doc.refresh_warning().is_none());
    assert!(doc.result().is_some());
}

#[test]
fn 会话_编辑与草稿口径() {
    let mut doc = loaded_local("a");
    doc.on_edit("ab");
    assert!(doc.is_dirty());
    doc.on_edit("a");
    assert!(!doc.is_dirty(), "改回原样就不脏");
    // 没有编辑器时草稿就是磁盘内容的投影
    assert_eq!(doc.draft(None), "a");
    assert_eq!(doc.draft(Some("编辑器里的".into())), "编辑器里的");
}

#[test]
fn 会话_远程载入_身份失效不落基线_从没载入过才报错() {
    let mut doc = Session::new();
    settled_load(&mut doc);
    assert!(doc.set_remote_source_invalid(true), "身份变了要重画");
    assert!(!doc.set_remote_source_invalid(true), "没变不重画");
    assert_eq!(doc.apply_remote_content(remote("a", 1)), None);
    assert_eq!(
        doc.error(),
        Some(t("fileViewer", "remoteConnectionChanged"))
    );
    assert!(doc.result().is_none());

    // 已有内容时身份失效:保留旧内容,不进致命错误页
    let mut doc = loaded_remote("a", 1);
    doc.set_remote_source_invalid(true);
    assert_eq!(doc.apply_remote_content(remote("b", 2)), None);
    assert!(doc.error().is_none());
    assert_eq!(doc.saved(), "a");
    assert_eq!(doc.remote_baseline(), Some(&1));
}

#[test]
fn 会话_远程冲突_重新加载时换基线清冲突() {
    let mut doc = loaded_remote("a", 1);
    doc.on_edit("a!");
    let generation = doc.begin_remote_refresh();
    assert!(doc.settle_remote_refresh(generation));
    assert_eq!(
        doc.on_refresh_loaded(remote("b", 2), true),
        RefreshLoaded::Conflict
    );
    let current = doc.take_remote_conflict().expect("冲突时读回的内容要留着");
    assert!(!doc.has_remote_conflict());
    assert_eq!(doc.apply_remote_content(current).as_deref(), Some("b"));
    assert_eq!(doc.remote_baseline(), Some(&2));
    assert!(!doc.is_dirty());
    assert!(doc.take_remote_conflict().is_none());
}

#[test]
fn 会话_激活刷新_只有干净空闲且身份有效的远程文本才重读() {
    let doc = loaded_remote("a", 1);
    assert!(doc.should_refresh_on_activation(true, false));
    assert!(
        !doc.should_refresh_on_activation(false, false),
        "本地文档不刷"
    );
    assert!(!doc.should_refresh_on_activation(true, true), "看图不刷");

    let mut dirty = loaded_remote("a", 1);
    dirty.on_edit("a!");
    assert!(
        !dirty.should_refresh_on_activation(true, false),
        "脏草稿不刷"
    );

    let mut saving = loaded_remote("a", 1);
    start_save(&mut saving, "b");
    assert!(
        !saving.should_refresh_on_activation(true, false),
        "保存中不刷"
    );

    let mut loading = loaded_remote("a", 1);
    loading.begin_load(false);
    assert!(
        !loading.should_refresh_on_activation(true, false),
        "载入中不刷"
    );

    let mut refreshing = loaded_remote("a", 1);
    refreshing.begin_remote_refresh();
    assert!(
        !refreshing.should_refresh_on_activation(true, false),
        "已在刷新"
    );

    let mut invalid = loaded_remote("a", 1);
    invalid.set_remote_source_invalid(true);
    assert!(
        !invalid.should_refresh_on_activation(true, false),
        "身份失效"
    );
}

#[test]
fn 会话_激活刷新_内容没变只换基线_保住编辑器() {
    let mut doc = loaded_remote("a\r\nb", 1);
    let generation = doc.begin_remote_refresh();
    assert!(doc.error().is_none());
    assert!(doc.settle_remote_refresh(generation));
    assert_eq!(
        doc.on_refresh_loaded(remote("a\r\nb", 2), true),
        RefreshLoaded::Unchanged
    );
    assert_eq!(doc.remote_baseline(), Some(&2));
    assert_eq!(doc.saved(), "a\nb");

    // 没有编辑器实体可保 / 行尾变了:都按「变了」重落
    let mut doc = loaded_remote("a\r\nb", 1);
    let generation = doc.begin_remote_refresh();
    assert!(doc.settle_remote_refresh(generation));
    assert_eq!(
        doc.on_refresh_loaded(remote("a\r\nb", 2), false),
        RefreshLoaded::Replaced(Some("a\nb".into()))
    );
    let generation = doc.begin_remote_refresh();
    assert!(doc.settle_remote_refresh(generation));
    assert_eq!(
        doc.on_refresh_loaded(remote("a\nb", 3), true),
        RefreshLoaded::Replaced(Some("a\nb".into())),
        "CRLF → LF 也算变了"
    );
    assert_eq!(doc.remote_baseline(), Some(&3));
}

#[test]
fn 会话_激活刷新_刷新途中开始打字挂冲突_草稿保住() {
    let mut doc = loaded_remote("a", 1);
    let generation = doc.begin_remote_refresh();
    doc.on_edit("a!");
    assert!(doc.settle_remote_refresh(generation));
    assert_eq!(
        doc.on_refresh_loaded(remote("b", 2), true),
        RefreshLoaded::Conflict
    );
    assert!(doc.has_remote_conflict());
    assert!(doc.is_dirty());
    assert_eq!(doc.saved(), "a", "基线不换,草稿照旧脏");
    assert_eq!(doc.remote_baseline(), Some(&1));
}

#[test]
fn 会话_激活刷新_内容变了按新内容重落_保存清理警告留着() {
    let mut doc = loaded_remote("a", 1);
    let save = start_save(&mut doc, "a2");
    assert!(doc.settle_save(save.generation));
    doc.on_remote_save_result(
        "a2".into(),
        Ok(RemoteSave::Saved {
            baseline: 2,
            warning: Some("临时文件没删掉".into()),
        }),
        Instant::now(),
        || Some("a2".into()),
    );
    assert_eq!(doc.save_warning(), Some("临时文件没删掉"));

    let generation = doc.begin_remote_refresh();
    assert!(doc.settle_remote_refresh(generation));
    assert_eq!(
        doc.on_refresh_loaded(remote("zzz", 3), true),
        RefreshLoaded::Replaced(Some("zzz".into()))
    );
    assert_eq!(doc.saved(), "zzz");
    assert_eq!(doc.remote_baseline(), Some(&3));
    assert_eq!(
        doc.save_warning(),
        Some("临时文件没删掉"),
        "刷新成功只消刷新警告,上次保存的清理警告留到下次保存 / 重载"
    );
}

#[test]
fn 会话_激活刷新_失败按有无内容分警告与致命_过期结果丢弃() {
    // 还没有任何内容:致命,进错误页
    let mut doc = Session::new();
    let generation = doc.begin_remote_refresh();
    assert!(doc.settle_remote_refresh(generation));
    assert_eq!(
        doc.on_refresh_failed("连不上".into(), false),
        RemoteRefreshFailurePresentation::Fatal
    );
    assert_eq!(doc.error(), Some("连不上"));
    assert!(doc.refresh_warning().is_none());
    // 下一次刷新先清掉上次的致命错误
    doc.begin_remote_refresh();
    assert!(doc.error().is_none());

    // 已有内容:只挂警告,编辑器 / 草稿照常可见
    let mut doc = loaded_remote("a", 1);
    with_refresh_warning(&mut doc);
    assert!(doc.error().is_none());

    // 保存作废在飞的刷新:它迟到的结果对不上代次
    let mut doc = loaded_remote("a", 1);
    let generation = doc.begin_remote_refresh();
    assert!(doc.prepare_save("b"));
    assert!(!doc.settle_remote_refresh(generation));
    assert!(doc.should_refresh_on_activation(true, false), "刷新中已撤");
}

#[test]
fn 会话_保存_干净或保存中不存_身份失效不存() {
    let mut doc = loaded_local("a");
    assert!(!doc.prepare_save("a"), "草稿与基线相同,静默不存");
    start_save(&mut doc, "b");
    assert!(!doc.prepare_save("c"), "保存中,静默不存");

    let mut doc = loaded_remote("a", 1);
    doc.set_remote_source_invalid(true);
    assert!(doc.prepare_save("b"));
    assert_eq!(doc.begin_save("b"), None);
    assert!(!doc.is_saving());
}

#[test]
fn 会话_保存_落盘文本先还原_tab_再还原行尾_并清上次的错误警告与冲突() {
    let mut doc = loaded_local("x\r\n\ty\r\n");
    let failed = start_save(&mut doc, "x\n    y\nz\n");
    assert!(doc.settle_save(failed.generation));
    doc.on_local_save_result(
        "x\n    y\nz\n".into(),
        Err("被占用".into()),
        Instant::now(),
        || None,
    );
    assert_eq!(doc.save_error(), Some("被占用"));

    let save = start_save(&mut doc, "x\n    y\nz\n");
    assert!(doc.is_saving());
    assert!(doc.save_error().is_none());
    assert_eq!(save.on_disk, "x\r\n\ty\r\nz\r\n");
    assert_eq!(save.generation, failed.generation, "保存不动代次");

    // 远程冲突也在开始保存时清掉
    let mut doc = loaded_remote("a", 1);
    let save = start_save(&mut doc, "b");
    assert!(doc.settle_save(save.generation));
    doc.on_remote_save_result(
        "b".into(),
        Ok(RemoteSave::ExternalChange {
            current: remote("c", 2),
        }),
        Instant::now(),
        || Some("b".into()),
    );
    assert!(doc.has_remote_conflict());
    start_save(&mut doc, "b");
    assert!(!doc.has_remote_conflict());
}

#[test]
fn 会话_保存_远程没有基线只读_取不到连接标记身份失效() {
    let mut doc = Session::new();
    settled_load(&mut doc);
    let no_baseline = FakeRemote {
        content: text("a"),
        baseline: None,
    };
    assert!(doc.apply_remote_content(no_baseline).is_some());
    assert!(doc.remote_baseline().is_none());
    start_save(&mut doc, "b");
    doc.abort_save_read_only();
    assert!(!doc.is_saving());
    assert_eq!(doc.save_error(), Some(t("fileViewer", "remoteReadOnly")));

    let mut doc = loaded_remote("a", 1);
    start_save(&mut doc, "b");
    doc.abort_save_source_invalid();
    assert!(!doc.is_saving());
    assert!(doc.remote_source_invalid());
}

#[test]
fn 会话_保存_过期结果丢弃且保存态不动() {
    let mut doc = loaded_local("a");
    let save = start_save(&mut doc, "b");
    assert!(!doc.settle_save(save.generation.wrapping_add(1)));
    assert!(doc.is_saving());
    assert!(doc.settle_save(save.generation));
    assert!(!doc.is_saving());
}

#[test]
fn 会话_本地保存_成功收尾按最新草稿算脏_失败只报错() {
    let mut doc = loaded_local("a");
    doc.on_edit("b");
    assert_eq!(
        doc.on_fs_change(Instant::now(), || Some("b".into())),
        FsChange::Flagged
    );
    let save = start_save(&mut doc, "b");
    assert!(doc.settle_save(save.generation));
    // 保存期间用户又敲了一个字
    doc.on_local_save_result("b".into(), Ok(()), Instant::now(), || Some("bc".into()));
    assert_eq!(doc.saved(), "b");
    assert_eq!(doc.disk(), "b");
    assert!(doc.is_dirty(), "按最新草稿比对");
    assert!(!doc.ext_changed(), "收尾清外部改动提示");

    let mut doc = loaded_local("a");
    let save = start_save(&mut doc, "b");
    assert!(doc.settle_save(save.generation));
    doc.on_local_save_result("b".into(), Ok(()), Instant::now(), || Some("b".into()));
    assert!(!doc.is_dirty());
    // 没有编辑器实体时草稿按基线算,收尾后不脏
    let save = start_save(&mut doc, "c");
    assert!(doc.settle_save(save.generation));
    doc.on_local_save_result("c".into(), Ok(()), Instant::now(), || None);
    assert!(!doc.is_dirty());

    let save = start_save(&mut doc, "d");
    assert!(doc.settle_save(save.generation));
    doc.on_local_save_result("d".into(), Err("磁盘满".into()), Instant::now(), || {
        panic!("失败不收尾,不该取草稿")
    });
    assert_eq!(doc.save_error(), Some("磁盘满"));
    assert_eq!(doc.saved(), "c", "失败不动基线");
}

#[test]
fn 会话_远程保存_只有写成功才清刷新警告_成功换新基线() {
    let mut doc = loaded_remote("a", 1);
    with_refresh_warning(&mut doc);

    let save = start_save(&mut doc, "b");
    assert!(doc.settle_save(save.generation));
    doc.on_remote_save_result("b".into(), Err("断线".into()), Instant::now(), || None);
    assert_eq!(doc.save_error(), Some("断线"));
    assert_eq!(doc.refresh_warning(), Some("刷新失败"), "失败不清刷新警告");

    let save = start_save(&mut doc, "b");
    assert!(doc.settle_save(save.generation));
    doc.on_remote_save_result(
        "b".into(),
        Ok(RemoteSave::ExternalChange {
            current: remote("远端改过", 9),
        }),
        Instant::now(),
        || None,
    );
    assert!(doc.has_remote_conflict());
    assert_eq!(doc.refresh_warning(), Some("刷新失败"), "冲突不清刷新警告");
    assert_eq!(doc.saved(), "a");
    assert_eq!(doc.remote_baseline(), Some(&1));

    let save = start_save(&mut doc, "b");
    assert!(doc.settle_save(save.generation));
    doc.on_remote_save_result(
        "b".into(),
        Ok(RemoteSave::Saved {
            baseline: 2,
            warning: None,
        }),
        Instant::now(),
        || Some("b".into()),
    );
    assert!(doc.refresh_warning().is_none(), "写成功才清刷新警告");
    assert_eq!(doc.remote_baseline(), Some(&2));
    assert_eq!(doc.saved(), "b");
    assert!(!doc.is_dirty());
    assert!(!doc.has_remote_conflict());
}

#[test]
fn 会话_外部改动_回声窗口内不理_脏或保存中挂提示_干净重载() {
    // 还没读到内容:不理
    let mut doc = Session::new();
    assert_eq!(
        doc.on_fs_change(Instant::now(), || panic!("没内容不该取草稿")),
        FsChange::Ignore
    );

    let mut doc = loaded_local("a");
    assert_eq!(
        doc.on_fs_change(Instant::now(), || Some("a".into())),
        FsChange::Reload,
        "干净:静默重载"
    );
    assert_eq!(
        doc.on_fs_change(Instant::now(), || None),
        FsChange::Reload,
        "没有编辑器按基线算,也是干净"
    );
    assert!(!doc.ext_changed());
    // 草稿比的是编辑器里的最新值,不是订阅回调里记的脏位
    assert_eq!(
        doc.on_fs_change(Instant::now(), || Some("a!".into())),
        FsChange::Flagged
    );
    assert!(doc.ext_changed());

    // 保存中:草稿与基线相同也只挂提示,不重建
    let mut doc = loaded_local("a");
    start_save(&mut doc, "b");
    assert_eq!(
        doc.on_fs_change(Instant::now(), || Some("a".into())),
        FsChange::Flagged
    );

    // 自己落盘的回声:保存后 ECHO_WINDOW 内不理(也不取草稿),到点照常判
    let mut doc = loaded_local("a");
    let save = start_save(&mut doc, "b");
    assert!(doc.settle_save(save.generation));
    let saved_at = Instant::now();
    doc.on_local_save_result("b".into(), Ok(()), saved_at, || Some("b".into()));
    assert_eq!(
        doc.on_fs_change(saved_at + Duration::from_millis(1999), || {
            panic!("回声窗口内不该取草稿")
        }),
        FsChange::Ignore
    );
    assert_eq!(
        doc.on_fs_change(saved_at + ECHO_WINDOW, || Some("b".into())),
        FsChange::Reload
    );
}
