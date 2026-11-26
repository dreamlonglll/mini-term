use std::fs;
use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::archive::hex;
use crate::store::{DOWNLOADS_DIR, STALE_AFTER, TMP_PREFIX, is_enclosed_entry_name_for_test};
use crate::*;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 每个测试一个独立的临时目录(`exe/` 当 exe 目录、`root/` 当扩展根目录)。
struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mt-extensions-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("exe")).unwrap();
        fs::create_dir_all(path.join("root")).unwrap();
        Self(path)
    }

    fn exe(&self) -> PathBuf {
        self.0.join("exe")
    }

    fn root(&self) -> PathBuf {
        self.0.join("root")
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn extensions(&self, manifest: Manifest) -> Extensions {
        Extensions::new(
            Some(self.exe()),
            Some(self.root()),
            ManifestState::Loaded(manifest),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// 一个 zip 条目:名字、内容(一律 deflate + unix 0755)。
struct Entry<'a> {
    name: &'a str,
    bytes: &'a [u8],
}

fn entry<'a>(name: &'a str, bytes: &'a [u8]) -> Entry<'a> {
    Entry { name, bytes }
}

fn zip_of(entries: &[Entry<'_>]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for item in entries {
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o755);
        writer.start_file(item.name, options).unwrap();
        writer.write_all(item.bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// 把中央目录里第一条记录的 unix mode 改成 `mode`(zip 的写入 API 只收权限位,
/// 造不出 FIFO 之类的特殊文件类型,只能改字节)。
fn patch_first_mode(zip: &mut [u8], mode: u32) {
    let at = zip
        .windows(4)
        .position(|window| window == [0x50, 0x4b, 0x01, 0x02])
        .expect("central directory header");
    zip[at + 38..at + 42].copy_from_slice(&(mode << 16).to_le_bytes());
}

/// 由 zip 字节与其中文件描述出一个清单组件(与 `pack-extensions.mjs pack` 同一口径)。
fn component(id: ComponentId, zip: &[u8], files: &[(&str, &[u8], bool)]) -> Component {
    let sha256 = sha(zip);
    Component {
        id,
        asset: asset_name(id, &sha256),
        sha256,
        size: zip.len() as u64,
        files: files
            .iter()
            .map(|(name, bytes, executable)| ComponentFile {
                name: (*name).to_string(),
                sha256: sha(bytes),
                size: bytes.len() as u64,
                executable: *executable,
            })
            .collect(),
    }
}

fn manifest_with(components: Vec<Component>) -> Manifest {
    Manifest {
        app_version: "1.15.0".into(),
        tag: "v1.15.0".into(),
        platform: "windows-x64".into(),
        offline_bundle: Some("Mini-Term_1.15.0_windows-x64-extensions.zip".into()),
        components,
    }
}

const HELPER: ComponentId = ComponentId::GitHelperLinuxX86_64;
const HELPER_BYTES: &[u8] = b"\x7fELF fake static helper";

/// 一个合规的 helper 扩展:zip 字节 + 清单组件。
fn helper_zip() -> (Vec<u8>, Component) {
    let zip = zip_of(&[entry(GIT_HELPER_FILE, HELPER_BYTES)]);
    let component = component(HELPER, &zip, &[(GIT_HELPER_FILE, HELPER_BYTES, true)]);
    (zip, component)
}

fn manifest_json(components: &str) -> String {
    format!(
        r#"{{"schema":1,"appVersion":"1.15.0","tag":"v1.15.0","platform":"windows-x64","components":[{components}]}}"#
    )
}

fn component_json(component: &Component) -> String {
    let files: Vec<String> = component
        .files
        .iter()
        .map(|file| {
            format!(
                r#"{{"name":"{}","sha256":"{}","size":{},"executable":{}}}"#,
                file.name, file.sha256, file.size, file.executable
            )
        })
        .collect();
    format!(
        r#"{{"id":"{}","asset":"{}","sha256":"{}","size":{},"files":[{}]}}"#,
        component.id,
        component.asset,
        component.sha256,
        component.size,
        files.join(",")
    )
}

/// 解包暂存目录与下载暂存不留半成品。
fn assert_no_leftovers(root: &Path, id: ComponentId) {
    let id_dir = root.join(id.as_str());
    if let Ok(entries) = fs::read_dir(&id_dir) {
        let names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|name| !name.starts_with(TMP_PREFIX)),
            "暂存目录没清掉: {names:?}"
        );
        assert!(names.is_empty(), "失败的安装不该留下任何目录: {names:?}");
    }
    if let Ok(entries) = fs::read_dir(root.join(DOWNLOADS_DIR)) {
        assert_eq!(entries.count(), 0, "下载暂存没清掉");
    }
}

// ─── id ─────────────────────────────────────────────────────

#[test]
fn id_字符串一经发布不改且能反查() {
    let ids: Vec<&str> = ComponentId::ALL.iter().map(|id| id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "git-helper-linux-x86_64",
            "git-helper-linux-aarch64",
            "git-helper-macos-x86_64",
            "git-helper-macos-aarch64",
            "ssh-tools",
            "syntax-pack",
        ]
    );
    for id in ComponentId::ALL {
        assert_eq!(ComponentId::parse(id.as_str()), Some(id));
    }
    assert_eq!(ComponentId::parse("git-helper-linux-riscv64"), None);
    for (target, id) in [
        ("linux-x86_64", ComponentId::GitHelperLinuxX86_64),
        ("linux-aarch64", ComponentId::GitHelperLinuxAarch64),
        ("macos-x86_64", ComponentId::GitHelperMacosX86_64),
        ("macos-aarch64", ComponentId::GitHelperMacosAarch64),
    ] {
        assert_eq!(ComponentId::git_helper(target), Some(id));
        assert_eq!(id.helper_target(), Some(target));
        assert_eq!(id.kind(), ComponentKind::GitHelper);
    }
    assert_eq!(ComponentId::git_helper("windows-x64"), None);
    assert_eq!(ComponentId::SshTools.helper_target(), None);
}

// ─── 清单 ────────────────────────────────────────────────────

#[test]
fn 合规清单解析且忽略未知字段与未知_id() {
    let (_, helper) = helper_zip();
    let text = format!(
        r#"{{"schema":1,"appVersion":"1.15.0","tag":"v1.15.0","platform":"linux-x64",
            "offlineBundle":"Mini-Term_1.15.0_linux-x64-extensions.zip","futureField":{{"x":1}},
            "components":[
              {{"id":"future-thing","asset":"whatever","shape":"anything goes"}},
              {{"noId":true}},
              {extra}
            ]}}"#,
        extra = component_json(&helper).replacen('{', r#"{"futureKey":[1,2,3],"#, 1)
    );
    let manifest = parse_manifest(&text).unwrap();
    assert_eq!(manifest.platform, "linux-x64");
    assert_eq!(manifest.components, vec![helper.clone()]);
    assert_eq!(manifest.component(HELPER), Some(&helper));
    assert_eq!(manifest.component_by_asset(&helper.asset), Some(&helper));
    assert_eq!(
        manifest.component_by_digest(&helper.sha256, helper.size),
        Some(&helper)
    );
    assert_eq!(manifest.component(ComponentId::SshTools), None);
}

#[test]
fn schema_过新整份不认() {
    assert_eq!(
        parse_manifest(r#"{"schema":2,"whatever":true}"#),
        Err(ManifestError::TooNew { schema: 2 })
    );
    assert_eq!(
        ManifestState::from(parse_manifest(r#"{"schema":7}"#)),
        ManifestState::TooNew { schema: 7 }
    );
    assert!(matches!(
        parse_manifest(r#"{"schema":0}"#),
        Err(ManifestError::Invalid(_))
    ));
}

#[test]
fn 坏清单一律不认() {
    let (_, helper) = helper_zip();
    let good = component_json(&helper);
    let swap = |from: &str, to: &str| manifest_json(&good.replacen(from, to, 1));
    let bad_cases: Vec<(&str, String)> = vec![
        ("不是 JSON", "{".into()),
        ("不是对象", "[1]".into()),
        ("缺 schema", r#"{"tag":"v1"}"#.into()),
        ("schema 不是整数", r#"{"schema":"1"}"#.into()),
        (
            "缺 tag",
            r#"{"schema":1,"appVersion":"1","platform":"x","components":[]}"#.into(),
        ),
        (
            "tag 含非法字符",
            manifest_json("").replace(r#""tag":"v1.15.0""#, r#""tag":"v1/../x""#),
        ),
        (
            "资产名不是内容寻址",
            swap(
                &helper.asset,
                "mt-ext_git-helper-linux-x86_64_000000000000.zip",
            ),
        ),
        ("sha256 不是 64 位十六进制", swap(&helper.sha256, "abc")),
        (
            "zip 体积为 0",
            swap(
                &format!(r#""size":{},"files""#, helper.size),
                r#""size":0,"files""#,
            ),
        ),
        (
            "zip 体积超限",
            swap(
                &format!(r#""size":{},"files""#, helper.size),
                &format!(r#""size":{},"files""#, MAX_ZIP_BYTES + 1),
            ),
        ),
        (
            "文件名带分隔符",
            swap(r#""name":"mt-git-helper""#, r#""name":"bin/mt-git-helper""#),
        ),
        (
            "文件名带 ..",
            swap(r#""name":"mt-git-helper""#, r#""name":"..""#),
        ),
        (
            "文件名以点开头",
            swap(r#""name":"mt-git-helper""#, r#""name":".hidden""#),
        ),
        (
            "文件名是设备名",
            swap(r#""name":"mt-git-helper""#, r#""name":"NUL.txt""#),
        ),
        (
            "文件超限",
            swap(
                &format!(r#""size":{},"executable""#, HELPER_BYTES.len()),
                &format!(r#""size":{},"executable""#, MAX_FILE_BYTES + 1),
            ),
        ),
        (
            "没有文件",
            swap(&good[good.find(r#""files":"#).unwrap()..], r#""files":[]}"#),
        ),
        ("重复组件", manifest_json(&format!("{good},{good}"))),
        (
            "组件字段类型错",
            swap(
                &format!(r#""size":{},"files""#, helper.size),
                r#""size":"big","files""#,
            ),
        ),
    ];
    for (case, text) in bad_cases {
        assert!(
            matches!(parse_manifest(&text), Err(ManifestError::Invalid(_))),
            "{case} 应被拒绝: {text}"
        );
    }
    // 只差大小写的两个文件在 Windows / macOS 上会互相覆盖
    let mut twin = helper.clone();
    twin.files.push(ComponentFile {
        name: "MT-GIT-HELPER".into(),
        ..twin.files[0].clone()
    });
    assert!(matches!(
        parse_manifest(&manifest_json(&component_json(&twin))),
        Err(ManifestError::Invalid(_))
    ));
}

#[test]
fn 文件名只许单段安全名() {
    for ok in [
        "mt-git-helper",
        "mt-ssh-cli.exe",
        "libmt_syntax_pack.so",
        "a b",
        "主题",
    ] {
        assert!(is_safe_file_name(ok), "应放行 {ok:?}");
    }
    for bad in [
        "", ".", "..", ".hidden", "a/b", "a\\b", "C:x", "x..y", "trail.", "trail ", "a\u{0}b",
        "CON", "nul", "com1.dll", "LPT9", "a<b", "a|b", "a?b", "a*b",
    ] {
        assert!(!is_safe_file_name(bad), "应拒绝 {bad:?}");
    }
    assert!(!is_safe_file_name(&"x".repeat(129)));
}

#[test]
fn 清单文件缺失是常态_读不懂是无效() {
    let fixture = Fixture::new();
    assert_eq!(
        load_manifest(&fixture.0.join("absent.json")),
        ManifestState::Missing
    );
    let broken = fixture.write("broken.json", b"{not json");
    assert!(matches!(load_manifest(&broken), ManifestState::Invalid(_)));
    let newer = fixture.write("newer.json", br#"{"schema":9}"#);
    assert_eq!(load_manifest(&newer), ManifestState::TooNew { schema: 9 });
    let (_, helper) = helper_zip();
    let good = fixture.write(
        "good.json",
        manifest_json(&component_json(&helper)).as_bytes(),
    );
    assert_eq!(
        load_manifest(&good).manifest().unwrap().components,
        vec![helper]
    );
}

#[test]
fn 扩展根目录口径() {
    let local = Some(PathBuf::from("/local"));
    assert_eq!(
        root_for(None, local.clone()),
        Some(
            PathBuf::from("/local")
                .join(APP_IDENTIFIER)
                .join(EXTENSIONS_DIR)
        )
    );
    // 空串按「没设」处理
    assert_eq!(
        root_for(Some(std::ffi::OsStr::new("")), local.clone()),
        Some(
            PathBuf::from("/local")
                .join(APP_IDENTIFIER)
                .join(EXTENSIONS_DIR)
        )
    );
    assert_eq!(
        root_for(Some(std::ffi::OsStr::new("/dev-data")), local),
        Some(PathBuf::from("/dev-data").join(EXTENSIONS_DIR))
    );
    assert_eq!(root_for(None, None), None);
}

// ─── 安装 ────────────────────────────────────────────────────

#[test]
fn 合规扩展原子落位且幂等() {
    let fixture = Fixture::new();
    let (zip, helper) = helper_zip();
    let extensions = fixture.extensions(manifest_with(vec![helper.clone()]));
    let zip_path = fixture.write("helper.zip", &zip);
    assert_eq!(extensions.status(HELPER), ComponentStatus::Missing);

    let dir = extensions.install_zip(HELPER, &zip_path).unwrap();
    assert_eq!(
        dir,
        fixture
            .root()
            .join(HELPER.as_str())
            .join(&helper.sha256[..16])
    );
    assert_eq!(fs::read(dir.join(GIT_HELPER_FILE)).unwrap(), HELPER_BYTES);
    assert_eq!(
        extensions.status(HELPER),
        ComponentStatus::Installed(dir.clone())
    );
    // 正式目录以外什么都没留下
    let siblings: Vec<_> = fs::read_dir(fixture.root().join(HELPER.as_str()))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(siblings.len(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(dir.join(GIT_HELPER_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }
    // 再装一次:原样返回
    assert_eq!(extensions.install_zip(HELPER, &zip_path).unwrap(), dir);

    // 正式目录被改坏(截断)了:当没装,重装时整个换掉
    fs::write(dir.join(GIT_HELPER_FILE), b"x").unwrap();
    assert_eq!(extensions.status(HELPER), ComponentStatus::Missing);
    assert_eq!(
        extensions.locate(HELPER, GIT_HELPER_FILE, None).unwrap(),
        None
    );
    assert_eq!(extensions.install_zip(HELPER, &zip_path).unwrap(), dir);
    assert_eq!(fs::read(dir.join(GIT_HELPER_FILE)).unwrap(), HELPER_BYTES);
}

/// 解包出来的文件 mtime 是**安装时刻**,不是 zip 里固定的 1980-01-01(pack-extensions.mjs 为了
/// 逐字节可复现把条目 mtime 钉死了)。mt-ai 的 SKILL.md 启动校正靠「本机 CLI 的 mtime 严格更新」
/// 把引用从旧版本目录收敛到新版本目录(旧目录被 GC 多留一份,「引用的 CLI 不在」那条不再必然
/// 触发)—— 要是解包时照搬了 zip 里的 mtime,新旧两份一样旧,就永远不改指了。
#[test]
fn 解包出来的文件_mtime_是安装时刻() {
    let fixture = Fixture::new();
    let (zip, helper) = helper_zip();
    let extensions = fixture.extensions(manifest_with(vec![helper]));
    let before = SystemTime::now() - Duration::from_secs(5);
    let dir = extensions
        .install_zip(HELPER, &fixture.write("h.zip", &zip))
        .unwrap();
    let modified = fs::metadata(dir.join(GIT_HELPER_FILE))
        .unwrap()
        .modified()
        .unwrap();
    assert!(modified >= before, "{modified:?} 早于安装时刻 {before:?}");
}

#[test]
fn zip_哈希或大小不符一律拒且不留半成品() {
    let fixture = Fixture::new();
    let (zip, helper) = helper_zip();
    let extensions = fixture.extensions(manifest_with(vec![helper.clone()]));

    let mut tampered = zip.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xff;
    let tampered_path = fixture.write("tampered.zip", &tampered);
    assert!(matches!(
        extensions.install_zip(HELPER, &tampered_path),
        Err(ExtensionError::Integrity(_))
    ));

    let mut longer = zip.clone();
    longer.push(0);
    let longer_path = fixture.write("longer.zip", &longer);
    assert!(matches!(
        extensions.install_zip(HELPER, &longer_path),
        Err(ExtensionError::Integrity(_))
    ));
    assert_no_leftovers(&fixture.root(), HELPER);
}

/// 清单按「坏 zip」重算哈希:zip 本身过了第一道,第二道逐条目校验才拒。
fn reject_contents(entries: &[Entry<'_>], files: &[(&str, &[u8], bool)]) {
    let fixture = Fixture::new();
    let zip = zip_of(entries);
    let component = component(HELPER, &zip, files);
    let extensions = fixture.extensions(manifest_with(vec![component]));
    let path = fixture.write("bad.zip", &zip);
    let error = extensions.install_zip(HELPER, &path).unwrap_err();
    assert!(
        matches!(error, ExtensionError::Integrity(_)),
        "应按校验失败拒绝: {error}"
    );
    assert_no_leftovers(&fixture.root(), HELPER);
    assert_eq!(extensions.status(HELPER), ComponentStatus::Missing);
}

#[test]
fn 多余条目_缺条目_重名都拒() {
    let helper = (GIT_HELPER_FILE, HELPER_BYTES, true);
    // 多一个
    reject_contents(
        &[
            entry(GIT_HELPER_FILE, HELPER_BYTES),
            entry("extra.txt", b"x"),
        ],
        &[helper],
    );
    // 少一个
    reject_contents(
        &[entry(GIT_HELPER_FILE, HELPER_BYTES)],
        &[helper, ("second", b"2", false)],
    );
    // 条目数对得上但名字不在清单里
    reject_contents(&[entry("other-name", HELPER_BYTES)], &[helper]);
}

#[test]
fn 越界路径一律拒() {
    for evil in [
        "../mt-git-helper",
        "/mt-git-helper",
        "sub/mt-git-helper",
        "C:mt-git-helper",
        "..\\mt-git-helper",
    ] {
        reject_contents(
            &[entry(evil, HELPER_BYTES)],
            &[(GIT_HELPER_FILE, HELPER_BYTES, true)],
        );
    }
}

#[test]
fn 单文件哈希不符拒() {
    // 声明的字节数一致,内容不同
    let wrong = b"\x7fELF fake static helpeR";
    assert_eq!(wrong.len(), HELPER_BYTES.len());
    let fixture = Fixture::new();
    let zip = zip_of(&[entry(GIT_HELPER_FILE, wrong)]);
    let component = component(HELPER, &zip, &[(GIT_HELPER_FILE, HELPER_BYTES, true)]);
    let extensions = fixture.extensions(manifest_with(vec![component]));
    let path = fixture.write("bad.zip", &zip);
    let error = extensions.install_zip(HELPER, &path).unwrap_err();
    assert!(error.to_string().contains("sha256"), "{error}");
    assert_no_leftovers(&fixture.root(), HELPER);

    // 字节数不同(zip 里声明的大小就对不上)
    reject_contents(
        &[entry(GIT_HELPER_FILE, b"short")],
        &[(GIT_HELPER_FILE, HELPER_BYTES, true)],
    );
}

#[test]
fn 符号链接与非常规条目拒() {
    let fixture = Fixture::new();
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .add_symlink(
            GIT_HELPER_FILE,
            "/usr/bin/git",
            SimpleFileOptions::default(),
        )
        .unwrap();
    let zip = writer.finish().unwrap().into_inner();
    let component = component(HELPER, &zip, &[(GIT_HELPER_FILE, b"/usr/bin/git", true)]);
    let extensions = fixture.extensions(manifest_with(vec![component]));
    let path = fixture.write("link.zip", &zip);
    assert!(matches!(
        extensions.install_zip(HELPER, &path),
        Err(ExtensionError::Integrity(_))
    ));
    assert_no_leftovers(&fixture.root(), HELPER);

    // 带 FIFO 文件类型位的条目
    let fixture = Fixture::new();
    let mut fifo = zip_of(&[entry(GIT_HELPER_FILE, HELPER_BYTES)]);
    patch_first_mode(&mut fifo, 0o010_644);
    let fifo_component = self::component(HELPER, &fifo, &[(GIT_HELPER_FILE, HELPER_BYTES, true)]);
    let extensions = fixture.extensions(manifest_with(vec![fifo_component]));
    let error = extensions
        .install_zip(HELPER, &fixture.write("fifo.zip", &fifo))
        .unwrap_err();
    assert!(error.to_string().contains("non-regular"), "{error}");
    assert_no_leftovers(&fixture.root(), HELPER);
}

#[test]
fn 没有清单或不在清单里不装() {
    let fixture = Fixture::new();
    let (zip, helper) = helper_zip();
    let path = fixture.write("helper.zip", &zip);
    let none = Extensions::new(
        Some(fixture.exe()),
        Some(fixture.root()),
        ManifestState::Missing,
    );
    assert_eq!(
        none.install_zip(HELPER, &path),
        Err(ExtensionError::NoManifest)
    );
    assert_eq!(
        none.import_offline(&path).unwrap_err(),
        ExtensionError::NoManifest
    );
    let newer = Extensions::new(
        Some(fixture.exe()),
        Some(fixture.root()),
        ManifestState::TooNew { schema: 2 },
    );
    assert_eq!(
        newer.install_zip(HELPER, &path),
        Err(ExtensionError::ManifestTooNew { schema: 2 })
    );
    let extensions = fixture.extensions(manifest_with(vec![helper]));
    assert_eq!(
        extensions.install_zip(ComponentId::SshTools, &path),
        Err(ExtensionError::NotInManifest(ComponentId::SshTools))
    );
    let rootless = Extensions::new(
        Some(fixture.exe()),
        None,
        extensions.manifest_state().clone(),
    );
    assert_eq!(
        rootless.install_zip(HELPER, &path),
        Err(ExtensionError::NoRoot)
    );
}

#[test]
fn 删除扩展() {
    let fixture = Fixture::new();
    let (zip, helper) = helper_zip();
    let extensions = fixture.extensions(manifest_with(vec![helper]));
    extensions.remove(HELPER).unwrap(); // 没装也算成功
    extensions
        .install_zip(HELPER, &fixture.write("h.zip", &zip))
        .unwrap();
    extensions.remove(HELPER).unwrap();
    assert!(!fixture.root().join(HELPER.as_str()).exists());
    assert_eq!(extensions.status(HELPER), ComponentStatus::Missing);
}

// ─── 查找 ────────────────────────────────────────────────────

#[test]
fn locate_按旧布局_开发根目录_已装扩展的顺序() {
    let fixture = Fixture::new();
    let (zip, helper) = helper_zip();
    let extensions = fixture.extensions(manifest_with(vec![helper]));
    let dev_root = fixture.0.join("dev");
    assert_eq!(
        extensions
            .locate(HELPER, GIT_HELPER_FILE, Some(&dev_root))
            .unwrap(),
        None,
        "都没有 = 未安装"
    );

    // ③ 已装扩展,带清单里的期望值
    let dir = extensions
        .install_zip(HELPER, &fixture.write("h.zip", &zip))
        .unwrap();
    let installed = extensions
        .locate(HELPER, GIT_HELPER_FILE, Some(&dev_root))
        .unwrap()
        .unwrap();
    assert_eq!(installed.source, LocatedSource::Installed);
    assert_eq!(installed.path, dir.join(GIT_HELPER_FILE));
    assert_eq!(
        installed.expected,
        Some((sha(HELPER_BYTES), HELPER_BYTES.len() as u64))
    );
    // 清单里没有的文件名找不到
    assert_eq!(extensions.locate(HELPER, "other", None).unwrap(), None);

    // ② 开发根目录压过已装扩展(只对 helper)
    let dev = fixture.write("dev/linux-x86_64/mt-git-helper", b"dev");
    let found = extensions
        .locate(HELPER, GIT_HELPER_FILE, Some(&dev_root))
        .unwrap()
        .unwrap();
    assert_eq!(
        (found.path, found.source),
        (dev.clone(), LocatedSource::DevRoot)
    );

    // ① 有效清单在场(正式安装包):exe 旁旧布局一律不认 —— tar.gz 覆盖解压留下的旧 helper
    // 不能压住下载的那份
    let bundled = fixture.write("exe/remote-git-helpers/linux-x86_64/mt-git-helper", b"");
    let found = extensions
        .locate(HELPER, GIT_HELPER_FILE, Some(&dev_root))
        .unwrap()
        .unwrap();
    assert_eq!((found.path, found.source), (dev, LocatedSource::DevRoot));
    let found = extensions
        .locate(HELPER, GIT_HELPER_FILE, None)
        .unwrap()
        .unwrap();
    assert_eq!(found.source, LocatedSource::Installed);
    assert_eq!(extensions.status(HELPER), ComponentStatus::Installed(dir));

    // 已装的被删了:旧布局照样不认,就是未安装
    extensions.remove(HELPER).unwrap();
    assert_eq!(
        extensions.locate(HELPER, GIT_HELPER_FILE, None).unwrap(),
        None
    );
    assert_eq!(extensions.status(HELPER), ComponentStatus::Missing);

    // 没有有效清单(dev / 源码构建):旧布局压过一切 —— 哪怕它是坏的(空文件):合不合规由
    // 调用方判,不静默落到下一层
    for state in [
        ManifestState::Missing,
        ManifestState::TooNew { schema: 2 },
        ManifestState::Invalid("broken".into()),
    ] {
        let dev_build = Extensions::new(Some(fixture.exe()), Some(fixture.root()), state);
        let found = dev_build
            .locate(HELPER, GIT_HELPER_FILE, Some(&dev_root))
            .unwrap()
            .unwrap();
        assert_eq!(
            (found.path, found.source),
            (bundled.clone(), LocatedSource::Bundled)
        );
        assert_eq!(found.expected, None);
        // 别的架构的旧布局不会被拿来顶替
        assert_eq!(
            dev_build
                .locate(ComponentId::GitHelperLinuxAarch64, GIT_HELPER_FILE, None)
                .unwrap(),
            None
        );
    }
}

#[test]
fn 有效清单在场时_exe_旁的_ssh_工具旧布局不认() {
    let fixture = Fixture::new();
    let cli: &[u8] = b"cli bytes";
    let mcp: &[u8] = b"mcp bytes";
    let zip = zip_of(&[entry(SSH_CLI_FILE, cli), entry(SSH_MCP_FILE, mcp)]);
    let tools = component(
        ComponentId::SshTools,
        &zip,
        &[(SSH_CLI_FILE, cli, true), (SSH_MCP_FILE, mcp, true)],
    );
    let extensions = fixture.extensions(manifest_with(vec![tools]));
    // 旧版本(1.14.x)留在 exe 旁的两个 sidecar
    let old_cli = fixture.write(&format!("exe/{SSH_CLI_FILE}"), b"old cli");
    fixture.write(&format!("exe/{SSH_MCP_FILE}"), b"old mcp");
    assert_eq!(
        extensions
            .locate(ComponentId::SshTools, SSH_CLI_FILE, None)
            .unwrap(),
        None
    );
    assert_eq!(
        extensions.status(ComponentId::SshTools),
        ComponentStatus::Missing
    );
    let dir = extensions
        .install_zip(ComponentId::SshTools, &fixture.write("tools.zip", &zip))
        .unwrap();
    let found = extensions
        .locate(ComponentId::SshTools, SSH_CLI_FILE, None)
        .unwrap()
        .unwrap();
    assert_eq!(
        (found.path, found.source),
        (dir.join(SSH_CLI_FILE), LocatedSource::Installed)
    );
    // 没有清单时同一个旧布局就是 dev 的就位产物
    let dev_build = Extensions::new(
        Some(fixture.exe()),
        Some(fixture.root()),
        ManifestState::Missing,
    );
    assert_eq!(
        dev_build
            .locate(ComponentId::SshTools, SSH_CLI_FILE, None)
            .unwrap()
            .map(|found| (found.path, found.source)),
        Some((old_cli, LocatedSource::Bundled))
    );
}

#[test]
fn 没有清单时只认旧布局与开发根目录() {
    let fixture = Fixture::new();
    let extensions = Extensions::new(
        Some(fixture.exe()),
        Some(fixture.root()),
        ManifestState::Missing,
    );
    assert_eq!(
        extensions.locate(HELPER, GIT_HELPER_FILE, None).unwrap(),
        None
    );
    let dev = fixture.write("dev/linux-x86_64/mt-git-helper", b"dev");
    assert_eq!(
        extensions
            .locate(HELPER, GIT_HELPER_FILE, Some(&fixture.0.join("dev")))
            .unwrap()
            .map(|found| found.path),
        Some(dev)
    );
    // 非 helper 不认开发根目录
    fixture.write("dev/ssh-tools/mt-ssh-cli", b"x");
    assert_eq!(
        extensions
            .locate(
                ComponentId::SshTools,
                "mt-ssh-cli",
                Some(&fixture.0.join("dev"))
            )
            .unwrap(),
        None
    );
    let cli = fixture.write("exe/mt-ssh-cli", b"cli");
    assert_eq!(
        extensions
            .locate(ComponentId::SshTools, "mt-ssh-cli", None)
            .unwrap()
            .map(|found| (found.path, found.source)),
        Some((cli, LocatedSource::Bundled))
    );
}

// ─── GC ──────────────────────────────────────────────────────

/// 把一个文件的修改时间设成「纪元 + `secs` 秒」。
fn set_modified(path: &Path, secs: u64) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

#[test]
fn gc_每个已知_id_留当前那份加最近装的一份其它版本() {
    let fixture = Fixture::new();
    let (zip, helper) = helper_zip();
    let extensions = fixture.extensions(manifest_with(vec![helper.clone()]));
    let current = extensions
        .install_zip(HELPER, &fixture.write("h.zip", &zip))
        .unwrap();
    let root = fixture.root();
    // 当前清单之外的两份 helper 版本:较新的留,较旧的删
    let older = fixture.write(
        "root/git-helper-linux-x86_64/0123456789abcdef/mt-git-helper",
        b"older",
    );
    let newer = fixture.write(
        "root/git-helper-linux-x86_64/fedcba9876543210/mt-git-helper",
        b"newer",
    );
    set_modified(&older, 1_000_000_000);
    set_modified(&newer, 1_500_000_000);
    // 当前清单里没有 ssh-tools:只留最近一份
    let tools_old = fixture.write("root/ssh-tools/1111111111111111/mt-ssh-cli", b"x");
    let tools_new = fixture.write("root/ssh-tools/2222222222222222/mt-ssh-cli", b"y");
    set_modified(&tools_old, 1_000_000_000);
    set_modified(&tools_new, 1_700_000_000);
    let fresh_tmp = root.join(HELPER.as_str()).join(".tmp-inflight");
    fs::create_dir_all(&fresh_tmp).unwrap();
    let stray_in_id = fixture.write("root/git-helper-linux-x86_64/stray.txt", b"x");
    let unknown = fixture.write("root/some-future-extension/abc/file", b"x");
    let download = fixture.write("root/.downloads/x.zip.part", b"partial");
    let stray = fixture.write("root/notes.txt", b"leave me");
    let other_dot = fixture.write("root/.lock/x", b"leave me");

    // 「现在」:暂存物都是新的,只删版本 / id 目录
    let report = extensions.gc_at(SystemTime::now());
    assert!(report.kept.is_empty(), "{report:?}");
    assert!(current.join(GIT_HELPER_FILE).is_file());
    assert!(newer.exists(), "最近装的那份其它版本要留着");
    assert!(!older.parent().unwrap().exists());
    assert!(tools_new.exists(), "清单里没有的已知 id 也留最近一份");
    assert!(!tools_old.parent().unwrap().exists());
    assert!(!stray_in_id.exists(), "id 目录里散落的文件不是版本目录");
    assert!(!unknown.parent().unwrap().parent().unwrap().exists());
    assert!(fresh_tmp.exists(), "正在解包的那份不能删");
    assert!(download.exists(), "正在下载的那份不能删");
    assert!(stray.exists());
    assert!(other_dot.exists());

    // 过了 10 分钟:暂存物都算陈旧;再跑一遍,留下的那两份其它版本照留(幂等)
    let later = SystemTime::now() + STALE_AFTER + Duration::from_secs(1);
    extensions.gc_at(later);
    assert!(!fresh_tmp.exists());
    assert!(!download.exists());
    assert!(newer.exists());
    assert!(tools_new.exists());
    assert!(current.join(GIT_HELPER_FILE).is_file());
    assert_eq!(
        extensions.status(HELPER),
        ComponentStatus::Installed(current)
    );
}

/// 删除要么整个删掉、要么原样不动:Windows 上版本目录里有运行中的 exe(mt-ssh-cli 的 daemon,
/// 工作目录也在那里)时 GC / `remove` 都把它原样留着(进 `kept` / 报错),不删成只剩 exe 的半截;
/// 进程退出后下次 GC 删掉,回收区不留东西。
#[cfg(windows)]
#[test]
fn 被占用的版本目录删不掉就原样留着() {
    let fixture = Fixture::new();
    let (_, helper) = helper_zip();
    let extensions = fixture.extensions(manifest_with(vec![helper]));
    let busy = fixture.root().join("ssh-tools").join("0123456789abcdef");
    fs::create_dir_all(&busy).unwrap();
    let exe = busy.join("p.exe");
    fs::copy(r"C:\Windows\System32\PING.EXE", &exe).unwrap();
    fs::write(busy.join("mt-ssh-mcp.exe"), b"sibling").unwrap();
    // 让被占用的那份成为「较旧、该删」的(运行中的 exe 打不开写,得在拉起前设)
    set_modified(&exe, 1_000_000_000);
    set_modified(&busy.join("mt-ssh-mcp.exe"), 1_000_000_000);
    let mut child = std::process::Command::new(&exe)
        .args(["-n", "30", "127.0.0.1"])
        .current_dir(&busy)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    // 另一份更新的其它版本,让被占用的那份成为该删的
    let newer = fixture.write("root/ssh-tools/fedcba9876543210/mt-ssh-cli.exe", b"x");
    let report = extensions.gc_at(SystemTime::now());
    let removed_while_busy = extensions.remove(ComponentId::SshTools);
    let intact = busy.join("mt-ssh-mcp.exe").is_file() && exe.is_file();
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(report.kept, vec![busy.clone()], "{report:?}");
    assert!(intact, "被占用的版本目录不能删成半截");
    assert!(removed_while_busy.is_err());
    assert!(newer.exists());

    let report = extensions.gc_at(SystemTime::now());
    assert!(report.kept.is_empty(), "{report:?}");
    assert!(!busy.exists());
    let leftovers: Vec<_> = fs::read_dir(fixture.root().join("ssh-tools"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers, vec!["fedcba9876543210".to_string()]);
}

/// 「最近装的」看目录里文件的 mtime,不看目录自身:Windows 上删一个被占用的旧版本目录会删掉
/// 一部分文件、目录 mtime 跟着变成「现在」,不能让这份残缺的冒充最近的那份。
#[test]
fn gc_按版本目录里文件的_mtime_认最近装的那份() {
    let fixture = Fixture::new();
    let (_, helper) = helper_zip();
    let extensions = fixture.extensions(manifest_with(vec![helper]));
    // 残缺的旧版本:目录刚被动过(mtime = 现在),里面剩下的文件是很久以前装的
    let partial = fixture.write(
        "root/git-helper-linux-x86_64/0123456789abcdef/mt-git-helper",
        b"partial",
    );
    set_modified(&partial, 1_000_000_000);
    let complete = fixture.write(
        "root/git-helper-linux-x86_64/fedcba9876543210/mt-git-helper",
        b"complete",
    );
    set_modified(&complete, 1_500_000_000);
    fixture.write(
        "root/git-helper-linux-x86_64/0123456789abcdef/touched-just-now",
        b"",
    );
    fs::remove_file(partial.with_file_name("touched-just-now")).unwrap();
    extensions.gc_at(SystemTime::now());
    assert!(complete.exists());
    assert!(!partial.parent().unwrap().exists());
}

#[test]
fn gc_没有清单时什么都不删() {
    let fixture = Fixture::new();
    let kept = fixture.write(
        "root/git-helper-linux-x86_64/0123456789abcdef/mt-git-helper",
        b"x",
    );
    let unknown = fixture.write("root/whatever/abc/file", b"x");
    for state in [
        ManifestState::Missing,
        ManifestState::TooNew { schema: 2 },
        ManifestState::Invalid("broken".into()),
    ] {
        let extensions = Extensions::new(Some(fixture.exe()), Some(fixture.root()), state);
        assert_eq!(
            extensions.gc_at(SystemTime::now() + Duration::from_secs(86_400)),
            GcReport::default()
        );
    }
    assert!(kept.exists());
    assert!(unknown.exists());
}

// ─── 离线导入 ────────────────────────────────────────────────

/// 离线包:条目原样 store(与 `pack-extensions.mjs bundle` 同一口径)。
fn bundle_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        writer
            .start_file(
                *name,
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn arm_helper() -> (Vec<u8>, Component) {
    let bytes: &[u8] = b"\x7fELF aarch64 helper";
    let zip = zip_of(&[entry(GIT_HELPER_FILE, bytes)]);
    let component = component(
        ComponentId::GitHelperLinuxAarch64,
        &zip,
        &[(GIT_HELPER_FILE, bytes, true)],
    );
    (zip, component)
}

#[test]
fn 离线包按本机清单匹配并跳过不认识的条目() {
    let fixture = Fixture::new();
    let (x64_zip, x64) = helper_zip();
    let (arm_zip, arm) = arm_helper();
    let extensions = fixture.extensions(manifest_with(vec![x64.clone(), arm.clone()]));
    let bundle = bundle_of(&[
        (&x64.asset, &x64_zip),
        // 重新打过一层目录的也认(取最后一段)
        (&format!("nested/{}", arm.asset), &arm_zip),
        // 别的版本的资产:不认识,跳过并计数
        (
            "mt-ext_git-helper-macos-x86_64_ffffffffffff.zip",
            b"other version",
        ),
        ("README.txt", b"hi"),
        // 随包的清单副本不信任、也不计数
        (MANIFEST_FILE, b"{\"schema\":1}"),
    ]);
    let path = fixture.write("bundle.zip", &bundle);
    let report = extensions.import_offline(&path).unwrap();
    assert_eq!(report.installed, vec![x64.id, arm.id]);
    assert_eq!(report.skipped, 2);
    assert!(report.failed.is_empty());
    assert!(matches!(
        extensions.status(x64.id),
        ComponentStatus::Installed(_)
    ));
    assert!(matches!(
        extensions.status(arm.id),
        ComponentStatus::Installed(_)
    ));
    assert_eq!(
        fs::read_dir(fixture.root().join(DOWNLOADS_DIR))
            .unwrap()
            .count(),
        0,
        "解出来的临时 zip 要删掉"
    );
}

#[test]
fn 离线包里坏掉的资产单独失败_其余照装() {
    let fixture = Fixture::new();
    let (x64_zip, x64) = helper_zip();
    let (_, arm) = arm_helper();
    let extensions = fixture.extensions(manifest_with(vec![x64.clone(), arm.clone()]));
    let bundle = bundle_of(&[(&x64.asset, &x64_zip), (&arm.asset, b"corrupted")]);
    let report = extensions
        .import_offline(&fixture.write("bundle.zip", &bundle))
        .unwrap();
    assert_eq!(report.installed, vec![x64.id]);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, arm.id);
    assert_eq!(extensions.status(arm.id), ComponentStatus::Missing);
    assert_no_leftovers(&fixture.root(), arm.id);
}

#[test]
fn 单个扩展_zip_也能导入() {
    let fixture = Fixture::new();
    let (zip, helper) = helper_zip();
    let extensions = fixture.extensions(manifest_with(vec![helper.clone()]));
    let report = extensions
        .import_offline(&fixture.write(&helper.asset, &zip))
        .unwrap();
    assert_eq!(report.installed, vec![HELPER]);
    assert!(matches!(
        extensions.status(HELPER),
        ComponentStatus::Installed(_)
    ));
}

#[test]
fn 离线包越界或符号链接整包拒绝() {
    let fixture = Fixture::new();
    let (zip, helper) = helper_zip();
    let extensions = fixture.extensions(manifest_with(vec![helper.clone()]));
    for evil in [
        format!("../{}", helper.asset),
        format!("/{}", helper.asset),
        format!("a\\{}", helper.asset),
        format!("C:{}", helper.asset),
    ] {
        let bundle = bundle_of(&[(&helper.asset, &zip), (&evil, &zip)]);
        assert!(
            matches!(
                extensions.import_offline(&fixture.write("evil.zip", &bundle)),
                Err(ExtensionError::Integrity(_))
            ),
            "{evil}"
        );
        assert_eq!(
            extensions.status(HELPER),
            ComponentStatus::Missing,
            "整包拒绝 = 什么都没装"
        );
    }
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .add_symlink(
            helper.asset.clone(),
            "/etc/passwd",
            SimpleFileOptions::default(),
        )
        .unwrap();
    let link = writer.finish().unwrap().into_inner();
    assert!(matches!(
        extensions.import_offline(&fixture.write("link.zip", &link)),
        Err(ExtensionError::Integrity(_))
    ));
    // 既不是本版本的资产,也不是 zip
    assert!(matches!(
        extensions.import_offline(&fixture.write("junk.bin", b"junk")),
        Err(ExtensionError::Integrity(_))
    ));
}

#[test]
fn 离线包条目名越界判定() {
    for ok in ["a.zip", "dir/a.zip", "dir/", "a/b/c.zip"] {
        assert!(is_enclosed_entry_name_for_test(ok), "{ok}");
    }
    for bad in [
        "", "/a", "../a", "a/../b", "a//b", "./a", "a\\b", "C:a", "a\0b", "/",
    ] {
        assert!(!is_enclosed_entry_name_for_test(bad), "{bad:?}");
    }
}

#[test]
fn 下载暂存路径落在根目录的下载区() {
    let fixture = Fixture::new();
    let (_, helper) = helper_zip();
    let extensions = fixture.extensions(manifest_with(vec![helper.clone()]));
    let a = extensions.new_download_path(HELPER).unwrap();
    let b = extensions.new_download_path(HELPER).unwrap();
    assert_ne!(a, b);
    assert_eq!(a.parent().unwrap(), fixture.root().join(DOWNLOADS_DIR));
    let name = a.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with(&helper.asset) && name.ends_with(".part"),
        "{name}"
    );
}
