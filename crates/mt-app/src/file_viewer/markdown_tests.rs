use super::*;

fn split_md_blocks(source: &str) -> Vec<MdSegment> {
    split_md_document(source).segs
}

fn outline_of(source: &str) -> Vec<(u8, String, usize)> {
    split_md_document(source)
        .outline
        .into_iter()
        .map(|heading| (heading.level, heading.text, heading.block))
        .collect()
}

#[test]
fn 大纲收顶层标题_块号指向标题所在块() {
    let src = concat!(
        "# 标题 **一**\n",
        "\n",
        "正文\n",
        "\n",
        "| a | b |\n",
        "|---|---|\n",
        "| 1 | 2 |\n",
        "\n",
        "## 用 `cargo` 构建 [链接](https://x.y)\n",
        "\n",
        "Setext 二级\n",
        "---\n",
    );
    let doc = split_md_document(src);
    assert_eq!(
        outline_of(src),
        vec![
            (1, "标题 一".to_string(), 0),
            (2, "用 cargo 构建 链接".to_string(), 3),
            (2, "Setext 二级".to_string(), 4),
        ]
    );
    for heading in &doc.outline {
        let MdSegment::Text(text) = &doc.segs[heading.block] else {
            panic!("{heading:?} 落在了非正文块:{:?}", doc.segs);
        };
        let first_word = heading.text.split(' ').next().unwrap();
        assert!(
            text.contains(first_word),
            "{heading:?} 应落在自己那一块:{text:?}"
        );
    }
}

#[test]
fn 标题与上文没有空行也另起一块() {
    let src = "前文\n## 第二节\n紧跟的正文";
    let segs = split_md_blocks(src);
    assert_eq!(segs.len(), 2, "{segs:?}");
    assert!(matches!(&segs[0], MdSegment::Text(t) if t == "前文"));
    // 标题与紧跟的正文仍在同一块(只在标题**之前**切)
    assert!(matches!(&segs[1], MdSegment::Text(t) if t == "## 第二节\n紧跟的正文"));
    assert_eq!(outline_of(src), vec![(2, "第二节".to_string(), 1)]);
}

#[test]
fn 大纲不收容器里与代码里的标题_空标题不收() {
    let src = concat!(
        "> # 引用里的\n",
        "\n",
        "- # 列表里的\n",
        "\n",
        "```\n",
        "# 代码里的\n",
        "```\n",
        "\n",
        "#\n",
        "\n",
        "### 真标题\n",
    );
    let doc = split_md_document(src);
    let outline: Vec<(u8, &str)> = doc
        .outline
        .iter()
        .map(|heading| (heading.level, heading.text.as_str()))
        .collect();
    assert_eq!(outline, vec![(3, "真标题")], "{:?}", doc.segs);
    assert!(
        matches!(&doc.segs[doc.outline[0].block], MdSegment::Text(t) if t == "### 真标题"),
        "{:?}",
        doc.segs
    );
}

#[test]
fn 整篇交回_text_view_时不给大纲() {
    // 引用定义要共享作用域 → 整篇一块,点哪条都只能滚到文首
    let src = "# 一\n\n[链接][ref]\n\n## 二\n\n[ref]: https://example.com\n";
    assert_eq!(split_md_blocks(src).len(), 1);
    assert!(outline_of(src).is_empty());
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
fn mermaid_围栏_只拆顶层_其余围栏与容器内的照走_textview() {
    // issue #80 的最小样例:顶层围栏拆成 Mermaid 段,code 是围栏内文本、raw 含反引号行
    let src = "前文\n\n```mermaid\ngraph TD\n    A[开始] --> B[结束]\n```\n\n后文";
    let segs = split_md_blocks(src);
    assert_eq!(segs.len(), 3, "{segs:?}");
    let MdSegment::Mermaid { code, raw } = &segs[1] else {
        panic!("第二段应是 mermaid:{segs:?}");
    };
    assert_eq!(code, "graph TD\n    A[开始] --> B[结束]");
    assert!(
        raw.starts_with("```mermaid\n") && raw.ends_with("```"),
        "{raw:?}"
    );
    assert!(matches!(&segs[0], MdSegment::Text(t) if t == "前文"));
    assert!(matches!(&segs[2], MdSegment::Text(t) if t == "后文"));
    // 图表块与图片 / 表格同档间距
    assert_eq!(block_top_margin(1, &segs[1]), 13.0);

    // info string 大小写放宽、带空格也认;别的语言与无语言围栏不拆;`mermaid-js`
    // 这类方言后缀不认
    assert!(is_mermaid_fence(Some("mermaid")));
    assert!(is_mermaid_fence(Some("Mermaid")));
    assert!(is_mermaid_fence(Some(" mermaid ")));
    assert!(!is_mermaid_fence(Some("mermaid-js")));
    assert!(!is_mermaid_fence(Some("rust")));
    assert!(!is_mermaid_fence(None));
    for src in [
        "```\ngraph TD\nA-->B\n```",
        "```rust\nfn main() {}\n```",
        "~~~mermaid-js\nA-->B\n~~~",
    ] {
        let segs = split_md_blocks(src);
        assert!(
            matches!(segs.as_slice(), [MdSegment::Text(_)]),
            "非 mermaid 围栏不得拆出图表段:{src:?} {segs:?}"
        );
    }
    // `~~~` 围栏与 ``` 等价
    let segs = split_md_blocks("~~~mermaid\ngraph LR\nA-->B\n~~~");
    assert!(
        matches!(segs.as_slice(), [MdSegment::Mermaid { code, .. }] if code == "graph LR\nA-->B"),
        "{segs:?}"
    );

    // 列表项 / 引用块里的围栏随容器整块交给 TextView,不拆
    for src in [
        "- 一项\n\n  ```mermaid\n  graph TD\n  A-->B\n  ```",
        "> ```mermaid\n> graph TD\n> A-->B\n> ```",
    ] {
        let segs = split_md_blocks(src);
        assert!(
            segs.iter()
                .all(|segment| matches!(segment, MdSegment::Text(_))),
            "容器内的 mermaid 围栏不得拆出图表段:{src:?} {segs:?}"
        );
    }
    // 缩进代码块里的「```mermaid」是代码文本
    let segs = split_md_blocks("    ```mermaid\n    graph TD\n    ```");
    assert!(matches!(segs.as_slice(), [MdSegment::Text(_)]), "{segs:?}");
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

// ─── 链接处置 ─────────────────────────────────────────────────

#[test]
fn 链接按原版四条口径分类() {
    let cur = "D:/Git/x/docs/README.md";
    assert_eq!(
        classify_link(cur, "https://example.com/a?b=1"),
        LinkAction::External("https://example.com/a?b=1".into())
    );
    assert_eq!(
        classify_link(cur, "HTTP://EXAMPLE.COM"),
        LinkAction::External("HTTP://EXAMPLE.COM".into())
    );
    assert_eq!(
        classify_link(cur, "#%E5%AE%89%E8%A3%85"),
        LinkAction::Anchor("安装".into())
    );
    assert_eq!(
        classify_link(cur, "mailto:a@b.c"),
        LinkAction::Scheme("mailto:a@b.c".into())
    );
    // 盘符不是协议
    assert_eq!(
        classify_link(cur, r"C:\tmp\a.md"),
        LinkAction::Local("C:/tmp/a.md".into())
    );
    assert_eq!(
        classify_link(cur, "../src/main.rs#L10"),
        LinkAction::Local("D:/Git/x/src/main.rs".into())
    );
    assert_eq!(classify_link(cur, "   "), LinkAction::Ignore);
    assert_eq!(classify_link(cur, "#"), LinkAction::Anchor(String::new()));
}

#[test]
fn 本地链接解析规范化路径() {
    // 相对路径、`./`、`..`、`%20`、反斜杠
    assert_eq!(
        resolve_local_href("D:/p/docs/a.md", "./img/b%20c.png?x=1"),
        Some("D:/p/docs/img/b c.png".into())
    );
    assert_eq!(
        resolve_local_href("D:/p/docs/a.md", r"..\..\..\etc"),
        Some("etc".into())
    );
    // POSIX 绝对路径保留前导 `/`(远程项目)
    assert_eq!(
        resolve_local_href("/home/u/p/a.md", "/etc/hosts"),
        Some("/etc/hosts".into())
    );
    assert_eq!(
        resolve_local_href("/home/u/p/a.md", "sub/../b.md"),
        Some("/home/u/p/b.md".into())
    );
    // Windows 绝对路径
    assert_eq!(
        resolve_local_href("/home/u/p/a.md", "D:/x/y.md"),
        Some("D:/x/y.md".into())
    );
    assert_eq!(resolve_local_href("D:/p/a.md", "#only-anchor"), None);
}

#[test]
fn 标题_slug_与原版一致() {
    assert_eq!(heading_slug("  Hello  World "), "hello-world");
    assert_eq!(heading_slug("安装 与 使用!"), "安装-与-使用");
    assert_eq!(heading_slug("v1.2.3 (beta)"), "v123-beta");
    assert_eq!(heading_slug("snake_case-name"), "snake_case-name");
    assert_eq!(heading_slug("Héllo"), "hllo");
    assert_eq!(
        strip_inline_markup("**Bold** `code` [link](http://x) ~~s~~"),
        "Bold code link s"
    );
}

#[test]
fn 锚点按标题_slug_或_html_id_命中() {
    let block = "## 快速开始\n\n正文";
    assert!(block_has_anchor(block, "快速开始"));
    assert!(block_has_anchor(block, "快速开始"));
    assert!(!block_has_anchor(block, "别的"));
    // 标题里的行内标记不影响
    assert!(block_has_anchor(
        "### Using `cargo` **now**",
        "using-cargo-now"
    ));
    // 围栏代码块里的 `# 注释` 不算标题
    assert!(!block_has_anchor("```bash\n# install\n```", "install"));
    // 原始 HTML 锚点
    assert!(block_has_anchor("<a id=\"top\"></a>\n\n# Title", "top"));
    // `#` 后无空格不是标题
    assert!(!block_has_anchor("#hashtag", "hashtag"));
    // 空 id 不命中任何块
    assert!(!block_has_anchor("# x", ""));
}
