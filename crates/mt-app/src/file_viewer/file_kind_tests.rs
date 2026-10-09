use super::*;

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

    assert!(is_pdf_file("D:\\docs\\手册.pdf") && is_pdf_file("/x/a.PDF"));
    assert!(!is_pdf_file("a.pdf.bak") && !is_pdf_file("pdf"));

    // 折行只给散文类(CodeEditor.tsx:203-206)
    assert!(should_wrap("a.md") && should_wrap("a.txt"));
    assert!(!should_wrap("a.rs") && !should_wrap("a.json"));

    // 没有扩展名一律不是
    assert!(!is_markdown_file("Makefile") && !is_image_file("Makefile"));
}

#[test]
fn 路径比对反斜杠归一且不分大小写() {
    assert!(same_path("D:\\Git\\a.rs", "d:/git/A.RS"));
    assert!(!same_path("D:\\Git\\a.rs", "D:\\Git\\b.rs"));
    // 目录级 notify 事件里的兄弟文件不该被认成自己
    assert!(!same_path("D:/p/README.md", "D:/p/README.md.bak"));
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
    assert_eq!(language_for("Cargo.lock"), "toml");
    assert_eq!(language_for("Gemfile"), "ruby");
    assert_eq!(language_for("Jenkinsfile"), "groovy");
    assert_eq!(language_for(".editorconfig"), "ini");
    assert_eq!(language_for(".prettierrc"), "json");
    assert_eq!(language_for(".env"), "bash");
    assert_eq!(language_for(".env.local"), "bash");
    // 补充语言包
    assert_eq!(language_for("Program.cs"), "csharp");
    assert_eq!(language_for("index.php"), "php");
    assert_eq!(language_for("Main.kt"), "kotlin");
    assert_eq!(language_for("build.gradle.kts"), "kotlin");
    assert_eq!(language_for("init.lua"), "lua");
    assert_eq!(language_for("deploy.ps1"), "powershell");
    assert_eq!(language_for("App.csproj"), "xml");
    assert_eq!(language_for("MainWindow.xaml"), "xml");
    assert_eq!(language_for("main.dart"), "dart");
    assert_eq!(language_for("build.gradle"), "groovy");
    assert_eq!(language_for("setup.cfg"), "ini");
    assert_eq!(language_for("run.bat"), "batch");
    assert_eq!(language_for("app.properties"), "ini");
    // 没有专属语法包的退到近似语言
    assert_eq!(language_for("App.vue"), "html");
    assert_eq!(language_for("App.svelte"), "html");
    assert_eq!(language_for("Index.cshtml"), "html");
    assert_eq!(language_for("style.scss"), "css");
    assert_eq!(language_for("BUILD.bazel"), "python");
    // 冷门语言不接:主流之外一律纯文本(用户 2026-09-17 定的口径)
    assert_eq!(language_for("main.hs"), "text");
    assert_eq!(language_for("main.tf"), "text");
    // 认不出 → 纯文本(原版「匹配不到就是纯文本」)
    assert_eq!(language_for("notes.xyz"), "text");
    assert_eq!(language_for("LICENSE"), "text");
}

#[test]
fn 映射出来的语言名注册表全都认得() {
    // 认不得会静默退成纯文本,画出来没有高亮而编译期无感 —— 把 `language_for`
    // 全部可能的返回值(从源码 match 臂里扫出来)逐个去注册表查一遍
    use gpui_component::highlighter::LanguageRegistry;
    crate::syntax_languages::register();
    let registry = LanguageRegistry::singleton();
    let source = include_str!("file_kind.rs");
    let body = source
        .split("pub fn language_for(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("找不到 language_for 的函数体");
    let mut names: Vec<&str> = body
        .split("=> ")
        .skip(1)
        .filter_map(|arm| {
            // `=> "rust",` / `=> return "make",` / `=> {\n return "json";` 三种写法
            let mut arm = arm.trim_start();
            loop {
                let trimmed = arm
                    .trim_start_matches("return ")
                    .trim_start_matches('{')
                    .trim_start();
                if trimmed == arm {
                    break;
                }
                arm = trimmed;
            }
            let rest = arm.strip_prefix('"')?;
            rest.split('"').next()
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    assert!(
        names.len() > 30,
        "只扫出 {} 个名字,扫描逻辑坏了: {names:?}",
        names.len()
    );
    for name in names {
        if name == "text" {
            continue;
        }
        let config = registry
            .language(name)
            .unwrap_or_else(|| panic!("注册表不认得语言名 {name}"));
        assert!(
            !config.highlights.is_empty(),
            "语言 {name} 注册了但高亮查询是空的(组件库那五个漏网之鱼?)"
        );
    }
}
