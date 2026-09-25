use super::*;

fn root() -> PathBuf {
    PathBuf::from(if cfg!(windows) { r"D:\proj" } else { "/proj" })
}

#[test]
fn 路径编码往返保住中文空格与保留字符() {
    let rel = "docs/说明 页#1?.html";
    let encoded = encode_path(rel);
    assert!(!encoded.contains(' ') && !encoded.contains('#') && !encoded.contains('?'));
    assert!(encoded.starts_with("docs/"), "{encoded}");
    assert_eq!(decode_path(&encoded).as_deref(), Some(rel));
}

#[test]
fn 坏的转义原样保留_非_utf8_解不出() {
    assert_eq!(decode_path("a%2").as_deref(), Some("a%2"));
    assert_eq!(decode_path("a%zzb").as_deref(), Some("a%zzb"));
    assert_eq!(decode_path("%41%42").as_deref(), Some("AB"));
    assert_eq!(decode_path("%FF"), None);
}

#[test]
fn 相对项目根的路径() {
    let root = root();
    assert_eq!(
        rel_of(&root, &root.join("site").join("index.html")).as_deref(),
        Some("site/index.html")
    );
    assert_eq!(rel_of(&root, &root), None, "根本身不是文件");
    let outside = if cfg!(windows) {
        r"D:\other\a.html"
    } else {
        "/other/a.html"
    };
    assert_eq!(rel_of(&root, Path::new(outside)), None);
}

#[test]
fn 同源_url_还原成相对路径() {
    let url = page_url("site/我的 页.html");
    assert_eq!(same_origin_rel(&url).as_deref(), Some("site/我的 页.html"));
    assert_eq!(
        same_origin_rel(&format!("{url}?v=1#top")).as_deref(),
        Some("site/我的 页.html")
    );
    assert_eq!(same_origin_rel("https://example.com/a.html"), None);
    // 前缀相同但不是同一个 host(`…localhost.evil`)
    assert_eq!(same_origin_rel(&format!("{ORIGIN}.evil/a.html")), None);
}

#[test]
fn 导航只放行同源的_html() {
    assert!(navigation_allowed(&page_url("a/b.html")));
    assert!(navigation_allowed(&page_url("a/B.HTM")));
    assert!(navigation_allowed("about:blank"));
    assert!(!navigation_allowed(&page_url("a/readme.md")));
    assert!(!navigation_allowed("https://example.com/"));
    assert!(!navigation_allowed("file:///D:/proj/a.html"));
}

#[test]
fn 文档本身回预览源码() {
    let root = root();
    assert_eq!(
        plan_request(
            "GET",
            "/site/index.html",
            Some("document"),
            "site/index.html",
            &root
        ),
        Plan::Doc
    );
    // 自己 fetch 自己也无妨(源码本来就在页面里)
    assert_eq!(
        plan_request(
            "GET",
            "/site/index.html",
            Some("empty"),
            "site/index.html",
            &root
        ),
        Plan::Doc
    );
}

#[test]
fn 页面资源按项目根解析() {
    let root = root();
    let plan = plan_request(
        "GET",
        "/site/css/a%20b.css",
        Some("style"),
        "site/index.html",
        &root,
    );
    assert_eq!(
        plan,
        Plan::File(root.join("site").join("css").join("a b.css"))
    );
    let plan = plan_request("GET", "/assets/app.js", None, "site/index.html", &root);
    assert_eq!(plan, Plan::File(root.join("assets").join("app.js")));
}

#[test]
fn 非网页资源与_fetch_一律拒绝() {
    let root = root();
    let doc = "index.html";
    assert_eq!(
        plan_request("GET", "/.env", None, doc, &root),
        Plan::Deny(403)
    );
    assert_eq!(
        plan_request("GET", "/config.json", Some("empty"), doc, &root),
        Plan::Deny(403)
    );
    assert_eq!(
        plan_request("GET", "/src/main.rs", Some("script"), doc, &root),
        Plan::Deny(403)
    );
    // 白名单里的扩展名,被 fetch 读也拒绝(浏览器 file:// 下同样读不到)
    assert_eq!(
        plan_request("GET", "/other.html", Some("empty"), doc, &root),
        Plan::Deny(403)
    );
    assert_eq!(
        plan_request("POST", "/a.css", None, doc, &root),
        Plan::Deny(405)
    );
}

#[test]
fn 越界与换盘符的路径拒绝() {
    let root = root();
    let doc = "index.html";
    assert_eq!(
        plan_request("GET", "/../x.css", None, doc, &root),
        Plan::Deny(400)
    );
    assert_eq!(
        plan_request("GET", "/a/%2E%2E/b.css", None, doc, &root),
        Plan::Deny(400)
    );
    assert_eq!(
        plan_request("GET", "/C:/Windows/a.css", None, doc, &root),
        Plan::Deny(400)
    );
    assert_eq!(
        plan_request("GET", "/a%5C..%5Cb.css", None, doc, &root),
        Plan::Deny(400)
    );
    assert_eq!(plan_request("GET", "/", None, doc, &root), Plan::Deny(400));
}

#[test]
fn 白名单大小写不敏感() {
    assert_eq!(mime_for("A.PNG"), Some("image/png"));
    assert_eq!(mime_for("x.Html"), Some("text/html"));
    assert_eq!(mime_for("Makefile"), None);
    assert_eq!(mime_for("a.json"), None);
}
