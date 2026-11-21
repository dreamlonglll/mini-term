use super::*;

// ---------------------------------------------------------------------------
// git CLI(网络与 worktree)
// ---------------------------------------------------------------------------

/// 在 Windows GUI 应用下 spawn console 子进程(比如 git.exe)默认会弹出 conhost
/// 黑框,并且窗口创建/焦点切换会让 UI 感知卡顿。这里统一给 `Command` 加
/// CREATE_NO_WINDOW 抑制掉控制台分配。
pub(super) fn hide_console_window(_cmd: &mut std::process::Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        _cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

// ── 超时档位 ──────────────────────────────────────────────────
//
// 全部 git CLI 调用按「最慢会慢在哪」分四档,子模块一律取这里的名字,不再各写各的秒数。

/// 只动引用 / 只读的快操作(建删改名分支、删贮藏、删标签、冲突取一侧、blame、
/// worktree prune):本身毫秒级,30s 只是兜底。
pub(super) const QUICK_OP_TIMEOUT: Duration = Duration::from_secs(30);
/// 纯本地、但会同步跑 hook / gpg 签名的操作(提交、拣选、还原、改消息、附注标签、
/// 删 worktree):pre-commit hook 可能跑格式化 / lint / 测试,30s 对大仓库偏紧;真卡死的
/// 形态是 hook 死循环或 gpg 等口令(stdin 已置 null,不会永久挂起),60s 兜底。
pub(super) const LOCAL_OP_TIMEOUT: Duration = Duration::from_secs(60);
/// 改写工作区 / 暂存区的操作(检出、合并、变基、重置、贮藏、继续 / 中止、暂存类、
/// 建 worktree):大仓库一次 checkout 能到几十秒,还可能跑 post-checkout hook 与
/// clean 过滤器(git-lfs 给大文件算哈希),给到 120s。
pub(super) const WORKTREE_OP_TIMEOUT: Duration = Duration::from_secs(120);
/// 网络操作(fetch / pull / push / 删远程分支 / 推标签):120s,超时提示用 [`NETWORK_HINT`]。
pub(super) const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);
/// 网络操作超时时补在错误后面的提示。各处超时提示一律以全角「，」开头、不带句末标点
/// (句号由 [`timeout_message`] 统一补),整句全角标点、不与半角混排。
pub(super) const NETWORK_HINT: &str =
    "，可能在等待凭证或网络故障，请确认已配置凭证管理器或 SSH key";

/// git 命令超时的说明:「git push 超时（120s），可能在等待凭证…。」,删过残留的
/// `index.lock` 时另起一行写明。展示给用户看的整句,标点全用全角(E2E r4 A10:原先
/// 「超时(120s),可能…。请确认…SSH key」半角括号 / 逗号混排、句末无句号)。
pub(super) fn timeout_message(
    op_label: &str,
    secs: u64,
    hint: &str,
    removed_lock: Option<&Path>,
) -> String {
    let mut message = format!("git {op_label} 超时（{secs}s）{hint}。");
    if let Some(lock) = removed_lock {
        message.push_str(&format!(
            "\n已终止 git 及其子进程，并删除了它留下的 {}",
            lock.display()
        ));
    }
    message
}

/// 会拉起编辑器的命令(合并提交、`rebase` / `pull --rebase` 续跑、标签)一律带上:
/// 沿用 git 生成的提交信息,不等一个永远不会有人操作的编辑器。
pub(super) const NO_EDITOR: &[(&str, &str)] = &[("GIT_EDITOR", "true")];

/// git 失败时给用户看的原文:**stderr 优先,空了退 stdout**,两者都空才给一句兜底。
///
/// 不把两者拼在一起:git 的报错本来就写在 stderr,stdout 上多半是进度 / porcelain 这类
/// 与失败无关的输出,拼上只会把原因埋进噪声。但有几类命令把说明写在 stdout 上(`merge`
/// 的冲突说明、`commit` 的「没有可提交的内容」),stderr 为空时只取 stderr 等于什么都
/// 没告诉用户,所以退一步取 stdout。
pub(super) fn failure_text(out: &GitOutput) -> String {
    let stderr = out.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let stdout = out.stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_string();
    }
    "git 执行失败(没有输出)".to_string()
}

/// 跑一条 git 命令,非零退出即把原文([`failure_text`])装进错误。
pub(super) fn run_checked(
    repo_path: &Path,
    args: &[&str],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<GitOutput> {
    run_checked_env(repo_path, args, &[], op_label, timeout, timeout_hint)
}

/// 同 [`run_checked`],另给子进程补几条环境变量(见 [`run_git_output_env`])。
pub(super) fn run_checked_env(
    repo_path: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<GitOutput> {
    let out = run_git_output_env(repo_path, args, envs, op_label, timeout, timeout_hint)?;
    if !out.success {
        bail!("{}", failure_text(&out));
    }
    Ok(out)
}

/// 执行器的前置校验:`repo_path` 是目录,且下面有 `.git`(避免在任意目录上跑 git)。
/// worktree 目录下 `.git` 是文件而非目录,`exists()` 两者皆真。
pub(super) fn ensure_repo_dir(repo_path: &Path) -> Result<()> {
    if !repo_path.is_dir() {
        bail!("不是有效目录:{}", repo_path.display());
    }
    if !repo_path.join(".git").exists() {
        bail!("不是 git 仓库(缺少 .git):{}", repo_path.display());
    }
    Ok(())
}

/// 通用 git CLI 执行器(pull/push/worktree 系列共用):
/// - 校验 `repo_path` 是目录并且包含 `.git`([`ensure_repo_dir`])
/// - `GIT_TERMINAL_PROMPT=0`:没有终端可问,要账号密码时立刻失败,不干等到超时
///   (凭证管理器的 GUI 弹窗不受影响)
/// - 到达上限即 kill 子进程并返回超时错误;stdout/stderr 由独立线程读,
///   孙进程(ssh / 凭证助手)攥着管道不放也不会把调用线程拖住
///
/// **会阻塞当前线程最多 `timeout`**,调用方自行放到后台执行器上跑。
pub(super) fn run_git_command(
    repo_path: &Path,
    args: &[&str],
    timeout: Duration,
    timeout_hint: &str,
) -> Result<String> {
    let op = args.join(" ");
    run_git_command_labeled(repo_path, args, &op, timeout, timeout_hint)
}

/// 同 `run_git_command`,但错误信息里的操作名由 `op_label` 指定而非拼 `args`。
///
/// 给 `git commit -m <message>` 这类**参数里带用户文本**的调用用:直接 join 会把
/// 整条提交信息(可能几十行)灌进「启动失败 / 超时」的错误里。
pub(super) fn run_git_command_labeled(
    repo_path: &Path,
    args: &[&str],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<String> {
    run_checked(repo_path, args, op_label, timeout, timeout_hint).map(|out| out.stdout)
}

/// 同 `run_git_command_labeled`,但退出码非零也返回 `Ok`,交调用方按 stdout/stderr
/// 自己判(merge 冲突、`branch -d` 未合并这类要和「真失败」分开处理的场合)。
pub(super) fn run_git_output(
    repo_path: &Path,
    args: &[&str],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<GitOutput> {
    run_git_output_env(repo_path, args, &[], op_label, timeout, timeout_hint)
}

/// 同 [`run_git_output`],另给子进程补几条环境变量。
///
/// 给 `rebase --continue` / `cherry-pick --continue` 这类**会拉起编辑器**的续跑用:
/// 传 `GIT_EDITOR=true` 让它直接沿用现成的提交信息。不用 `-c core.editor=true` ——
/// 用户环境里设了 `GIT_EDITOR` 时它压过 `core.editor`,编辑器照样被拉起来。
pub(super) fn run_git_output_env(
    repo_path: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<GitOutput> {
    ensure_repo_dir(repo_path)?;
    spawn_git(repo_path, args, envs, op_label, timeout, timeout_hint)
}

/// 同 `run_git_command_labeled`,但把 `input` 整段喂给子进程的 stdin(写完即关管道)。
/// 给 `git blame --contents -` 这类要从 stdin 读内容的调用用;其余调用的 stdin
/// 仍是 null(见 `spawn_git`)。
pub(super) fn run_git_command_with_input(
    repo_path: &Path,
    args: &[&str],
    op_label: &str,
    input: Vec<u8>,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<String> {
    ensure_repo_dir(repo_path)?;
    let output = spawn_git_with_input(
        repo_path,
        args,
        &[],
        op_label,
        timeout,
        timeout_hint,
        Some(input),
    )?;
    if output.success {
        Ok(output.stdout)
    } else {
        bail!("{}", failure_text(&output));
    }
}

/// 一次 git CLI 调用的结果。
pub(super) struct GitOutput {
    pub(super) success: bool,
    /// 退出码;Unix 上被信号杀掉时为 `None`。[`GIT_USAGE_ERROR`] = 不认识的选项。
    pub(super) code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

/// git 的「用法错误」退出码(`usage_with_options` 一律 `exit(129)`):老版本不认某个
/// 新选项时就是它。判断「这个 git 不支持某选项」只看它,不看会被本地化的提示文本。
pub(super) const GIT_USAGE_ERROR: i32 = 129;

/// 管道读完后等孙进程松手的上限。git 本身已退出,剩下的只是 ssh / 凭证助手
/// 这类可能继承了管道句柄的后代 —— 等不到就按已读到的算。
pub(super) const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// 判断 index.lock 是不是「本次启动之后才出现」时对启动时刻的放宽量:文件时间戳取自
/// 粗粒度的系统时钟(Windows 上一拍约 15ms),可能比启动前读到的精确时刻还早一点。
pub(super) const LOCK_MTIME_SLACK: Duration = Duration::from_millis(100);

/// 边读边攒的管道输出。读线程把读到的每一块追加进共享缓冲,读到 EOF 再发信号;
/// 等不到 EOF(孙进程攥着管道不放)时取走已经攒下的那部分 —— 旧实现读完才整块交出,
/// 宽限期一到已读到的输出就整块丢了。
pub(super) struct PipeDrain {
    buf: std::sync::Arc<Mutex<Vec<u8>>>,
    done: std::sync::mpsc::Receiver<()>,
}

impl PipeDrain {
    fn start(pipe: Option<Box<dyn std::io::Read + Send>>) -> Self {
        use std::io::Read as _;
        let buf = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (tx, done) = std::sync::mpsc::channel();
        let sink = buf.clone();
        std::thread::spawn(move || {
            if let Some(mut pipe) = pipe {
                let mut chunk = [0u8; 8192];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => sink.lock().extend_from_slice(&chunk[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            }
            // 忽略发送失败:超时路径上接收端已被 drop
            let _ = tx.send(());
        });
        Self { buf, done }
    }

    /// 最多等到 `deadline` 读线程收尾,然后取走已读到的内容。
    fn collect(self, deadline: Instant) -> String {
        let _ = self
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
        let bytes = std::mem::take(&mut *self.buf.lock());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// 子进程连同它的全部后代。超时要整棵终止:Git for Windows 的 `git.exe` 只是个启动器,
/// 真正干活的 git、hook 的 sh、ssh / 凭证助手都是它的后代 —— 只杀启动器,它们照跑,
/// checkout / merge / `commit -a` 的 pre-commit 期间攥着的 index.lock 也一直不放。
///
/// - Windows:spawn 后把子进程放进一个 Job Object,之后它起的后代自动入 Job,超时
///   `TerminateJobObject` 一把收掉。**不**设 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`:那样
///   正常结束时一关句柄就连带杀掉合法地留在后台的后代(凭证缓存守护进程之类)。
///   spawn 与入 Job 之间有极短的窗口,git 在那之前还没起任何子进程。
/// - Unix:spawn 时 `process_group(0)` 让 git 自成一个进程组(pgid = pid),超时对整组
///   发 SIGKILL(本 crate 不依赖 libc,借系统的 `kill` 命令)。
pub(super) struct ProcessTree {
    #[cfg(windows)]
    job: Option<job_object::Job>,
}

impl ProcessTree {
    /// spawn 之前:Unix 上让子进程自成进程组。
    fn prepare(_cmd: &mut std::process::Command) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            _cmd.process_group(0);
        }
    }

    /// spawn 之后:Windows 上把子进程挂进 Job。挂不上(极少见)就只能退回只杀 git 本身。
    fn attach(_child: &std::process::Child) -> Self {
        Self {
            #[cfg(windows)]
            job: job_object::Job::attach(_child),
        }
    }

    /// 终止整棵进程树并等 git 本身退出。
    fn kill(&self, child: &mut std::process::Child) {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
        #[cfg(unix)]
        {
            use std::process::{Command, Stdio};
            // process_group(0) 之后 pgid 就是 git 的 pid;负号 = 发给整个进程组
            let group = format!("-{}", child.id());
            let _ = Command::new("kill")
                .args(["-KILL", "--", group.as_str()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        // 兜底(没挂进 Job / 系统里没有 kill 命令):至少把 git 自己杀掉
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(windows)]
pub(super) mod job_object {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
    };

    /// 一个匿名 Job Object 的句柄,Drop 时关闭(不连带终止里面的进程)。
    pub(super) struct Job(HANDLE);

    impl Job {
        pub(super) fn attach(child: &std::process::Child) -> Option<Job> {
            // SAFETY:两个参数都允许为空(默认安全属性、匿名);返回的句柄归 Job 所有
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle == 0 {
                return None;
            }
            let job = Job(handle);
            // SAFETY:child 存活期间它的进程句柄一直有效
            let ok = unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) };
            (ok != 0).then_some(job)
        }

        pub(super) fn terminate(&self) {
            // SAFETY:句柄在 Job 存活期间有效
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY:句柄只在这里关一次
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// 超时被杀的 git 留下的 `<gitdir>/index.lock`:只认**本次启动之后**才出现的那个(更早的
/// 属于别的 git 进程,不归这里管)。gitdir 走 git2 求 —— linked worktree 的 `.git` 是个
/// 文件,锁在 `<主仓库>/.git/worktrees/<名字>/` 下。返回删掉的路径。
pub(super) fn remove_fresh_index_lock(repo_path: &Path, started: SystemTime) -> Option<PathBuf> {
    let lock = Repository::open(repo_path).ok()?.path().join("index.lock");
    let since = started.checked_sub(LOCK_MTIME_SLACK).unwrap_or(started);
    if std::fs::metadata(&lock).ok()?.modified().ok()? < since {
        return None;
    }
    // Windows 上被终止的进程要过一会儿才真正放开文件句柄:删不掉就稍等再试
    for _ in 0..20 {
        match std::fs::remove_file(&lock) {
            Ok(()) => return Some(lock),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    None
}

/// 等子进程退出,最多等到 `deadline`:退出了是 `Ok(Some(状态))`,到点还没退是 `Ok(None)`
/// (由调用方去杀)。
///
/// 旧实现每 15ms `try_wait` 一次,几十毫秒就跑完的 git(绝大多数调用)平白多等 0~15ms。
/// Windows 上直接等进程句柄变为有信号(`WaitForSingleObject` 带超时),退出即醒、不轮询;
/// 其余平台没有 libc 依赖就没有带超时的 `waitpid`,退而求其次:从 1ms 起翻倍、封顶 15ms
/// 的退避轮询,快的命令几乎不多等,慢的命令也不至于空转。
pub(super) fn wait_child_until(
    child: &mut std::process::Child,
    deadline: Instant,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::Foundation::{HANDLE, WAIT_FAILED};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Some(status));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            // 不传 INFINITE(u32::MAX):超长的超时也分段等,到点由上面的判断收口
            let millis = u32::try_from(left.as_millis())
                .unwrap_or(u32::MAX - 1)
                .max(1);
            // SAFETY:child 存活期间它的进程句柄一直有效;只等、不关句柄
            let woke = unsafe { WaitForSingleObject(child.as_raw_handle() as HANDLE, millis) };
            if woke == WAIT_FAILED {
                // 等不了(极少见):退回旧的固定间隔轮询
                std::thread::sleep(Duration::from_millis(15));
            }
        }
    }
    #[cfg(not(windows))]
    {
        let mut nap = Duration::from_millis(1);
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Some(status));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            std::thread::sleep(nap.min(deadline - now));
            nap = (nap * 2).min(Duration::from_millis(15));
        }
    }
}

/// 起 git 子进程并限时等待;只有「起不来 / 超时」是 `Err`。stdin 是 null。
pub(super) fn spawn_git(
    repo_path: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
) -> Result<GitOutput> {
    spawn_git_with_input(repo_path, args, envs, op_label, timeout, timeout_hint, None)
}

/// [`spawn_git`] 的本体。`input` 为 `Some` 时 stdin 接管道,由独立线程整段写完后
/// 关闭(内容大于管道缓冲时,git 一边读一边已经开始往 stdout 写,同一线程先写后读会
/// 互相等死);`None` 时 stdin 是 null,与此前的行为一致。
pub(super) fn spawn_git_with_input(
    repo_path: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    op_label: &str,
    timeout: Duration,
    timeout_hint: &str,
    input: Option<Vec<u8>>,
) -> Result<GitOutput> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    // 超时后据此认出「本次留下的」index.lock(见 remove_fresh_index_lock)
    let started = SystemTime::now();
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(envs.iter().copied())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console_window(&mut cmd);
    ProcessTree::prepare(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("启动 git {} 失败:{}", op_label, e))?;
    let tree = ProcessTree::attach(&child);

    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        std::thread::spawn(move || {
            // 写失败(git 提前退出、管道已断)不在这里报:退出码与 stderr 会说明原因
            let _ = stdin.write_all(&bytes);
            // drop 即关管道,git 才读得到 EOF
        });
    }

    let stdout = PipeDrain::start(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = PipeDrain::start(child.stderr.take().map(|p| Box::new(p) as _));

    let deadline = Instant::now() + timeout;
    let status = match wait_child_until(&mut child, deadline) {
        Ok(Some(status)) => status,
        Ok(None) => {
            tree.kill(&mut child);
            let lock = remove_fresh_index_lock(repo_path, started);
            bail!(
                "{}",
                timeout_message(op_label, timeout.as_secs(), timeout_hint, lock.as_deref())
            );
        }
        Err(e) => bail!("git {} 等待失败:{}", op_label, e),
    };

    // 两根管道共用一个宽限期:git 已退出,只剩后代可能还攥着管道
    let grace = Instant::now() + PIPE_DRAIN_GRACE;
    Ok(GitOutput {
        success: status.success(),
        code: status.code(),
        stdout: stdout.collect(grace),
        stderr: stderr.collect(grace),
    })
}

pub(super) fn run_git_network_command(repo_path: &Path, op: &'static str) -> Result<String> {
    run_git_command(repo_path, &[op], NETWORK_TIMEOUT, NETWORK_HINT)
}

/// 无参 `git pull`(旧接口;面板走 `sync` 里的 `git_pull_with` / `git_pull_by_config`)。
/// **阻塞最多 [`NETWORK_TIMEOUT`]**,必须在后台线程上调用。
pub fn git_pull(repo_path: &Path) -> Result<String> {
    run_git_network_command(repo_path, "pull")
}

/// 无参 `git push`(旧接口;面板走 `sync` 里的 `git_push_with`)。
/// **阻塞最多 [`NETWORK_TIMEOUT`]**,必须在后台线程上调用。
pub fn git_push(repo_path: &Path) -> Result<String> {
    run_git_network_command(repo_path, "push")
}
