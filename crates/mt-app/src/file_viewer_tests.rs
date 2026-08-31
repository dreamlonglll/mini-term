use super::*;

fn result(content: &str) -> FileContentResult {
    FileContentResult {
        content: content.to_string(),
        is_binary: false,
        too_large: false,
    }
}

fn contains_raw_markdown_html(node: &MarkdownNode) -> bool {
    matches!(node, MarkdownNode::Html(_))
        || node
            .children()
            .is_some_and(|children| children.iter().any(contains_raw_markdown_html))
}

fn contains_network_loading_markdown_construct(node: &MarkdownNode) -> bool {
    matches!(
        node,
        MarkdownNode::Html(_) | MarkdownNode::Image(_) | MarkdownNode::ImageReference(_)
    ) || node.children().is_some_and(|children| {
        children
            .iter()
            .any(contains_network_loading_markdown_construct)
    })
}

fn contains_active_markdown_construct(node: &MarkdownNode) -> bool {
    matches!(
        node,
        MarkdownNode::Html(_)
            | MarkdownNode::Link(_)
            | MarkdownNode::LinkReference(_)
            | MarkdownNode::Image(_)
            | MarkdownNode::ImageReference(_)
    ) || node
        .children()
        .is_some_and(|children| children.iter().any(contains_active_markdown_construct))
}

fn visible_backslash_escaped_source(value: &str) -> String {
    let mut chars = value.chars().peekable();
    let mut visible = String::with_capacity(value.len());
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek().is_some_and(|next| next.is_ascii_punctuation()) {
            visible.push(chars.next().expect("peeked punctuation must remain"));
        } else {
            visible.push(ch);
        }
    }
    visible
}

#[test]
fn 文件类型三条判定与原版正则同口径() {
    assert!(is_markdown_file("D:\\a\\README.md"));
    assert!(is_markdown_file("/x/notes.MARKDOWN"), "大小写不敏感");
    assert!(is_markdown_file("a.mkd") && is_markdown_file("a.mdx"));
    assert!(!is_markdown_file("a.mdx.bak"), "只看最后一段扩展名");

    assert!(is_image_file("a.PNG") && is_image_file("a.jpeg") && is_image_file("a.jpg"));
    assert!(is_image_file("a.svg") && is_image_file("a.ico") && is_image_file("a.avif"));
    assert!(is_image_file("a.tif") && is_image_file("a.tiff"));
    assert!(!is_image_file("a.txt"));

    assert!(is_html_file("a.html") && is_html_file("a.HTM"));
    assert!(
        !is_html_file("a.xhtml"),
        "原版正则是 /\\.html?$/,xhtml 不算"
    );

    // 折行只给散文类(CodeEditor.tsx:203-206)
    assert!(should_wrap("a.md") && should_wrap("a.txt"));
    assert!(!should_wrap("a.rs") && !should_wrap("a.json"));

    // 没有扩展名一律不是
    assert!(!is_markdown_file("Makefile") && !is_image_file("Makefile"));
}

#[test]
fn 远程_html_只走源码而本地_html_保留预览() {
    assert!(supports_rich_preview(false, "index.html"));
    assert!(!supports_rich_preview(true, "index.html"));
    assert!(supports_rich_preview(false, "README.md"));
    assert!(supports_rich_preview(true, "README.md"));
}

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

#[test]
fn 表格分段_基本两列表() {
    let src = "前文\n\n| 文件 | 职责 |\n|---|---|\n| `a.rs` | 说明 A |\n| b.rs | 说明 B |\n\n后文";
    let segs = split_md_blocks(src);
    assert_eq!(segs.len(), 3);
    assert!(matches!(&segs[0], MdSegment::Text(t) if t.contains("前文")));
    let MdSegment::Table(t) = &segs[1] else {
        panic!("第二段应是表格");
    };
    assert_eq!(t.header, vec!["文件", "职责"]);
    assert_eq!(t.rows.len(), 2);
    assert_eq!(t.rows[0], vec!["`a.rs`", "说明 A"]);
    assert!(matches!(&segs[2], MdSegment::Text(t) if t.contains("后文")));
}

#[test]
fn 表格分段_围栏代码块里的竖线不算表格() {
    let src = "```\n| a | b |\n|---|---|\n```\n正文";
    let segs = split_md_blocks(src);
    assert_eq!(segs.len(), 1, "围栏内的表格样式行不拆:{segs:?}");
}

#[test]
fn markdown_分块尊重围栏标记与长度() {
    let src = concat!(
        "````md\n",
        "```\n",
        "~~~\n",
        "![tracker](https://attacker.example/pixel)\n",
        "| a | b |\n",
        "|---|---|\n",
        "````\n",
        "正文",
    );
    let segs = split_md_blocks(src);
    assert!(
        segs.iter()
            .all(|segment| matches!(segment, MdSegment::Text(_))),
        "围栏内不得拆出图片或表格:{segs:?}"
    );
}

#[test]
fn markdown_分块不会把跨行行内代码识别为图片() {
    let src = concat!(
        "`example\n",
        "![tracker](https://attacker.example/pixel)\n",
        "example`",
    );
    let segs = split_md_blocks(src);
    assert!(
        segs.iter()
            .all(|segment| matches!(segment, MdSegment::Text(_))),
        "跨行行内代码不得拆出图片资源:{segs:?}"
    );
}

#[test]
fn markdown_分块不会拆开列表容器里的围栏代码() {
    for src in [
        concat!(
            "- ````\n",
            "  before\n",
            "  \n",
            "  ![tracker](https://attacker.example/pixel)\n",
            "  | a | b |\n",
            "  |---|---|\n",
            "  ````\n",
        ),
        concat!(
            "1. ~~~\n",
            "   ![tracker](https://attacker.example/pixel)\n",
            "   ~~~\n",
        ),
    ] {
        let segs = split_md_blocks(src);
        assert!(
            segs.iter()
                .all(|segment| matches!(segment, MdSegment::Text(_))),
            "列表容器里的代码不得拆出资源块:{segs:?}"
        );
    }
}

#[test]
fn markdown_分块不会拆开_raw_html_代码容器() {
    let src = concat!(
        "<pre>\n",
        "![tracker](https://attacker.example/pixel)\n",
        "\n",
        "| a | b |\n",
        "|---|---|\n",
        "</pre>\n",
    );
    let segs = split_md_blocks(src);
    assert!(
        segs.iter()
            .all(|segment| matches!(segment, MdSegment::Text(_))),
        "raw HTML 容器里的文本不得拆出资源块:{segs:?}"
    );
}

#[test]
fn markdown_分块遇到嵌套引用定义时保留整篇作用域() {
    let src = concat!(
        "> [image]: https://example.com/pixel.png\n",
        "\n",
        "![preview][image]\n",
        "\n",
        "| a | b |\n",
        "|---|---|\n",
        "| 1 | 2 |",
    );
    let segs = split_md_blocks(src);
    assert_eq!(segs.len(), 1, "引用定义存在时不得分块:{segs:?}");
    assert!(matches!(&segs[0], MdSegment::Text(text) if text == src));
}

#[test]
fn markdown_分块遇到脚注定义时保留整篇作用域() {
    for src in [
        concat!(
            "正文[^note]\n",
            "\n",
            "[^note]: 脚注正文\n",
            "\n",
            "| a | b |\n",
            "|---|---|\n",
            "| 1 | 2 |",
        ),
        concat!(
            "正文[^note]\n",
            "\n",
            "> [^note]: 引用块里的脚注正文\n",
            "\n",
            "| a | b |\n",
            "|---|---|\n",
            "| 1 | 2 |",
        ),
    ] {
        let segs = split_md_blocks(src);
        assert_eq!(segs.len(), 1, "脚注定义存在时不得分块:{segs:?}");
        assert!(matches!(&segs[0], MdSegment::Text(text) if text == src));
    }
}

#[test]
fn markdown_分块不会把缩进代码识别为表格() {
    let src = concat!(
        "    | x |\n",
        "    | --- |\n",
        "    | ![track](https://example.com/pixel) |",
    );
    let segs = split_md_blocks(src);
    assert!(
        segs.iter()
            .all(|segment| matches!(segment, MdSegment::Text(_))),
        "缩进代码不得拆成表格或图片:{segs:?}"
    );
}

#[test]
fn markdown_分块按制表位识别混合缩进代码() {
    for prefix in ["\t", " \t", "  \t", "   \t"] {
        let image = format!("{prefix}![track](https://example.com/pixel)");
        let image_segments = split_md_blocks(&image);
        assert!(
            image_segments
                .iter()
                .all(|segment| matches!(segment, MdSegment::Text(_))),
            "混合缩进图片不得拆出图片段:{prefix:?} {image_segments:?}"
        );

        let table = format!(
            "{prefix}| x |\n{prefix}| --- |\n{prefix}| ![track](https://example.com/pixel) |"
        );
        let table_segments = split_md_blocks(&table);
        assert!(
            table_segments
                .iter()
                .all(|segment| matches!(segment, MdSegment::Text(_))),
            "混合缩进表格不得拆出表格段:{prefix:?} {table_segments:?}"
        );
    }
}

#[test]
fn 表格分段_对齐与码段竖线() {
    // 分隔行的 :---: 语法
    let src = "| a | b | c |\n| :--- | :---: | ---: |\n| 1 | 2 | 3 |";
    let MdSegment::Table(t) = &split_md_blocks(src)[0] else {
        panic!()
    };
    assert_eq!(
        t.aligns,
        vec![MdAlign::Left, MdAlign::Center, MdAlign::Right]
    );

    // code span 里的 | 不拆格,\| 是字面竖线
    assert_eq!(split_cells("| `a|b` | c\\|d |"), vec!["`a|b`", "c|d"]);

    // 短行按表头列数补空
    let src = "| a | b |\n|---|---|\n| 仅一格 |";
    let MdSegment::Table(t) = &split_md_blocks(src)[0] else {
        panic!()
    };
    assert_eq!(t.rows[0], vec!["仅一格", ""]);
}

#[test]
fn 分段_空行拆块_围栏内空行不拆_块距节奏() {
    // 空行是块边界:三段文本 + 一个标题 = 四块
    let segs = split_md_blocks("段落一\n\n段落二\n\n### 标题\n\n段落三");
    assert_eq!(segs.len(), 4, "{segs:?}");
    // 块距:首块 0、普通块 11、标题块 20(原版 margin-top 1.4em 的近似)
    assert_eq!(block_top_margin(0, &segs[0]), 0.0);
    assert_eq!(block_top_margin(1, &segs[1]), 11.0);
    assert_eq!(block_top_margin(2, &segs[2]), 20.0);

    // 围栏代码块里的空行不拆块
    let segs = split_md_blocks("```\naaa\n\nbbb\n```");
    assert_eq!(segs.len(), 1, "{segs:?}");

    // `#` 后没空格不算标题;表格块 13(原版 table margin 1em)
    assert_eq!(
        block_top_margin(1, &MdSegment::Text("#hash 不是标题".into())),
        11.0
    );
    let t = MdSegment::Table(MdTable {
        header: vec![],
        aligns: vec![],
        rows: vec![],
    });
    assert_eq!(block_top_margin(3, &t), 13.0);
}

#[test]
fn 表格列宽_短列有底宽_长列封顶() {
    let t = MdTable {
        header: vec!["文件".into(), "职责".into()],
        aligns: vec![MdAlign::Left, MdAlign::Left],
        rows: vec![vec![
            "`process_monitor.rs`".into(),
            "这一格是很长很长的中文说明,足以超过封顶阈值的长度,再加一点点凑数的文字。".into(),
        ]],
    };
    let w = column_weights(&t);
    assert_eq!(w.len(), 2);
    // 第一列 20 字符、第二列封顶 60 → 20/80 = 0.25,短列不至于被压没
    assert!(w[0] > 0.2 && w[0] < 0.3, "第一列权重 {w:?}");
    assert!((w[0] + w[1] - 1.0).abs() < 1e-5);

    // 纯短表:两列都吃底宽,均分
    let t2 = MdTable {
        header: vec!["a".into(), "b".into()],
        aligns: vec![MdAlign::Left, MdAlign::Left],
        rows: vec![],
    };
    let w2 = column_weights(&t2);
    assert!((w2[0] - 0.5).abs() < 1e-5);
}

#[test]
fn 表格格子_纯文字走快路_带标记的交回_textview() {
    // 快路:一句纯文字(表格里的绝大多数)
    assert!(is_plain_cell("已完成"));
    assert!(is_plain_cell("用户登录模块"));
    assert!(is_plain_cell(""), "空格子");
    assert!(is_plain_cell("P0"));
    // `-` 不在行首不是标记;`=` 单行永远成不了 setext 标题
    assert!(is_plain_cell("2026-08-25"));
    assert!(is_plain_cell("a=b"));
    assert!(is_plain_cell("张三 李四"), "单个空格照走快路");

    // 行内标记一律交回
    assert!(!is_plain_cell("`a.rs`"));
    assert!(!is_plain_cell("**必填**"));
    assert!(!is_plain_cell("下划_线"));
    assert!(!is_plain_cell("[文档](a.md)"));
    assert!(!is_plain_cell("![图](a.png)"));
    assert!(!is_plain_cell("~~废弃~~"));
    assert!(!is_plain_cell("<br>"));
    assert!(!is_plain_cell("a&amp;b"));
    assert!(!is_plain_cell("a\\|b"), "转义符");

    // GFM autolink literal:裸 URL / www. / 邮箱会自动成链接
    assert!(!is_plain_cell("https://example.com"));
    assert!(!is_plain_cell("www.example.com"));
    assert!(!is_plain_cell("a@b.com"));

    // 块级标记在行首才算,而格子已 trim,只看开头一处
    assert!(!is_plain_cell("# 标题"));
    assert!(!is_plain_cell("- 列表项"));
    assert!(!is_plain_cell("+ 列表项"));
    assert!(!is_plain_cell("---"), "分隔线");
    assert!(!is_plain_cell("1. 第一步"));
    assert!(!is_plain_cell("2) 第二步"));
    assert!(is_plain_cell("1.5 倍"), "小数不是有序列表");
    assert!(is_plain_cell("2026 年"), "光是数字开头不算");

    // markdown 折叠空白,纯文本不折 —— 有连续空白就交回,免得排版有差
    assert!(!is_plain_cell("a  b"));
    assert!(!is_plain_cell("a\tb"));
}

#[test]
fn 表格格子_真实形状的表大头走快路() {
    // 「文件 | 职责」这类文档表:只有第一列带反引号,其余都是纯文字
    let src = "| 模块 | 负责人 | 状态 | 备注 |\n|---|---|---|---|\n\
               | `auth.rs` | 张三 | 已完成 | 见设计稿 |\n\
               | 支付 | 李四 | 进行中 | 依赖第三方 |";
    let MdSegment::Table(t) = &split_md_blocks(src)[0] else {
        panic!("应解析成表格")
    };
    let cells: Vec<&String> = t.header.iter().chain(t.rows.iter().flatten()).collect();
    let fast = cells.iter().filter(|c| is_plain_cell(c)).count();
    assert_eq!(cells.len(), 12);
    assert_eq!(fast, 11, "只有 `auth.rs` 那一格该交回 TextView");
}

#[test]
fn 图片段落_认得五种常见写法() {
    // 单张
    let segments = split_md_blocks("![主界面](docs/screenshots/main.png)");
    let [MdSegment::Images(imgs)] = segments.as_slice() else {
        panic!("单张图片应由 AST 拆出来自绘")
    };
    assert_eq!(imgs.len(), 1);
    assert_eq!(imgs[0].url, "docs/screenshots/main.png");
    assert_eq!(imgs[0].alt, "主界面");
    assert!(imgs[0].link.is_none());

    // 带 title
    let segments = split_md_blocks(r#"![图](a.png "标题")"#);
    let [MdSegment::Images(imgs)] = segments.as_slice() else {
        panic!("带标题图片应由 AST 拆出来自绘")
    };
    assert_eq!(imgs[0].url, "a.png");
    assert_eq!(imgs[0].title.as_deref(), Some("标题"));

    // 链接包裹(徽章)
    let segments = split_md_blocks("[![CI](https://img.shields.io/x.svg)](https://ci.example)");
    let [MdSegment::Images(imgs)] = segments.as_slice() else {
        panic!("链接包裹图片应由 AST 拆出来自绘")
    };
    assert_eq!(imgs[0].url, "https://img.shields.io/x.svg");
    assert_eq!(imgs[0].link.as_deref(), Some("https://ci.example"));

    // 一行并排两张
    let segments = split_md_blocks("![a](1.png) ![b](2.png)");
    let [MdSegment::Images(imgs)] = segments.as_slice() else {
        panic!("并排图片应由 AST 拆出来自绘")
    };
    assert_eq!(imgs.len(), 2);
    assert_eq!(imgs[1].url, "2.png");

    // 尖括号写法(路径里有空格)
    let segments = split_md_blocks("![x](<my shots/a b.png>)");
    let [MdSegment::Images(imgs)] = segments.as_slice() else {
        panic!("尖括号目标图片应由 AST 拆出来自绘")
    };
    assert_eq!(imgs[0].url, "my shots/a b.png");
}

#[test]
fn 图片段落_普通文本与无效_commonmark_不会升级成资源() {
    // 前后有文字 → 交给 TextView(内联图片不自绘)
    for source in [
        "看这张 ![a](1.png)",
        "![a](1.png) 就是主界面",
        "- ![a](1.png)",
        "> ![a](1.png)",
        "    ![a](1.png)",
        "![a]()",
        "[文档](a.md)",
        "![x](https://attacker.example/pixel trailing)",
        "![x](https://attacker.example/pixel \"unclosed)",
    ] {
        let segments = split_md_blocks(source);
        assert!(
            segments
                .iter()
                .all(|segment| matches!(segment, MdSegment::Text(_))),
            "普通文本或无效图片语法不得升级成资源:{source:?} {segments:?}"
        );
    }
}

#[test]
fn 纯图片段落自绘_混合段落保留给_textview() {
    let src = "# 标题\n\n上面一句说明\n\n![主界面](docs/main.png)\n\n下面一句";
    let segs = split_md_blocks(src);
    assert_eq!(segs.len(), 4, "{segs:?}");
    let MdSegment::Images(imgs) = &segs[2] else {
        panic!("第三段应是图片:{segs:?}");
    };
    assert_eq!(imgs[0].url, "docs/main.png");
    assert!(matches!(&segs[1], MdSegment::Text(t) if t == "上面一句说明"));
    assert!(matches!(&segs[3], MdSegment::Text(t) if t == "下面一句"));

    let mixed = split_md_blocks("上面一句说明\n![主界面](docs/main.png)\n下面一句");
    assert_eq!(mixed.len(), 1, "混合段落应完整交给 TextView:{mixed:?}");
    assert!(matches!(&mixed[0], MdSegment::Text(_)));

    // 围栏代码块里的图片语法是代码,不拆
    let segs = split_md_blocks("```md\n![a](1.png)\n```");
    assert_eq!(segs.len(), 1, "{segs:?}");
    assert!(matches!(&segs[0], MdSegment::Text(_)));

    let with_definition = split_md_blocks(concat!(
        "![direct](https://example.com/direct.png)\n\n",
        "[docs]: https://example.com/docs\n\n",
        "正文\n",
    ));
    assert!(
        matches!(
            &with_definition[0],
            MdSegment::Images(images) if images[0].url.ends_with("direct.png")
        ),
        "普通定义不得让直链图片失去自绘占位:{with_definition:?}"
    );
    assert_eq!(
        with_definition.len(),
        2,
        "未引用定义不应产生空 TextView 块:{with_definition:?}"
    );
    assert!(matches!(&with_definition[1], MdSegment::Text(text) if text == "正文"));

    let reference = split_md_blocks(concat!(
        "![badge][image]\n\n",
        "[image]: https://example.com/badge.svg\n",
    ));
    assert!(
        matches!(reference.as_slice(), [MdSegment::Text(_)]),
        "引用图片保留整篇定义作用域并在远程 TextView 路径安全降级:{reference:?}"
    );
}

#[test]
fn 远程图片必须先获批准_本地图片保持自动加载() {
    assert!(!markdown_image_can_load(true, false));
    assert!(markdown_image_can_load(true, true));
    assert!(markdown_image_can_load(false, false));
}

#[test]
fn 图片目标_相对路径按当前文件目录解析() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR"));
    // 相对路径 → 落到当前文件所在目录(原版 convertFileSrc(fileDir + '/' + src))
    assert_eq!(
        resolve_image_src("docs/a.png", base),
        MdImageSrc::Local(base.join("docs/a.png"))
    );
    // %20 还原
    assert_eq!(
        resolve_image_src("my%20shots/a.png", base),
        MdImageSrc::Local(base.join("my shots/a.png"))
    );

    // 宿主平台的绝对路径原样
    let absolute = base.join("shots/a.png");
    assert_eq!(
        resolve_image_src(&absolute.to_string_lossy(), base),
        MdImageSrc::Local(absolute)
    );

    #[cfg(windows)]
    {
        // Windows 盘符不能被当成 scheme；file:// 三斜杠会去掉盘符前的 `/`
        assert_eq!(
            resolve_image_src("D:/shots/a.png", base),
            MdImageSrc::Local(PathBuf::from("D:/shots/a.png"))
        );
        assert_eq!(
            resolve_image_src("file:///D:/shots/a.png", base),
            MdImageSrc::Local(PathBuf::from("D:/shots/a.png"))
        );
    }
    // 远程与不认识的 scheme
    assert_eq!(
        resolve_image_src("https://x.dev/a.png", base),
        MdImageSrc::Remote("https://x.dev/a.png".into())
    );
    assert_eq!(
        resolve_image_src("data:image/png;base64,AAA", base),
        MdImageSrc::Unsupported
    );
    assert_eq!(resolve_image_src("  ", base), MdImageSrc::Unsupported);
}

#[test]
fn svg_判定_不被查询串骗到() {
    // 徽章 URL 常带 `?style=`,扩展名只看路径那一截
    assert!(is_svg_target(
        "https://img.shields.io/badge/a-b.svg?style=flat"
    ));
    assert!(is_svg_target("D:\\icons\\a.SVG"));
    assert!(!is_svg_target("https://x.dev/a.png"));
    assert!(!is_svg_target("a/b.svg.png"), "只看最后一段扩展名");
}

#[test]
fn md_内联图片的本地路径改写成_file_url() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
    // 列表项里的内联图片(块级图片行走自绘,不经过这条)
    let out = rewrite_md_image_urls("- ![图](shots/a.png) 说明", &base);
    let image_url = to_file_url(&base.join("shots/a.png")).expect("测试基准路径应为绝对路径");
    assert!(out.starts_with(&format!("- ![图]({image_url})")), "{out}");
    // title 保留
    let out = rewrite_md_image_urls(r#"![图](a.png "标题")"#, &base);
    assert!(out.contains(r#""标题""#), "{out}");
    // 远程与 data: 原样
    let remote = "![x](https://x.dev/a.png)";
    assert_eq!(rewrite_md_image_urls(remote, &base), remote);
    let data = "![x](data:image/png;base64,AAA)";
    assert_eq!(rewrite_md_image_urls(data, &base), data);
    // 围栏代码块 / 行内 code 里的图片语法是代码,不许动
    let fenced = "```md\n![a](b.png)\n```";
    assert_eq!(rewrite_md_image_urls(fenced, &base), fenced);
    let inline_code = "写法是 `![a](b.png)` 这样";
    assert_eq!(rewrite_md_image_urls(inline_code, &base), inline_code);
    // 解析器没有确认成 Image 的宽松/残缺写法不得被改写成有效资源。
    for invalid in ["![x](shots/a.png trailing)", "![x](shots/a.png \"unclosed)"] {
        assert_eq!(rewrite_md_image_urls(invalid, &base), invalid);
    }
}

#[test]
fn html_的本地资源改写成_file_url() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("site");
    let image_url = to_file_url(&base.join("img/a.png")).expect("测试基准路径应为绝对路径");
    let out = rewrite_html_urls(r#"<img src="img/a.png" alt="a">"#, &base);
    assert_eq!(out, format!(r#"<img src="{image_url}" alt="a">"#));
    // 单引号 / 大写属性名 / 等号旁的空白都认
    let image_url = to_file_url(&base.join("a.png")).expect("测试基准路径应为绝对路径");
    let out = rewrite_html_urls("<img SRC = 'a.png'>", &base);
    assert_eq!(out, format!("<img SRC = '{image_url}'>"));
    // href / poster 同样处理
    let poster_url = to_file_url(&base.join("p.jpg")).expect("测试基准路径应为绝对路径");
    let out = rewrite_html_urls(r#"<video poster="p.jpg"></video>"#, &base);
    assert!(out.contains(&poster_url), "{out}");

    // 排除清单(原版正则那一串)一律原样
    for keep in [
        r#"<a href="https://x.dev">x</a>"#,
        r#"<img src="data:image/png;base64,AAA">"#,
        // 井号锚点:`"#` 会提前结束 `r#"…"#`,这条必须用 `r##"…"##`
        r##"<a href="#anchor">锚</a>"##,
        r#"<a href="mailto:a@b.c">mail</a>"#,
        r#"<a href="javascript:void(0)">js</a>"#,
        r#"<img src="file:///D:/site/a.png">"#,
    ] {
        assert_eq!(rewrite_html_urls(keep, &base), keep, "不该改:{keep}");
    }
    // `data-src` 不是 src
    let keep = r#"<img data-src="a.png">"#;
    assert_eq!(rewrite_html_urls(keep, &base), keep);

    // 远程清洗器在见过 svg/math 后会保守扫描后续 raw-text，防 HTML5
    // namespace 恢复漏掉活动图片；可信本地 HTML 不得复用这条 fail-closed
    // 策略，否则 textarea 里的示例文本会被误改成 file:// URL。
    let keep = r#"<svg></svg><textarea><img src="literal.png"></textarea>"#;
    assert_eq!(rewrite_html_urls(keep, &base), keep);
}

#[test]
fn 远程富文本禁用自动资源但保留显式网络链接() {
    let markdown = concat!(
        "- ![secret](file:///home/user/secret.png)\n",
        "![tracker](http://127.0.0.1:8080/a.png)\n",
        "`![code](file:///tmp/code.png)`\n",
        "```md\n![fenced](file:///tmp/fenced.png)\n```",
    );
    let sanitized = sanitize_remote_markdown(markdown);
    assert!(sanitized.contains("- secret"), "{sanitized}");
    assert!(sanitized.contains("tracker"), "{sanitized}");
    assert!(!sanitized.contains("![tracker]"), "{sanitized}");
    assert!(!sanitized.contains("file:///home/user/secret.png"));
    assert!(sanitized.contains("`![code](file:///tmp/code.png)`"));
    assert!(sanitized.contains("![fenced](file:///tmp/fenced.png)"));

    let references = sanitize_remote_markdown(concat!(
        "![secret][local]\n",
        "[local]: <file:///home/user/secret.png> \"title\"\n",
        "![web][remote]\n",
        "[remote]: https://example.com/image.png\n",
    ));
    assert!(!references.contains("file:///"), "{references}");
    assert!(
        references.contains("[remote]: https://example.com/image.png"),
        "{references}"
    );
    // Unresolved reference syntax may remain as literal text. The reparsed
    // AST below is the security boundary: no active image/reference node
    // may survive sanitization.
    let references_ast = markdown::to_mdast(&references, &ParseOptions::gfm())
        .expect("sanitized references must remain parseable");
    let mut unsafe_reference_nodes = Vec::new();
    collect_remote_markdown_replacements(&references_ast, &mut unsafe_reference_nodes);
    assert!(unsafe_reference_nodes.is_empty(), "{references}");

    let links = sanitize_remote_markdown(concat!(
        "[local](file:///etc/passwd)\n",
        "[relative](../secret.txt)\n",
        "[web](https://example.com/docs)\n",
        "[<file:///etc/shadow>](file:///tmp/outer)\n",
        "<file:///etc/group>\n",
        "`[code](file:///tmp/code)`\n",
        "``[code](file:///tmp/double)``\n",
        "` unmatched [unsafe](file:///tmp/unmatched)\n",
        "```md\n[code](file:///tmp/fenced)\n```",
    ));
    assert!(!links.contains("file:///etc/passwd"), "{links}");
    assert!(!links.contains("../secret.txt"), "{links}");
    assert!(!links.contains("file:///etc/group"), "{links}");
    assert!(!links.contains("file:///etc/shadow"), "{links}");
    assert!(!links.contains("file:///tmp/outer"), "{links}");
    assert!(!links.contains("file:///tmp/unmatched"), "{links}");
    assert!(links.contains("local\nrelative\n"), "{links}");
    assert!(links.contains("[web](https://example.com/docs)"), "{links}");
    assert!(links.contains("`[code](file:///tmp/code)`"), "{links}");
    assert!(links.contains("``[code](file:///tmp/double)``"), "{links}");
    assert!(links.contains("` unmatched unsafe"), "{links}");
    assert!(links.contains("[code](file:///tmp/fenced)"), "{links}");

    let multiline = sanitize_remote_markdown(concat!(
        "![secret](\nfile:///home/user/secret.png\n)\n",
        "[open](\nfile:///etc/passwd\n)\n",
    ));
    assert!(!multiline.contains("file:///"), "{multiline}");
    assert!(multiline.contains("secret"), "{multiline}");
    assert!(multiline.contains("open"), "{multiline}");

    let decoded_label_injection = sanitize_remote_markdown(concat!(
        "[&#91;open&#93;&#40;file:///etc/passwd&#41;](file:///outer)\n",
        "![&#91;image&#93;&#40;file:///tmp/a.png&#41;](file:///image)\n",
        "[&#91;ref&#93;]: file:///definition\n",
    ));
    assert!(
        !decoded_label_injection.contains("file:///outer"),
        "{decoded_label_injection}"
    );
    assert!(
        !decoded_label_injection.contains("file:///image"),
        "{decoded_label_injection}"
    );
    // 定义不能中断前面的段落；这一行从首次解析起就是普通文本，不会生成链接。
    assert!(
        decoded_label_injection.contains("[&#91;ref&#93;]: file:///definition"),
        "{decoded_label_injection}"
    );
    let ast = markdown::to_mdast(&decoded_label_injection, &ParseOptions::gfm())
        .expect("sanitized markdown must remain parseable");
    let mut unsafe_nodes = Vec::new();
    collect_remote_markdown_replacements(&ast, &mut unsafe_nodes);
    assert!(unsafe_nodes.is_empty(), "{decoded_label_injection}");

    let fence_edges = sanitize_remote_markdown(concat!(
        "    ```\n",
        "[after-indent](file:///tmp/after-indent)\n",
        "```md\n",
        "~~~\n",
        "[inside](file:///tmp/inside)\n",
        "```\n",
        "[outside](file:///tmp/outside)\n",
    ));
    assert!(
        !fence_edges.contains("file:///tmp/after-indent"),
        "{fence_edges}"
    );
    assert!(fence_edges.contains("file:///tmp/inside"), "{fence_edges}");
    assert!(
        !fence_edges.contains("file:///tmp/outside"),
        "{fence_edges}"
    );

    let html = concat!(
        r#"<img src="file:///home/user/secret.png">"#,
        r#"<img src="http://127.0.0.1:8080/a.png">"#,
        r#"<a href="file:///etc/passwd">local</a>"#,
        r#"<a href="https://example.com/docs">web</a>"#,
        r##"<a href="#section">anchor</a>"##,
    );
    let sanitized = sanitize_remote_html_urls(html);
    assert!(!sanitized.contains("file:///"), "{sanitized}");
    assert_eq!(sanitized.matches(r#"src="about:blank""#).count(), 2);
    assert!(sanitized.contains(r##"href="#""##), "{sanitized}");
    assert!(
        sanitized.contains("https://example.com/docs"),
        "{sanitized}"
    );
    assert!(sanitized.contains(r##"href="#section""##), "{sanitized}");

    let unquoted = sanitize_remote_html_urls(concat!(
        r#"<img src=file:///etc/passwd>"#,
        r#"<img/src=file:///etc/group>"#,
        r#"<img alt="x"src=file:///etc/hosts>"#,
        r#"<img src=https://example.com/image.png>"#,
        r#"<a href=../secret.txt>local</a>"#,
    ));
    assert!(!unquoted.contains("file:///"), "{unquoted}");
    assert!(!unquoted.contains("src=https://example.com/image.png"));
    assert!(unquoted.contains("src=about:blank"), "{unquoted}");
    assert!(unquoted.contains("href=#"), "{unquoted}");

    let stray_text = sanitize_remote_html_urls(
        "plain href=\" without a closing quote\n<img src=file:///etc/shadow>",
    );
    assert!(!stray_text.contains("file:///"), "{stray_text}");

    for source in [
        r#"<!-- normal --><img src="https://evil.test/normal.png">"#,
        r#"<!--x--!><img src="https://evil.test/bang.png">"#,
        r#"<!--><img src="https://evil.test/abrupt.png">"#,
        r#"<!--><img src="https://evil.test/abrupt-with-tail.png">-->"#,
        r#"<!---><img src="https://evil.test/short.png">"#,
        r#"</div "><img src="https://evil.test/end-tag.png">"#,
        r#"<script>x</script "><img src="https://evil.test/raw-end-tag.png">"#,
        r#"<svg><script><img src="https://evil.test/foreign.png"></script></svg>"#,
        r#"<svg><p><math></svg><script><img src="https://evil.test/foreign-recovery-a.png"></script>"#,
        r#"<svg></math><p><math></svg><script><img src="https://evil.test/foreign-recovery-b.png"></script>"#,
    ] {
        let html = sanitize_remote_html_urls(source);
        assert!(html.contains(r#"src="about:blank""#), "{html}");
        assert!(!html.contains("src=\"https://evil.test"), "{html}");

        let markdown = sanitize_remote_markdown(source);
        let ast = markdown::to_mdast(&markdown, &ParseOptions::gfm())
            .expect("sanitized Markdown must remain parseable");
        assert!(!contains_raw_markdown_html(&ast), "{markdown}");
        assert!(
            !contains_network_loading_markdown_construct(&ast),
            "{markdown}"
        );
        assert_eq!(visible_backslash_escaped_source(&markdown), source);
    }

    let raw_text = concat!(
        r#"<textarea /><img src="https://example.com/text-example.png"></textarea>"#,
        r#"<img src="https://evil.test/after-textarea.png">"#,
    );
    let scanned = sanitize_remote_html_urls(raw_text);
    assert!(
        scanned.contains("https://example.com/text-example.png"),
        "{scanned}"
    );
    assert!(scanned.contains(r#"src="about:blank""#), "{scanned}");
    assert!(
        !scanned.contains("https://evil.test/after-textarea.png"),
        "{scanned}"
    );

    let markdown = sanitize_remote_markdown(raw_text);
    assert_eq!(visible_backslash_escaped_source(&markdown), raw_text);
    let ast = markdown::to_mdast(&markdown, &ParseOptions::gfm())
        .expect("sanitized Markdown must remain parseable");
    assert!(!contains_raw_markdown_html(&ast), "{markdown}");
    assert!(
        !contains_network_loading_markdown_construct(&ast),
        "{markdown}"
    );
}

#[test]
fn markdown_html_只降级真实_ast_节点并保留代码原文() {
    let source = concat!(
        "`<img src=\"https://example.com/inline.png\">`\n\n",
        "`<Widget src=\"file:///tmp/widget\" />`\n\n",
        "```html\n<a href=\"file:///tmp/example\">example</a>\n```\n\n",
        "```jsx\n<Component href=\"file:///tmp/component\" />\n```\n\n",
        "<pre>\n&lt;img src=\"https://example.com/pre-example.png\"&gt;\n</pre>\n\n",
        "<!-- <img src=\"https://example.com/comment-example.png\"> -->\n\n",
        "<script>const demo = '<img src=\"https://example.com/script-example.png\">';</script>\n\n",
        r#"<img src="https://example.com/active.png">"#,
        "\n",
        r#"<a href="file:///etc/passwd">local</a>"#,
        "\n",
        r#"<a href="https://example.com/docs">web</a>"#,
    );
    let sanitized = sanitize_remote_markdown(source);
    assert!(
        sanitized.contains("`<img src=\"https://example.com/inline.png\">`"),
        "{sanitized}"
    );
    assert!(
        sanitized.contains("`<Widget src=\"file:///tmp/widget\" />`"),
        "{sanitized}"
    );
    assert!(
        sanitized.contains("```html\n<a href=\"file:///tmp/example\">example</a>\n```"),
        "{sanitized}"
    );
    assert!(
        sanitized.contains("```jsx\n<Component href=\"file:///tmp/component\" />\n```"),
        "{sanitized}"
    );
    assert_eq!(visible_backslash_escaped_source(&sanitized), source);

    let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
        .expect("sanitized Markdown must remain parseable");
    assert!(!contains_raw_markdown_html(&ast), "{sanitized}");
    assert!(
        !contains_network_loading_markdown_construct(&ast),
        "{sanitized}"
    );
}

#[test]
fn 审核载荷在远程与会话_markdown中都不能形成活动_html() {
    for payload in [
        r#"<div><select><title></select><img src="https://attacker.example/beacon.png"></title></div>"#,
        r#"<select><plaintext></select><img src="https://attacker.example/b2.png"><a href="file:///C:/Windows/notepad.exe">open</a>"#,
        r#"<template><col><title></template><img src="https://attacker.example/b3.png"></title>"#,
        r#"<div data-example="![beacon](https://attacker.example/b4.png)"></div>"#,
    ] {
        for sanitized in [
            sanitize_remote_markdown(payload),
            sanitize_session_markdown(payload),
        ] {
            assert_eq!(visible_backslash_escaped_source(&sanitized), payload);
            let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
                .expect("sanitized Markdown must remain parseable");
            assert!(!contains_raw_markdown_html(&ast), "{sanitized}");
            assert!(
                !contains_network_loading_markdown_construct(&ast),
                "{sanitized}"
            );
            assert!(!contains_active_markdown_construct(&ast), "{sanitized}");
            let mut unsafe_nodes = Vec::new();
            collect_untrusted_markdown_replacements(&ast, &mut unsafe_nodes);
            assert!(unsafe_nodes.is_empty(), "{sanitized}");
        }
    }
}

#[test]
fn html_block_type_1到5后的缩进活动载荷会清洗到不动点() {
    let html_blocks = [
        "<pre></pre>",
        "<style></style>",
        "<!-- comment -->",
        "<?php ?>",
        "<!DOCTYPE html>",
        "<![CDATA[value]]>",
    ];
    let indented_payloads = [
        "![network](https://attacker.example/image.png)",
        "[local](file:///etc/passwd)",
        "![local](file:///etc/passwd)",
        r#"<img src="https://attacker.example/raw.png">"#,
        r#"<a href="file:///etc/passwd">open</a>"#,
    ];

    for html_block in html_blocks {
        for payload in indented_payloads {
            let source = format!("{html_block}\n    {payload}\n");
            for sanitized in [
                sanitize_remote_markdown(&source),
                sanitize_session_markdown(&source),
            ] {
                assert_ne!(
                    sanitized,
                    markdown_as_indented_code(&source),
                    "正常审核载荷应在轮次上限内收敛:{source}"
                );
                let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
                    .expect("fixed-point Markdown must remain parseable");
                let mut replacements = Vec::new();
                collect_untrusted_markdown_replacements(&ast, &mut replacements);
                assert!(replacements.is_empty(), "{source}\n---\n{sanitized}");
                assert!(
                    !contains_active_markdown_construct(&ast),
                    "{source}\n---\n{sanitized}"
                );
            }
        }
    }
}

#[test]
fn markdown清洗超出轮次时整篇降级为可见代码块() {
    let source = concat!(
        "<!-- comment -->\n",
        "    ![network](https://attacker.example/image.png)\n",
    );
    let sanitized = sanitize_untrusted_markdown_with_pass_limit(source, 1);
    assert_eq!(sanitized, markdown_as_indented_code(source));

    let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
        .expect("fallback Markdown must remain parseable");
    let mut replacements = Vec::new();
    collect_untrusted_markdown_replacements(&ast, &mut replacements);
    assert!(replacements.is_empty(), "{sanitized}");
    assert!(!contains_active_markdown_construct(&ast), "{sanitized}");
}

#[test]
fn 已安全markdown在首轮不动点保持原文() {
    let source = concat!(
        "# 标题\n\n",
        "正文 [docs](https://example.com/docs)\n\n",
        "`<img src=\"https://example.com/code.png\">`\n",
    );
    assert_eq!(sanitize_remote_markdown(source), source);
    assert_eq!(sanitize_session_markdown(source), source);
}

#[test]
fn 不安全目标降级时保留带标点的标签() {
    let sanitized = sanitize_remote_markdown(concat!(
        "[main.rs](src/main.rs)\n",
        "![截图(1).png](./a.png)\n",
    ));
    assert!(sanitized.contains(r"main\.rs"), "{sanitized}");
    assert!(sanitized.contains(r"截图\(1\)\.png"), "{sanitized}");
    assert!(!sanitized.contains("link"), "{sanitized}");
    assert!(!sanitized.contains("image"), "{sanitized}");

    let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
        .expect("sanitized labels must remain parseable");
    let mut unsafe_nodes = Vec::new();
    collect_remote_markdown_replacements(&ast, &mut unsafe_nodes);
    assert!(unsafe_nodes.is_empty(), "{sanitized}");
}

#[test]
fn 会话富文本不触发任何图片或外部_html_资源() {
    let source = concat!(
        "![web](https://example.com/pixel)\n",
        "![local](file:///etc/passwd)\n",
        "![reference][image]\n",
        "[image]: https://example.com/reference.png\n",
        "[docs](https://example.com/docs)\n",
        "`<img src=\"https://example.com/code-inline.png\">`\n",
        "```html\n<img src=\"https://example.com/code-fenced.png\">\n```\n",
        r##"<img src="https://example.com/html.png"><img src="file:///etc/group"><a href="https://example.com/html">html</a><a href="#section">anchor</a>"##,
    );
    let sanitized = sanitize_session_markdown(source);
    let visible = visible_backslash_escaped_source(&sanitized);
    assert!(
        visible.contains(
            r##"<img src="https://example.com/html.png"><img src="file:///etc/group"><a href="https://example.com/html">html</a><a href="#section">anchor</a>"##,
        ),
        "raw HTML 源码应保持可见:{sanitized}"
    );
    assert!(
        sanitized.contains("`<img src=\"https://example.com/code-inline.png\">`"),
        "{sanitized}"
    );
    assert!(
        sanitized.contains("```html\n<img src=\"https://example.com/code-fenced.png\">\n```"),
        "{sanitized}"
    );
    assert!(
        sanitized.contains("[docs](https://example.com/docs)"),
        "{sanitized}"
    );

    let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
        .expect("sanitized session markdown must remain parseable");
    assert!(!contains_raw_markdown_html(&ast), "{sanitized}");
    let mut unsafe_nodes = Vec::new();
    collect_untrusted_markdown_replacements(&ast, &mut unsafe_nodes);
    assert!(unsafe_nodes.is_empty(), "{sanitized}");
}

#[test]
fn 本地预览读取只接受限额内普通文件() {
    let dir = std::env::temp_dir().join(format!("mt-preview-http-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);

    let small = dir.join("small.png");
    std::fs::write(&small, b"small-image").unwrap();
    assert_eq!(
        fetch_local_preview_bytes(&small).unwrap().as_slice(),
        b"small-image"
    );
    assert!(
        fetch_local_preview_bytes(&dir).is_err(),
        "目录不得作为预览资源读取"
    );

    let oversized = dir.join("oversized.png");
    let file = std::fs::File::create(&oversized).unwrap();
    file.set_len(PREVIEW_IMAGE_MAX_BYTES + 1).unwrap();
    drop(file);
    assert!(
        fetch_local_preview_bytes(&oversized).is_err(),
        "超过硬上限的稀疏文件必须在读取前拒绝"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 路径比对反斜杠归一且不分大小写() {
    assert!(same_path("D:\\Git\\a.rs", "d:/git/A.RS"));
    assert!(!same_path("D:\\Git\\a.rs", "D:\\Git\\b.rs"));
    // 目录级 notify 事件里的兄弟文件不该被认成自己
    assert!(!same_path("D:/p/README.md", "D:/p/README.md.bak"));
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

#[test]
fn 语言按扩展名映射到组件库认得的名字() {
    assert_eq!(language_for("main.rs"), "rust");
    assert_eq!(language_for("D:\\p\\src\\store.ts"), "typescript");
    assert_eq!(language_for("App.tsx"), "tsx");
    assert_eq!(language_for("index.JS"), "javascript", "大小写不敏感");
    assert_eq!(language_for("Cargo.toml"), "toml");
    assert_eq!(language_for("config.yml"), "yaml");
    assert_eq!(language_for("a.jsonc"), "json");
    assert_eq!(language_for("run.sh"), "bash");
    assert_eq!(language_for("a.hpp"), "cpp");
    assert_eq!(language_for("a.h"), "c");
    // 特殊文件名压扩展名
    assert_eq!(language_for("Makefile"), "make");
    assert_eq!(language_for("CMakeLists.txt"), "cmake");
    assert_eq!(language_for("Dockerfile"), "bash");
    // 认不出 → 纯文本(原版「匹配不到就是纯文本」)
    assert_eq!(language_for("notes.xyz"), "text");
    assert_eq!(language_for("LICENSE"), "text");
}

#[test]
fn 映射出来的语言名组件库全都认得() {
    // 认不得会静默退成 Plain,画出来没有高亮而编译期无感 —— 用它自己的
    // `from_str` 钉住:除了 "text",每个名字都要落到非 Plain 的分支
    use gpui_component::highlighter::Language;
    for name in [
        "rust",
        "typescript",
        "tsx",
        "javascript",
        "json",
        "python",
        "go",
        "ruby",
        "java",
        "csharp",
        "c",
        "cpp",
        "css",
        "html",
        "bash",
        "toml",
        "yaml",
        "markdown",
        "sql",
        "swift",
        "zig",
        "elixir",
        "scala",
        "proto",
        "graphql",
        "diff",
        "cmake",
        "ejs",
        "erb",
        "make",
    ] {
        assert_ne!(
            Language::from_str(name).name(),
            Language::Plain.name(),
            "组件库不认得语言名 {name}"
        );
    }
    assert_eq!(Language::from_str("text").name(), Language::Plain.name());
}

#[test]
fn 命中行定位拒绝越界行号() {
    let text = "a\nb\nc\n";
    assert_eq!(highlight_target(Some(2), text), Some(2));
    assert_eq!(highlight_target(Some(3), text), Some(3));
    // 越界不动(原版 `highlightLine > doc.lines` 直接 return)
    assert_eq!(highlight_target(Some(9), text), None);
    assert_eq!(highlight_target(Some(0), text), None, "行号是 1-based");
    // 文件树那条路压根不给行号
    assert_eq!(highlight_target(None, text), None);
    // 空文件也算有第 1 行
    assert_eq!(highlight_target(Some(1), ""), Some(1));
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
