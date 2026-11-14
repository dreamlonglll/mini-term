use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("mt-git-helper: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    anyhow::ensure!(
        args.next().as_deref() == Some(std::ffi::OsStr::new("--root")),
        "usage: mt-git-helper --root <absolute-directory>"
    );
    let root = PathBuf::from(
        args.next()
            .ok_or_else(|| anyhow::anyhow!("missing --root directory"))?,
    );
    anyhow::ensure!(
        args.next().is_none(),
        "usage: mt-git-helper --root <absolute-directory>"
    );
    // The desktop owns the hint; never infer a different encoding from the remote locale.
    if let Ok(region) = std::env::var("MINI_TERM_ENCODING_REGION") {
        mt_project::encoding::set_region_hint(&region);
    }
    mt_git_helper::serve(&root, std::io::stdin().lock(), std::io::stdout().lock())
}
