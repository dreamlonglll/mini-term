use super::*;

fn result(content: &str) -> FileContentResult {
    FileContentResult {
        content: content.to_string(),
        is_binary: false,
        too_large: false,
    }
}

#[test]
fn 远程_html_只走源码而本地_html_保留预览() {
    assert!(supports_rich_preview(false, "index.html"));
    assert!(!supports_rich_preview(true, "index.html"));
    assert!(supports_rich_preview(false, "README.md"));
    assert!(supports_rich_preview(true, "README.md"));
}

#[test]
fn 命中行定位拒绝越界行号() {
    let text = "a\nb\nc\n";
    let line = |n| Some(LineTarget::line(n));
    assert_eq!(highlight_target(line(2), text), line(2));
    assert_eq!(highlight_target(line(3), text), line(3));
    // 越界不动(原版 `highlightLine > doc.lines` 直接 return)
    assert_eq!(highlight_target(line(9), text), None);
    assert_eq!(highlight_target(line(0), text), None, "行号是 1-based");
    // 文件树那条路压根不给行号
    assert_eq!(highlight_target(None, text), None);
    // 空文件也算有第 1 行
    assert_eq!(highlight_target(line(1), ""), line(1));
    // 范围的终点夹到末行
    let range = |line, end| {
        Some(LineTarget {
            line,
            end: Some(end),
        })
    };
    assert_eq!(highlight_target(range(1, 222), text), range(1, 3));
}

#[test]
fn 四种渲染分支的判定顺序() {
    // 图片先于一切:原版图片分支压根不读文件
    assert_eq!(branch_of(true, true, false, None), Branch::Image);
    assert_eq!(branch_of(false, true, false, None), Branch::Loading);
    assert_eq!(branch_of(false, false, true, None), Branch::Error);

    let mut binary = result("");
    binary.is_binary = true;
    let mut large = result("");
    large.too_large = true;
    // 二进制先于过大 —— 二进制文件的 content 也是空的,顺序换了会显示成「文件过大」
    assert_eq!(
        branch_of(false, false, false, Some(&binary)),
        Branch::Binary
    );
    assert_eq!(
        branch_of(false, false, false, Some(&large)),
        Branch::TooLarge
    );
    assert_eq!(
        branch_of(false, false, false, Some(&result("x"))),
        Branch::Editor
    );
    // 读完了但既没结果也没错(不该发生)按 loading 处理,不画空编辑器
    assert_eq!(branch_of(false, false, false, None), Branch::Loading);
}

#[test]
fn 三种不可编辑的情况都不画编辑器() {
    let mut binary = result("");
    binary.is_binary = true;
    let mut large = result("");
    large.too_large = true;
    assert!(!can_edit(true, Some(&result("x"))), "图片");
    assert!(!can_edit(false, Some(&binary)), "二进制");
    assert!(!can_edit(false, Some(&large)), "过大");
    assert!(!can_edit(false, None), "还没读到");
    assert!(can_edit(false, Some(&result("x"))));
}

/// 后端的两道防线(1MB 上限 / 非 UTF-8 即二进制)与前端分支合起来跑一遍真磁盘。
#[test]
fn 二进制与超限探测走真文件() {
    let dir = std::env::temp_dir().join(format!("mt-fv-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);

    // 非 UTF-8 → is_binary
    let bin = dir.join("bin.dat");
    std::fs::write(&bin, [0xff, 0xfe, 0x00, 0x01]).unwrap();
    let res = mt_project::fs::read_file_content(&dir, &bin).unwrap();
    assert!(res.is_binary && !res.too_large);
    assert_eq!(branch_of(false, false, false, Some(&res)), Branch::Binary);
    assert!(!can_edit(false, Some(&res)));

    // > 1MB → too_large(且 content 为空)
    let big = dir.join("big.txt");
    std::fs::write(
        &big,
        vec![b'a'; (mt_project::fs::MAX_FILE_VIEW_SIZE + 1) as usize],
    )
    .unwrap();
    let res = mt_project::fs::read_file_content(&dir, &big).unwrap();
    assert!(res.too_large && !res.is_binary && res.content.is_empty());
    assert_eq!(branch_of(false, false, false, Some(&res)), Branch::TooLarge);

    let _ = std::fs::remove_dir_all(&dir);
}
