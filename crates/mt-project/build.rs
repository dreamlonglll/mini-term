fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // libgit2-sys 0.17 的 fs_path.c 调用了 GetNamedSecurityInfoW（advapi32.lib），
    // 但其 build.rs 漏掉了 `cargo:rustc-link-lib=advapi32`。此前因 windows-sys 0.52
    // 顺带链接了全量 windows.0.52.0.lib 才被掩盖；升至 notify 8 + windows-sys 0.61
    // （改走 raw-dylib）后需在此为 Windows 目标显式补齐。
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        println!("cargo:rustc-link-lib=advapi32");
    }
}
