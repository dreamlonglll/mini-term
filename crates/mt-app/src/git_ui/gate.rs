//! 按仓库的进程内**写操作闸**。
//!
//! 面板仓库栏(Fetch / 拉取 / 分支动作)、更改区(暂存 / 丢弃 / 提交)、历史区(检出 / 拣选 /
//! 重置…)、贮藏 / 推送 / 分支名称弹窗、diff 弹窗的按块操作各有各的「进行中」标记,彼此
//! 看不见:拉取还在跑时照样能点提交、拣选,两条 git 命令一起抢 `index.lock`,后一条报
//! 「另一个 git 进程正在运行」,或者把前一条截在半路。这里给它们一道共用的闸:**同一个
//! 仓库同一时刻只放一个写操作进去**。
//!
//! - 键是仓库路径的比较键([`gate_key`]:分隔符统一、去尾部分隔符,Windows 上不分大小写),
//!   同一个仓库的不同写法认作同一个;不同仓库互不相干;
//! - [`try_acquire`] 占闸,拿到的 [`WriteGuard`] 一 drop 就放闸 —— 成功、失败、超时、
//!   提前返回、panic 展开、任务被取消(持有它的 future 被丢弃)都一样;
//! - 只读刷新(取列表、取 diff、算签名)不进闸;
//! - 闸只活在 UI 线程上(`thread_local`,与 `git_panel` 的 `SECTION_UI` 同一形态),守卫
//!   `!Send`:编译期挡住把它带进后台执行器(在那边 drop 会去放另一个线程的闸)。各区按
//!   [`is_busy`] 画禁用态,不另行订阅 —— 占 / 放的地方负责请求整窗重画(见
//!   [`super::spawn_gated`])。

use std::cell::RefCell;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::rc::Rc;

thread_local! {
    static HELD: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// 仓库路径 → 闸的键。`D:\Git\x`、`D:/Git/x/`、(Windows 上)`d:\git\X` 是同一个仓库。
fn gate_key(repo: &str) -> String {
    if cfg!(windows) {
        mt_core::path_key::windows_eq_key(repo)
    } else {
        mt_core::path_key::slash_form(repo)
    }
}

/// 占着闸的凭证。drop 即放闸。
#[must_use = "守卫一 drop 闸就放开了:要一直攥到写操作结束"]
pub struct WriteGuard {
    key: String,
    /// `Rc` 让守卫 `!Send`(见模块注释)。
    _ui_thread: PhantomData<Rc<()>>,
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        // 线程退出的析构阶段 thread_local 可能已经没了:那时也没有闸可放
        let _ = HELD.try_with(|held| held.borrow_mut().remove(&self.key));
    }
}

/// 占 `repo` 的闸。已被占(本进程里另一项写操作正在跑这个仓库)时返回 `None`,
/// 调用方不执行、就地提示。空路径不占。
pub fn try_acquire(repo: &str) -> Option<WriteGuard> {
    if repo.is_empty() {
        return None;
    }
    let key = gate_key(repo);
    let inserted = HELD.with(|held| held.borrow_mut().insert(key.clone()));
    inserted.then_some(WriteGuard {
        key,
        _ui_thread: PhantomData,
    })
}

/// `repo` 的闸现在是不是被占着(按钮画禁用态用)。
pub fn is_busy(repo: &str) -> bool {
    !repo.is_empty() && HELD.with(|held| held.borrow().contains(&gate_key(repo)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 占 → 同仓库再占被拒 → 放 → 又能占;不同仓库互不相干。
    #[test]
    fn 占闸_重入拒绝_放闸() {
        let repo = "/tmp/gate-a";
        assert!(!is_busy(repo));
        let guard = try_acquire(repo).expect("空闲时占得到");
        assert!(is_busy(repo));
        assert!(try_acquire(repo).is_none(), "同一仓库不许第二个写操作进去");
        let other = try_acquire("/tmp/gate-b").expect("别的仓库不受影响");
        drop(guard);
        assert!(!is_busy(repo));
        assert!(is_busy("/tmp/gate-b"));
        let again = try_acquire(repo).expect("放闸后又能占");
        drop((again, other));
        assert!(!is_busy("/tmp/gate-b"));
    }

    /// 同一个仓库的不同写法是同一道闸。
    #[test]
    fn 路径写法不同也是同一道闸() {
        let _guard = try_acquire("/tmp/gate-c/").unwrap();
        assert!(is_busy("/tmp/gate-c"));
        assert!(try_acquire("/tmp/gate-c").is_none());
        if cfg!(windows) {
            let _win = try_acquire(r"D:\Git\Gate").unwrap();
            assert!(is_busy("d:/git/gate/"));
        }
        assert!(try_acquire("").is_none() && !is_busy(""), "空路径不占");
    }

    /// 提前返回、panic 展开都会放闸(守卫随作用域 / 栈展开 drop)。
    #[test]
    fn 提前返回与_panic_也放闸() {
        let repo = "/tmp/gate-d";
        fn early_return(repo: &str, bail: bool) -> Option<()> {
            let _guard = try_acquire(repo)?;
            if bail {
                return None;
            }
            Some(())
        }
        assert!(early_return(repo, true).is_none());
        assert!(!is_busy(repo));
        assert!(early_return(repo, false).is_some());
        assert!(!is_busy(repo));

        let result = std::panic::catch_unwind(|| {
            let _guard = try_acquire(repo).unwrap();
            assert!(is_busy(repo));
            panic!("写操作半路炸了");
        });
        assert!(result.is_err());
        assert!(!is_busy(repo), "panic 展开时守卫照样 drop");
    }
}
