//! 「SSH 连接」表单的「测试连接」:用表单**当前**的值走一次与正式连接同一套的建连 + 认证
//! ([`mt_ssh::SshPool::probe`]),认证成功即断开,不执行任何命令。
//!
//! # 为什么不走 [`crate::probe_connection`] / [`crate::validate_dir`]
//!
//! 那两条经池的 `acquire`:会复用池里的会话(测到的就不是表单上这份配置)、会把新会话放进
//! 池、认证失败还会记下 30 秒冷却 —— 测一次失败,随后保存的正式连接就被白白挡 30 秒。
//! `probe` 与池新建会话共用建连 / 认证 / known_hosts / 超时预算的全部代码,只是不写池的
//! 状态(会话表与冷却记录),理由与边界见它的文档。
//!
//! # 线程与取消
//!
//! 任务跑在本 crate 自持的运行时上([`start_connection_test`] 立即返回),调用方在任何执行器
//! 上 await [`ConnectionTest::outcome`] 都行(gpui 前台任务里直接 await,与 `run_git_on`
//! 同一路数)。[`ConnectionTest`] drop 即 abort 那条任务:建连 future 随之丢弃,`mt-ssh` 的
//! 「drop 即掐断传输」保证 socket 与 russh 会话任务一并释放(与建连超时同一条路)。调用方
//! 关表单 = 取消,不必另做什么。

use mt_config::SshConnection;
use mt_ssh::{AcquireError, AcquireErrorKind, AuthFailureReport, ProbeSuccess};

use super::{ACQUIRE_TIMEOUT, state};

/// 失败的类别:界面按它挑一句标题,原文另行展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionTestFailureKind {
    /// 连接配置本身的问题(主机为空、没有用户名……)。
    Config,
    /// DNS、TCP 或 SSH 握手失败。
    Network,
    /// 在时限内没建好连接;原文写明卡在哪个阶段。
    Timeout,
    /// 主机密钥校验没过(与 known_hosts 不符 / 已吊销 / known_hosts 读写失败)。
    HostKey,
    /// 服务器没接受任何一种认证方式;逐方法明细见 [`ConnectionTestFailure::auth`]。
    Auth,
    /// 同一身份刚认证失败过,冷却期内没去连服务器。
    Cooldown,
    /// 本机这边的问题(SSH 运行时建不起来、测试任务异常结束),或将来新增的类别。
    Other,
}

/// 一次测试失败。`message` 是可直接展示的原文(英文类别开头,不含密码与私钥内容)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionTestFailure {
    pub kind: ConnectionTestFailureKind,
    pub message: String,
    /// 认证失败时拆开的逐方法明细;格式认不出(或不是认证失败)为 `None`,照原文展示。
    pub auth: Option<AuthFailureReport>,
}

impl ConnectionTestFailure {
    fn other(message: impl Into<String>) -> Self {
        Self {
            kind: ConnectionTestFailureKind::Other,
            message: message.into(),
            auth: None,
        }
    }

    /// 池的错误分类 → 本模块的类别;认证失败顺手拆出逐方法明细。
    fn from_acquire(error: AcquireError) -> Self {
        let kind = match error.kind {
            AcquireErrorKind::Config => ConnectionTestFailureKind::Config,
            AcquireErrorKind::Network => ConnectionTestFailureKind::Network,
            AcquireErrorKind::Timeout => ConnectionTestFailureKind::Timeout,
            AcquireErrorKind::HostKey => ConnectionTestFailureKind::HostKey,
            AcquireErrorKind::Auth => ConnectionTestFailureKind::Auth,
            AcquireErrorKind::Cooldown => ConnectionTestFailureKind::Cooldown,
            // `AcquireErrorKind` 是 non_exhaustive:将来新增的类别先按原文展示
            _ => ConnectionTestFailureKind::Other,
        };
        let auth = (kind == ConnectionTestFailureKind::Auth)
            .then(|| mt_ssh::parse_auth_failure(&error.message))
            .flatten();
        Self {
            kind,
            message: error.message,
            auth,
        }
    }
}

/// 一次进行中的测试。drop 即取消(abort 运行时上的那条任务,建了一半的连接随之掐断)。
pub struct ConnectionTest {
    task: tokio::task::JoinHandle<Result<ProbeSuccess, ConnectionTestFailure>>,
}

impl ConnectionTest {
    /// 等结果。
    pub async fn outcome(mut self) -> Result<ProbeSuccess, ConnectionTestFailure> {
        match (&mut self.task).await {
            Ok(result) => result,
            Err(error) => Err(ConnectionTestFailure::other(format!(
                "连接测试异常结束: {error}"
            ))),
        }
    }
}

impl Drop for ConnectionTest {
    fn drop(&mut self) {
        // 已经跑完的任务上 abort 是空操作
        self.task.abort();
    }
}

/// 开始测试一份连接配置(多半还没保存)。**不阻塞**,主线程直接调。
///
/// `conn.password` 与存进配置的口径相同:信封或遗留明文,在池认证的那一刻解开(`mt-ssh` 唯一
/// 的解封点);表单里新填的明文由调用方先封存再传进来,这里不碰明文。
///
/// 外层再套一道与正式连接相同的 [`ACQUIRE_TIMEOUT`](池自己的 `connect_timeout` 先到点,
/// 能报出卡在哪个阶段;这一道只兜底)。
pub fn start_connection_test(conn: SshConnection) -> Result<ConnectionTest, ConnectionTestFailure> {
    let st = state();
    let runtime = st.runtime().map_err(ConnectionTestFailure::other)?;
    let task = runtime.spawn(async move {
        // 池在运行时里懒建(构造要 spawn reaper);`probe` 只读池的冷却记录,不碰会话表
        let pool = st.pool();
        match tokio::time::timeout(ACQUIRE_TIMEOUT, pool.probe(&conn)).await {
            Ok(Ok(success)) => Ok(success),
            Ok(Err(error)) => Err(ConnectionTestFailure::from_acquire(error)),
            Err(_) => Err(ConnectionTestFailure {
                kind: ConnectionTestFailureKind::Timeout,
                message: format!("连接 {} 超时({}s)", conn.host, ACQUIRE_TIMEOUT.as_secs()),
                auth: None,
            }),
        }
    });
    Ok(ConnectionTest { task })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acquire_error(kind: AcquireErrorKind, message: &str) -> AcquireError {
        AcquireError {
            kind,
            message: message.to_string(),
        }
    }

    /// 认证失败拆出逐方法明细;其余类别原文照搬、不硬拆。
    #[test]
    fn 池的错误按类别映射且认证失败拆出逐方法明细() {
        let failure = ConnectionTestFailure::from_acquire(acquire_error(
            AcquireErrorKind::Auth,
            "authentication failed for u@h:22 (server allows: publickey,password): identity \
             file ~/.ssh/k (connection setting): file not found; password: rejected by server",
        ));
        assert_eq!(failure.kind, ConnectionTestFailureKind::Auth);
        let auth = failure.auth.expect("逐方法明细");
        assert_eq!(auth.server_methods, "publickey,password");
        assert_eq!(
            auth.attempts,
            vec![
                "identity file ~/.ssh/k (connection setting): file not found".to_string(),
                "password: rejected by server".to_string(),
            ]
        );

        let host_key = ConnectionTestFailure::from_acquire(acquire_error(
            AcquireErrorKind::HostKey,
            "ssh connect to h:22 failed: host key verification failed: ...",
        ));
        assert_eq!(host_key.kind, ConnectionTestFailureKind::HostKey);
        assert!(host_key.auth.is_none());
        assert!(host_key.message.starts_with("ssh connect to h:22 failed"));

        // 冷却期的报错里带着上次的认证失败,但这次根本没去连:不拆,按冷却展示
        let cooldown = ConnectionTestFailure::from_acquire(acquire_error(
            AcquireErrorKind::Cooldown,
            "authentication cooldown: ... Last error: authentication failed for u@h:22 (server \
             allows: password): password: rejected by server",
        ));
        assert_eq!(cooldown.kind, ConnectionTestFailureKind::Cooldown);
        assert!(cooldown.auth.is_none());

        // 认证失败但格式认不出:类别照旧,明细为空,界面显示原文
        let odd = ConnectionTestFailure::from_acquire(acquire_error(
            AcquireErrorKind::Auth,
            "something else",
        ));
        assert_eq!(odd.kind, ConnectionTestFailureKind::Auth);
        assert!(odd.auth.is_none());
    }
}
