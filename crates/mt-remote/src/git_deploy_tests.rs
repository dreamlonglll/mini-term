use super::*;

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Assets(PathBuf);

impl Assets {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mt-git-assets-test-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn asset(&self, root: &str, target: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(root).join(target).join("mt-git-helper");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for Assets {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn platform_probe_selects_only_supported_exact_targets() {
    for (probe, target) in [
        ("Linux\nx86_64\n", "linux-x86_64"),
        ("Linux\naarch64\n", "linux-aarch64"),
        ("Darwin\narm64\n", "macos-aarch64"),
        ("Darwin\nx86_64\n", "macos-x86_64"),
    ] {
        assert_eq!(parse_target(probe).unwrap(), target);
    }
    for unsupported in [
        "Linux\nriscv64\n",
        "FreeBSD\nx86_64\n",
        "Linux\n",
        "Linux\nx86_64\nnoise\n",
    ] {
        assert!(parse_target(unsupported).is_err(), "{unsupported:?}");
    }
}

/// exe 在 `fixture.0` 下(旧布局 `remote-git-helpers/` 与它同级),扩展根目录在 `extensions/`。
fn extensions(fixture: &Assets, manifest: mt_extensions::ManifestState) -> Extensions {
    Extensions::new(
        Some(fixture.0.clone()),
        Some(fixture.0.join("extensions")),
        manifest,
    )
}

fn no_manifest(fixture: &Assets) -> Extensions {
    extensions(fixture, mt_extensions::ManifestState::Missing)
}

fn selected(path: PathBuf) -> SelectedAsset {
    SelectedAsset {
        path,
        expected_sha256: None,
    }
}

#[test]
fn bundled_assets_take_precedence_over_explicit_development_root() {
    let fixture = Assets::new();
    let bundled = fixture.asset("remote-git-helpers", "linux-x86_64", b"bundled");
    fixture.asset("development", "linux-x86_64", b"development");
    let selection = select_asset(
        &no_manifest(&fixture),
        Some(&fixture.0.join("development")),
        "linux-x86_64",
    )
    .unwrap();
    assert_eq!(selection, selected(bundled));
}

#[test]
fn explicit_resource_root_is_used_only_if_bundle_is_absent() {
    let fixture = Assets::new();
    let development = fixture.asset("development", "linux-aarch64", b"configured");
    assert_eq!(
        select_asset(
            &no_manifest(&fixture),
            Some(&fixture.0.join("development")),
            "linux-aarch64"
        )
        .unwrap(),
        selected(development)
    );
    // 三层都没有:类型化的「缺 helper」,带服务器平台键 —— 不是一句要匹配的人话
    assert_eq!(
        select_asset(&no_manifest(&fixture), None, "linux-aarch64").unwrap_err(),
        DeployError::HelperMissing {
            target: "linux-aarch64"
        }
    );
}

#[test]
fn missing_architecture_never_uses_another_binary() {
    let fixture = Assets::new();
    fixture.asset("remote-git-helpers", "macos-aarch64", b"host-only");
    assert_eq!(
        select_asset(&no_manifest(&fixture), None, "linux-x86_64").unwrap_err(),
        DeployError::HelperMissing {
            target: "linux-x86_64"
        }
    );
}

#[test]
fn invalid_bundled_asset_never_silently_uses_development_asset() {
    let fixture = Assets::new();
    fixture.asset("remote-git-helpers", "linux-x86_64", b"");
    fixture.asset("development", "linux-x86_64", b"valid");
    assert!(matches!(
        select_asset(
            &no_manifest(&fixture),
            Some(&fixture.0.join("development")),
            "linux-x86_64"
        ),
        Err(DeployError::Failed(_))
    ));
}

#[cfg(unix)]
#[test]
fn helper_asset_symlinks_are_rejected() {
    let fixture = Assets::new();
    let real = fixture.asset("development", "linux-x86_64", b"valid");
    let bundled = fixture
        .0
        .join("remote-git-helpers/linux-x86_64/mt-git-helper");
    std::fs::create_dir_all(bundled.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, &bundled).unwrap();
    assert!(matches!(
        select_asset(&no_manifest(&fixture), None, "linux-x86_64"),
        Err(DeployError::Failed(_))
    ));
}

/// 按需下载装好的扩展是第三层,带着清单里的期望哈希(部署时核对)。
#[test]
fn installed_extension_is_the_last_layer_and_carries_the_manifest_digest() {
    use sha2::Digest as _;
    let fixture = Assets::new();
    let bytes = b"\x7fELF installed helper";
    let digest: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let zip_sha = "ab".repeat(32);
    let manifest = mt_extensions::parse_manifest(&format!(
        r#"{{"schema":1,"appVersion":"1.15.0","tag":"v1.15.0","platform":"linux-x64","components":[
            {{"id":"git-helper-linux-x86_64","asset":"mt-ext_git-helper-linux-x86_64_{}.zip",
              "sha256":"{zip_sha}","size":100,
              "files":[{{"name":"mt-git-helper","sha256":"{digest}","size":{},"executable":true}}]}}]}}"#,
        &zip_sha[..12],
        bytes.len()
    ))
    .unwrap();
    let installed = fixture.asset(
        &format!("extensions/git-helper-linux-x86_64/{}", &zip_sha[..16]),
        "",
        bytes,
    );
    let extensions = extensions(&fixture, mt_extensions::ManifestState::Loaded(manifest));
    assert_eq!(
        select_asset(&extensions, None, "linux-x86_64").unwrap(),
        SelectedAsset {
            path: installed.clone(),
            expected_sha256: Some(digest.clone()),
        }
    );
    // 有效清单在场(正式安装包)时 exe 旁旧布局一律不认:tar.gz 覆盖解压留下的旧 helper 压不住
    // 下载的那份(mt-extensions 的 locate 口径,HANDOFF 1.2 修订)
    let bundled = fixture.asset("remote-git-helpers", "linux-x86_64", b"bundled");
    assert_eq!(
        select_asset(&extensions, None, "linux-x86_64").unwrap(),
        SelectedAsset {
            path: installed,
            expected_sha256: Some(digest),
        }
    );
    // 没有清单(dev / 源码构建)时同一份旧布局照用
    assert_eq!(
        select_asset(&no_manifest(&fixture), None, "linux-x86_64").unwrap(),
        selected(bundled)
    );
}

#[test]
fn missing_helper_failures_round_trip_without_parsing_prose() {
    let failure = missing_helper_failure("macos-aarch64", "download failed: timeout");
    assert_eq!(
        missing_helper(&failure),
        Some(MissingHelper {
            target: "macos-aarch64",
            detail: "download failed: timeout",
        })
    );
    // 没有原因(没注入下载器)也认
    let bare = missing_helper_failure("linux-x86_64", "");
    assert_eq!(
        missing_helper(&bare).map(|m| m.target),
        Some("linux-x86_64")
    );
    // 别的 code、不认识的平台、别的失败类型一律不是
    for other in [
        GitFailure::service("remote_git_deployment_failed", "linux-x86_64\nx"),
        GitFailure::service(REMOTE_GIT_HELPER_MISSING, "windows-x64\nx"),
        GitFailure::from("remote_git_helper_missing"),
    ] {
        assert_eq!(missing_helper(&other), None, "{other}");
    }
}

#[test]
fn cache_directory_commands_quote_all_remote_path_bytes() {
    let path = "/home/a b'$(touch /tmp/unsafe)/.cache";
    let command = secure_directory_command(path, true);
    let quoted = quote_posix(path);
    assert!(command.contains(&format!("test -L {quoted}")));
    assert!(command.contains(&format!("mkdir {quoted}")));
    assert!(command.contains(&format!("chmod 700 {quoted}")));
    assert!(command.contains(&format!("test -O {quoted}")));
    assert!(command.contains("umask 077"));
}

#[test]
fn existing_cache_ancestors_are_not_chmodded() {
    let command = secure_directory_command("/home/u/.cache", false);
    assert!(!command.contains("chmod"));
    assert!(command.contains("umask 077"));
    assert!(command.contains("test -O"));
    assert!(command.contains("test ! -L"));
}

#[test]
fn partial_paths_are_unique_siblings_never_final_executables() {
    let a = partial_path("/home/u/.cache/mini-term/git-helper/hash");
    let b = partial_path("/home/u/.cache/mini-term/git-helper/hash");
    assert_ne!(a, b);
    assert!(a.ends_with(".partial"));
    assert!(a.starts_with("/home/u/.cache/mini-term/git-helper/hash/.mt-git-helper-"));
}

#[test]
fn revoked_permission_stops_deployment_before_runtime_assets_or_network() {
    let connection = SshConnection {
        id: "denied-deployment".into(),
        name: "test".into(),
        host: "example.invalid".into(),
        port: 22,
        user: "tester".into(),
        password: None,
        identity_file: None,
        group: None,
        use_system_ssh: false,
        extra: Default::default(),
    };
    let error = ensure_helper(&connection, &|| Err("remote_git_disabled".into())).unwrap_err();
    assert_eq!(error, DeployError::Failed("remote_git_disabled".into()));
}
