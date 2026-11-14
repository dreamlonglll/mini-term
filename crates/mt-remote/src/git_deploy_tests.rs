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

#[test]
fn bundled_assets_take_precedence_over_explicit_development_root() {
    let fixture = Assets::new();
    let bundled = fixture.asset("remote-git-helpers", "linux-x86_64", b"bundled");
    fixture.asset("development", "linux-x86_64", b"development");
    let executable = fixture.0.join("mini-term");
    let selected = select_asset(
        &executable,
        Some(&fixture.0.join("development")),
        "linux-x86_64",
    )
    .unwrap();
    assert_eq!(selected, bundled);
}

#[test]
fn explicit_resource_root_is_used_only_if_bundle_is_absent() {
    let fixture = Assets::new();
    let development = fixture.asset("development", "linux-aarch64", b"configured");
    let executable = fixture.0.join("mini-term");
    assert_eq!(
        select_asset(
            &executable,
            Some(&fixture.0.join("development")),
            "linux-aarch64"
        )
        .unwrap(),
        development
    );
    let error = select_asset(&executable, None, "linux-aarch64").unwrap_err();
    assert!(error.contains("linux-aarch64"));
    assert!(error.contains("unavailable"));
}

#[test]
fn missing_architecture_never_uses_another_binary() {
    let fixture = Assets::new();
    fixture.asset("remote-git-helpers", "macos-aarch64", b"host-only");
    assert!(select_asset(&fixture.0.join("mini-term"), None, "linux-x86_64").is_err());
}

#[test]
fn invalid_bundled_asset_never_silently_uses_development_asset() {
    let fixture = Assets::new();
    fixture.asset("remote-git-helpers", "linux-x86_64", b"");
    fixture.asset("development", "linux-x86_64", b"valid");
    assert!(
        select_asset(
            &fixture.0.join("mini-term"),
            Some(&fixture.0.join("development")),
            "linux-x86_64"
        )
        .is_err()
    );
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
    assert!(select_asset(&fixture.0.join("mini-term"), None, "linux-x86_64").is_err());
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
        extra: Default::default(),
    };
    let error = ensure_helper(&connection, &|| Err("remote_git_disabled".into())).unwrap_err();
    assert_eq!(error, "remote_git_disabled");
}
