//! 与 `scripts/pack-extensions.mjs` 的往返:`tests/fixtures/` 是那个脚本打出来的一份夹具
//! (两个 helper 扩展 + 清单 + 离线包)。`tests/packExtensions.test.cjs` 断言脚本现在仍逐字节
//! 产出同样的东西,这里断言本 crate 的安装代码装得上 —— 两头一夹,谁的口径漂了都有一边红。
//!
//! 另有一条 `#[ignore]` 的本地往返:指向一份用真实文件现打的产物
//! (`MT_EXT_ROUNDTRIP_MANIFEST` / `MT_EXT_ROUNDTRIP_ASSETS` / `MT_EXT_ROUNDTRIP_BUNDLE`),
//! `cargo test -p mt-extensions -- --ignored` 跑。

use std::fs;
use std::path::{Path, PathBuf};

use mt_extensions::{
    ComponentId, ComponentStatus, Extensions, GIT_HELPER_FILE, LocatedSource, ManifestState,
    load_manifest,
};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "mt-extensions-fixture-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn loaded(path: &Path) -> ManifestState {
    let state = load_manifest(path);
    assert!(
        matches!(state, ManifestState::Loaded(_)),
        "{} 应能解析: {state:?}",
        path.display()
    );
    state
}

#[test]
fn 打包脚本的夹具逐个资产装得上() {
    let fixtures = fixtures();
    let state = loaded(&fixtures.join("extensions.json"));
    let manifest = state.manifest().unwrap().clone();
    assert_eq!(manifest.platform, "windows-x64");
    assert_eq!(
        manifest.components.iter().map(|c| c.id).collect::<Vec<_>>(),
        [
            ComponentId::GitHelperLinuxAarch64,
            ComponentId::GitHelperLinuxX86_64
        ]
    );
    let temp = TempRoot::new("assets");
    let extensions = Extensions::new(None, Some(temp.0.clone()), state);
    for (component, input) in [
        (ComponentId::GitHelperLinuxX86_64, "x64"),
        (ComponentId::GitHelperLinuxAarch64, "arm64"),
    ] {
        let asset = &extensions.component(component).unwrap().asset;
        let dir = extensions
            .install_zip(component, &fixtures.join("assets").join(asset))
            .unwrap();
        assert_eq!(
            fs::read(dir.join(GIT_HELPER_FILE)).unwrap(),
            fs::read(fixtures.join("input").join(input).join(GIT_HELPER_FILE)).unwrap(),
            "{component} 解出来的内容应与打包输入逐字节一致"
        );
        let found = extensions
            .locate(component, GIT_HELPER_FILE, None)
            .unwrap()
            .unwrap();
        assert_eq!(found.source, LocatedSource::Installed);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&found.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "--exe 打的文件装成 0755");
        }
    }
}

#[test]
fn 打包脚本的离线包导入得上() {
    let fixtures = fixtures();
    let state = loaded(&fixtures.join("extensions.json"));
    let bundle = fixtures.join(state.manifest().unwrap().offline_bundle.clone().unwrap());
    let temp = TempRoot::new("bundle");
    let extensions = Extensions::new(None, Some(temp.0.clone()), state);
    let report = extensions.import_offline(&bundle).unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.skipped, 0, "随包的 extensions.json 不计入跳过");
    assert_eq!(report.installed.len(), 2);
    for id in [
        ComponentId::GitHelperLinuxX86_64,
        ComponentId::GitHelperLinuxAarch64,
    ] {
        assert!(matches!(
            extensions.status(id),
            ComponentStatus::Installed(_)
        ));
    }
}

/// 本地往返:用真实文件现打一份(见文件头),再交给安装代码。
#[test]
#[ignore = "需要 MT_EXT_ROUNDTRIP_* 指向 pack-extensions.mjs 现打的产物"]
fn 真实产物往返() {
    let manifest = PathBuf::from(
        std::env::var_os("MT_EXT_ROUNDTRIP_MANIFEST").expect("MT_EXT_ROUNDTRIP_MANIFEST"),
    );
    let assets = PathBuf::from(
        std::env::var_os("MT_EXT_ROUNDTRIP_ASSETS").expect("MT_EXT_ROUNDTRIP_ASSETS"),
    );
    let bundle = PathBuf::from(
        std::env::var_os("MT_EXT_ROUNDTRIP_BUNDLE").expect("MT_EXT_ROUNDTRIP_BUNDLE"),
    );
    let state = loaded(&manifest);
    let components = state.manifest().unwrap().components.clone();
    assert!(!components.is_empty());

    let one_by_one = TempRoot::new("real-assets");
    let extensions = Extensions::new(None, Some(one_by_one.0.clone()), state.clone());
    for component in &components {
        let dir = extensions
            .install_zip(component.id, &assets.join(&component.asset))
            .unwrap();
        for file in &component.files {
            assert_eq!(fs::metadata(dir.join(&file.name)).unwrap().len(), file.size);
            // 装好之后按清单找得到(ssh-tools 的 mt-ssh-cli / mt-ssh-mcp 走的就是这条)
            let located = extensions
                .locate(component.id, &file.name, None)
                .unwrap()
                .expect("装好之后 locate 应找得到");
            assert_eq!(located.path, dir.join(&file.name));
            assert_eq!(located.source, LocatedSource::Installed);
            assert_eq!(
                located.expected,
                Some((file.sha256.clone(), file.size)),
                "{}",
                file.name
            );
        }
        eprintln!("installed {} -> {}", component.asset, dir.display());
    }

    let from_bundle = TempRoot::new("real-bundle");
    let extensions = Extensions::new(None, Some(from_bundle.0.clone()), state);
    let report = extensions.import_offline(&bundle).unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.installed.len(), components.len());
    eprintln!(
        "offline bundle: installed {:?}, skipped {}",
        report.installed, report.skipped
    );
}
