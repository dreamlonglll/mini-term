use super::*;

/// 仓库发现允许越过**项目根**往上爬的层数(项目是 monorepo 子目录时仓库在项目根之上)。
pub(super) const MAX_DISCOVER_PARENTS: usize = 5;

/// 为 `project_path` 下的文件 `abs_file` 找它所属的仓库。
///
/// 起点是文件所在目录、向上搜;搜索上界锚在 **项目根再往上 [`MAX_DISCOVER_PARENTS`] 级**。
/// 两个坑都踩过,别改回去:
///
/// 1. **上界不能按文件算。** 旧实现拿文件往上数 5 级当 ceiling,文件嵌得越深、ceiling 就
///    越可能落在仓库根**以内**——`src/pages/task/my/my.vue` 往上 5 级正好是仓库根,diff
///    直接报「找不到仓库」(issue #58);凡是仓库根以下 ≥4 层目录的文件全中招。锚在项目根
///    上,能不能找到就与文件深度无关了。
/// 2. **libgit2 的 ceiling 是排他的**:走到 ceiling 目录本身即停、不检查它(`repository.c`
///    的 `path.ptr[ceiling_offset] == 0` 即 break)。要让「往上 N 级」都被查到,ceiling 得取
///    第 N+1 级。
///
/// 起点从文件所在目录出发而不是直接开项目根,是为了让嵌套子仓库里的文件找到离它最近的
/// 那个仓库(文件树的「查看变更」标签就是按最近仓库算的)。目录已随文件一起被删掉时回退
/// 到最近一个还存在的祖先——libgit2 会对起点做 realpath,不存在直接报错。
pub(super) fn discover_repo_for(project_path: &Path, abs_file: &Path) -> Option<Repository> {
    let start = abs_file
        .ancestors()
        .skip(1)
        .find(|p| p.is_dir())
        .unwrap_or(project_path);
    open_repo_within(start, project_path)
}

/// `project_path` 下的文件 `abs_file` 在仓库工作区 `workdir` 里的相对路径(`/` 分隔);
/// 不在这个工作区里(或就是工作区根本身)为 `None`。
///
/// 先直接算;算出 `..` 开头时再把项目根 `canonicalize` 一遍重算:libgit2 发现仓库时对
/// 起点做过 realpath,Unix 上经软链进入的项目(macOS 的 `/var` → `/private/var` 也是)
/// workdir 是解析后的真实路径、项目路径不是,直接比出来是 `../<链接名>/…` —— diff 误显示
/// 为整文件新增、按块暂存找不到条目。只规范化**项目根**(它一定存在),文件那一段原样
/// 拼回去:文件本身可能已被删除,也不该顺着文件自己的软链跳走。
///
/// Windows 上 std 的 `canonicalize` 给的是 `\\?\` 形式,[`mt_core::path::canonicalize`]
/// 已剥回 `C:\…` / `\\server\share\…`(`\\?\UNC\wsl$\…` → `\\wsl$\…`)。第一步就算对时
/// (绝大多数情况,libgit2 在 Windows 上不解析联接点)根本走不到这里。
pub(super) fn repo_relative_path(
    workdir: &Path,
    project_path: &Path,
    abs_file: &Path,
) -> Option<String> {
    let inside = |rel: PathBuf| -> Option<String> {
        let escapes = rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        });
        let text = slash_path(&rel);
        (!escapes && !text.is_empty()).then_some(text)
    };
    if let Some(rel) = diff_paths(abs_file, workdir).and_then(inside) {
        return Some(rel);
    }
    let in_project = abs_file.strip_prefix(project_path).ok()?;
    let real_project = canonicalize(project_path).ok()?;
    diff_paths(real_project.join(in_project), workdir).and_then(inside)
}

/// 仓库里的路径相对项目根的展示形式要用的「项目根」:与 workdir 有前缀关系就用原样;
/// 对不上(项目经软链进入、workdir 是解析过的真实路径)时换成规范化后的项目根。
pub(super) fn status_prefix(workdir: &Path, project_path: &Path) -> PathBuf {
    let related = |p: &Path| workdir.starts_with(p) || p.starts_with(workdir);
    if related(project_path) {
        return project_path.to_path_buf();
    }
    canonicalize(project_path)
        .ok()
        .filter(|real| related(real))
        .unwrap_or_else(|| project_path.to_path_buf())
}

/// 项目根所属的仓库:从项目根本身向上找,上界同样是项目根再往上 [`MAX_DISCOVER_PARENTS`] 级。
pub(super) fn discover_repo_limited(project_path: &Path) -> Option<Repository> {
    open_repo_within(project_path, project_path)
}

/// 项目根所属仓库的工作区根(形态与 [`discover_git_repos`] 给的 `path` 一致);项目不在任何
/// 仓库里(父目录型项目)为 `None`。只往上找、不扫子目录,开销是几次 stat —— 收着的 Git
/// 面板替文件树挂工作区监听时用,不为它去扫盘。
pub fn enclosing_repo(project_path: &Path) -> Option<PathBuf> {
    discover_repo_limited(project_path)?
        .workdir()
        .map(Path::to_path_buf)
}

/// 从 `start` 向上找仓库,最多找到 `anchor` 往上 [`MAX_DISCOVER_PARENTS`] 级为止
/// (libgit2 的 ceiling 排他,故取第 N+1 级)。
pub(super) fn open_repo_within(start: &Path, anchor: &Path) -> Option<Repository> {
    let ceiling = nth_parent(anchor, MAX_DISCOVER_PARENTS + 1);
    Repository::open_ext(start, RepositoryOpenFlags::empty(), [&ceiling]).ok()
}

/// `path` 往上 `n` 级的祖先;不够 `n` 级就停在根(或相对路径的顶层)。
pub(super) fn nth_parent(path: &Path, n: usize) -> PathBuf {
    let mut cur = path;
    for _ in 0..n {
        match cur.parent() {
            Some(p) if !p.as_os_str().is_empty() => cur = p,
            _ => break,
        }
    }
    cur.to_path_buf()
}

#[derive(Clone)]
pub(super) struct RepoPathEntry {
    pub(super) name: String,
    pub(super) path: PathBuf,
    pub(super) is_worktree: bool,
}

type RepoPathCacheMap = HashMap<PathBuf, (Instant, Vec<RepoPathEntry>)>;

pub(super) static REPO_PATH_CACHE: std::sync::LazyLock<Mutex<RepoPathCacheMap>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) const REPO_CACHE_TTL: Duration = Duration::from_secs(30);

/// worktree 增删/清理之后调用:仓库集合已变,让所有项目的发现缓存立即失效,
/// 否则 History/Changes 面板要等 TTL 过期才能看到新条目。
///
/// 缓存里只有路径(名字 / 路径 / 是不是 worktree),当前分支在 [`discover_git_repos`]
/// 里每次现读 —— 检出 / 新建 / 改名 / 删除分支不改变缓存的内容,不必调它(调了只会让
/// 下一次发现白白重扫一遍目录)。
pub fn invalidate_repo_cache() {
    REPO_PATH_CACHE.lock().clear();
}

pub(super) fn find_repos_cached_paths(project_path: &Path) -> Vec<RepoPathEntry> {
    let key = project_path.to_path_buf();
    {
        let cache = REPO_PATH_CACHE.lock();
        if let Some((ts, entries)) = cache.get(&key)
            && ts.elapsed() < REPO_CACHE_TTL
        {
            return entries.clone();
        }
    }
    let entries = discover_repo_paths(project_path);
    REPO_PATH_CACHE
        .lock()
        .insert(key, (Instant::now(), entries.clone()));
    entries
}

pub(super) fn discover_repo_paths(project_path: &Path) -> Vec<RepoPathEntry> {
    let mut entries = Vec::new();

    if let Some(repo) = discover_repo_limited(project_path)
        && let Some(workdir) = repo.workdir()
    {
        let repo_root = workdir.to_path_buf();
        let name = repo_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "root".to_string());
        // 每个项目只展示自己工作区的仓库,不再把关联 worktree 注入为独立条目——
        // worktree 通过「设为项目」拥有自己的 Git 面板,这里再列一遍就是重复。
        entries.push(RepoPathEntry {
            name,
            path: repo_root,
            is_worktree: repo.is_worktree(),
        });
        return entries;
    }

    const MAX_DEPTH: u32 = 5;
    // 依赖 / 构建产物 / 工具缓存目录:动辄成千上万个子目录,里面不会有用户自己的仓库。
    // 只收「明确是依赖或缓存」的名字(Python 虚拟环境 / tox、Gradle、Nuxt 的产物目录、
    // mypy / pytest 的缓存、bower 依赖);`build` / `vendor` 这类可能放着真仓库的不收。
    const SKIP_DIRS: &[&str] = &[
        ".git",
        "node_modules",
        "target",
        ".next",
        "dist",
        "__pycache__",
        ".superpowers",
        ".venv",
        "venv",
        ".tox",
        ".gradle",
        ".nuxt",
        ".mypy_cache",
        ".pytest_cache",
        "bower_components",
    ];
    fn scan(dir: &Path, depth: u32, entries: &mut Vec<RepoPathEntry>) {
        if depth > MAX_DEPTH {
            return;
        }
        let dir_entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in dir_entries.flatten() {
            let sub = entry.path();
            // 枚举目录时顺带拿到的类型,不再每项多 stat 一次;符号链接 / 联接点才去
            // 看它指向的是不是目录(与旧实现一样跟进去)
            let is_dir = match entry.file_type() {
                Ok(t) if t.is_symlink() => sub.is_dir(),
                Ok(t) => t.is_dir(),
                Err(_) => sub.is_dir(),
            };
            if !is_dir {
                continue;
            }
            let dir_name = entry.file_name();
            let dir_name_str = dir_name.to_string_lossy();
            if SKIP_DIRS.contains(&dir_name_str.as_ref()) {
                continue;
            }
            // 先看有没有 `.git`(目录或 worktree 的 gitfile)再开库:libgit2 开一个不是仓库的
            // 目录要试一圈布局才失败(约 430µs),5000 个普通目录就是 2s;stat 一次只要几十 µs。
            // 没有 `.git` 的目录 `Repository::open` 本来也只会失败或开出裸仓库(没有工作区,
            // 下面照样不收),跳过开库不改变结果
            if sub.join(".git").exists()
                && let Ok(repo) = Repository::open(&sub)
                && let Some(workdir) = repo.workdir()
                && canonicalize(workdir).ok() == canonicalize(&sub).ok()
            {
                let name = sub
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                // 物理上在项目目录内的 worktree 才会走到这里(作为子目录仓库),
                // 项目目录外的关联 worktree 不再注入
                entries.push(RepoPathEntry {
                    name,
                    path: sub,
                    is_worktree: repo.is_worktree(),
                });
                continue;
            }
            scan(&sub, depth + 1, entries);
        }
    }
    scan(project_path, 1, &mut entries);
    // `read_dir` 给的是文件系统顺序 —— Windows 的 NTFS 恰好按名字,但 ext4 是
    // 哈希序,同一个目录两次枚举都可能不一样,仓库下拉里的条目会莫名换位置。
    // 按路径排一遍:顺序确定,且同名仓库(monorepo 里的一堆 `api`)按父目录
    // 挨在一起,配上 UI 那行父级路径正好一眼对比。
    entries.sort_by_key(|e| e.path.to_string_lossy().to_lowercase());
    entries
}

/// 扫描 project_path 下的 git 仓库。
/// 只收集项目自身 / 子目录下物理可见的仓库;项目目录外的关联 worktree 不注入——
/// 它们经「设为项目」成为独立项目后,有自己的 History / Changes 面板。
pub(super) fn find_repos(project_path: &Path) -> Vec<(String, PathBuf, Repository, bool)> {
    let cached_paths = find_repos_cached_paths(project_path);
    let mut repos = Vec::new();
    for entry in cached_paths {
        if let Ok(repo) = Repository::open(&entry.path) {
            repos.push((entry.name, entry.path, repo, entry.is_worktree));
        }
    }
    repos
}
pub fn discover_git_repos(project_path: &Path) -> Result<Vec<GitRepoInfo>> {
    let repos = find_repos(project_path);
    Ok(repos
        .into_iter()
        .map(|(name, abs_path, repo, is_worktree)| {
            let current_branch = repo.head().ok().and_then(|h| {
                if h.is_branch() {
                    h.shorthand().map(|s| s.to_string())
                } else {
                    // detached HEAD — 显示短 hash
                    h.target().map(|oid| {
                        let s = oid.to_string();
                        format!("({})", &s[..7.min(s.len())])
                    })
                }
            });
            GitRepoInfo {
                name,
                path: abs_path,
                current_branch,
                is_worktree,
            }
        })
        .collect())
}
