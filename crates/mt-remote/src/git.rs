//! Minimal, persistent remote Git transport. All public calls block: use a background thread.

use std::sync::Arc;
use std::time::Duration;

use mt_config::SshConnection;
use mt_git_helper::{
    Hello, MAX_FRAME_BYTES, Operation, PROTOCOL_VERSION, Reply, Request, Response,
};
use mt_ssh::russh::{Channel, ChannelMsg, client};
use mt_ssh::{CachedSession, SshPool};
use serde::de::DeserializeOwned;

use super::{acquire_session, state};

const START_TIMEOUT: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const STDERR_CAP: usize = 16 * 1024;

#[derive(Clone, Copy)]
struct GitTimeouts {
    start: Duration,
    request: Duration,
    close: Duration,
}

impl GitTimeouts {
    const DEFAULT: Self = Self {
        start: START_TIMEOUT,
        request: REQUEST_TIMEOUT,
        close: CLOSE_TIMEOUT,
    };
}

/// One authenticated SSH channel running one helper. Requests are serialized by `&mut self`.
///
/// No reconnect or replay is performed, including after a failed write. Dropping the client
/// schedules bounded cleanup on the existing remote SSH runtime without blocking the caller.
pub struct RemoteGitClient {
    runtime: Arc<tokio::runtime::Runtime>,
    transport: Option<GitChannel>,
    next_id: u64,
    pending_write: Option<u64>,
}

struct GitChannel {
    session: Arc<CachedSession>,
    pool: Arc<SshPool>,
    connection_id: String,
    channel: Channel<client::Msg>,
    stderr: Vec<u8>,
    stderr_truncated: bool,
    timeouts: GitTimeouts,
}

impl RemoteGitClient {
    /// Connect using the application's SSH pool and launch an absolute remote helper path.
    /// Only the two quoted paths enter the remote shell; operations use framed JSON stdin.
    pub fn connect(
        conn: &SshConnection,
        helper_path: &str,
        project_root: &str,
    ) -> Result<Self, String> {
        let runtime = state().runtime()?;
        let pool = runtime.block_on(async { state().pool() });
        Self::connect_with(
            conn,
            helper_path,
            project_root,
            runtime,
            pool,
            GitTimeouts::DEFAULT,
        )
    }

    fn connect_with(
        conn: &SshConnection,
        helper_path: &str,
        project_root: &str,
        runtime: Arc<tokio::runtime::Runtime>,
        pool: Arc<SshPool>,
        timeouts: GitTimeouts,
    ) -> Result<Self, String> {
        validate_absolute_path(helper_path, "helper")?;
        validate_absolute_path(project_root, "root")?;
        let command = format!(
            "exec {} --root {}",
            quote_posix(helper_path),
            quote_posix(project_root)
        );
        let transport = runtime.block_on(async {
            let session = acquire_session(&pool, conn).await?;
            let opening = async {
                let handle = session.lock().await;
                if session.is_unhealthy_now() {
                    return Err("SSH session is unhealthy; remote Git was not started".to_string());
                }
                handle
                    .channel_open_session()
                    .await
                    .map_err(|e| format!("opening Git channel: {e}"))
            };
            let channel = match tokio::time::timeout(timeouts.start, opening).await {
                Ok(result) => result?,
                Err(_) => {
                    session.mark_unhealthy(Duration::from_secs(30));
                    let _ = tokio::time::timeout(
                        timeouts.close,
                        pool.evict_if_same(&conn.id, &session),
                    )
                    .await;
                    return Err(
                        "opening Git channel timed out; session retired, no requests replayed"
                            .into(),
                    );
                }
            };
            let mut transport = GitChannel {
                session,
                pool,
                connection_id: conn.id.clone(),
                channel,
                stderr: Vec::new(),
                stderr_truncated: false,
                timeouts,
            };
            let handshake = tokio::time::timeout(timeouts.start, async {
                transport
                    .channel
                    .exec(true, command)
                    .await
                    .map_err(|e| format!("starting remote Git helper: {e}"))?;
                let hello: Hello = transport.read_frame().await?;
                if hello.protocol_version != PROTOCOL_VERSION {
                    return Err(format!(
                        "Git helper protocol mismatch: expected {PROTOCOL_VERSION}, got {}",
                        hello.protocol_version
                    ));
                }
                Ok(())
            })
            .await;
            let error = match handshake {
                Ok(Ok(())) => return Ok(transport),
                Ok(Err(error)) => error,
                Err(_) => format!(
                    "remote Git helper handshake timed out ({:?})",
                    timeouts.start
                ),
            };
            let error = transport.with_stderr(error);
            let _ = transport.shutdown(false).await;
            Err(error)
        })?;
        Ok(Self {
            runtime,
            transport: Some(transport),
            next_id: 1,
            pending_write: None,
        })
    }

    /// Send exactly once. A transport/protocol failure permanently invalidates this client.
    /// A helper-reported operation error does not invalidate an otherwise synchronized stream.
    pub fn request(&mut self, operation: Operation) -> Result<Reply, String> {
        self.request_typed(operation)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn request_typed(
        &mut self,
        operation: Operation,
    ) -> Result<Reply, mt_git_helper::GitFailure> {
        if self.transport.is_none() {
            return Err(
                "remote Git client is closed; reconnect explicitly (never replay a failed write)"
                    .into(),
            );
        }
        if operation.is_write() && self.pending_write.is_some() {
            return Err(mt_git_helper::GitFailure::outcome_unknown(
                "a previous write still requires explicit completion verification",
            ));
        }
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or("remote Git request id exhausted")?;
        let is_write = operation.is_write();
        let expected_reply = operation.expected_reply_kind();
        let smart_checkout = operation.needs_smart_checkout_timeout();
        let frame = mt_git_helper::encode_frame(&Request {
            protocol_version: PROTOCOL_VERSION,
            id,
            operation,
        })
        .map_err(|e| format!("encoding remote Git request: {e}"))?;
        let transport = self.transport.as_mut().expect("checked above");
        let timeout = if smart_checkout {
            transport.timeouts.request.max(Duration::from_secs(7 * 60))
        } else {
            transport.timeouts.request
        };
        let result = self.runtime.block_on(async {
            tokio::time::timeout(timeout, async {
                transport.session.touch();
                // From this point delivery is ambiguous even if SSH data fails.
                transport.channel.data(frame.as_slice()).await
                    .map_err(|e| format!("sending remote Git request: {e}"))?;
                let response: Response = transport.read_frame().await?;
                if response.protocol_version != PROTOCOL_VERSION || response.id != id {
                    return Err(format!(
                        "remote Git response mismatch: expected protocol {PROTOCOL_VERSION}, id {id}; got protocol {}, id {}",
                        response.protocol_version, response.id
                    ));
                }
                if let Ok(reply) = &response.result
                    && reply.kind() != expected_reply
                {
                    return Err(format!(
                        "remote Git response kind mismatch: expected {expected_reply}, got {}",
                        reply.kind()
                    ));
                }
                transport.session.touch();
                Ok(response.result)
            }).await.unwrap_or_else(|_| Err(format!("remote Git request timed out ({timeout:?})")))
        });
        match result {
            Ok(reply) => {
                if is_write
                    && matches!(
                        &reply,
                        Err(mt_git_helper::GitFailure::OutcomeUnknown { .. })
                    )
                {
                    self.pending_write = Some(id);
                }
                reply
            }
            Err(error) => {
                let transport = self.transport.take().expect("checked above");
                let mut error = transport.with_stderr(error);
                if is_write {
                    self.pending_write = Some(id);
                }
                if let Err(cleanup) = self.runtime.block_on(transport.shutdown(false)) {
                    error.push_str(&format!("; {cleanup}"));
                }
                Err(if is_write {
                    mt_git_helper::GitFailure::outcome_unknown(format_args!(
                        "{error}; verify the repository before any further write; this request was not retried"
                    ))
                } else {
                    error.into()
                })
            }
        }
    }

    /// Decode while the originating client still owns its request identity.
    pub fn request_data<T: DeserializeOwned>(
        &mut self,
        operation: Operation,
    ) -> Result<T, mt_git_helper::GitFailure> {
        let is_write = operation.is_write();
        let reply = self.request_typed(operation)?;
        reply.decode().map_err(|message| {
            if is_write {
                self.mark_last_write_unknown();
                mt_git_helper::GitFailure::outcome_unknown(message)
            } else {
                message.into()
            }
        })
    }

    pub(crate) fn mark_last_write_unknown(&mut self) {
        self.pending_write = self.next_id.checked_sub(1);
    }

    /// Only a receipt from the original sequential helper proves completion.
    /// A newly connected helper or a repository read cannot supply this proof.
    pub(crate) fn prove_pending_write_completed(
        &mut self,
    ) -> Result<(), mt_git_helper::GitFailure> {
        let completed_id = self
            .pending_write
            .ok_or_else(|| mt_git_helper::GitFailure::from("no pending remote write"))?;
        match self.request_typed(Operation::Barrier { completed_id })? {
            Reply::Completed {
                completed_id: received,
            } if received == completed_id => {
                self.pending_write = None;
                Ok(())
            }
            _ => Err(mt_git_helper::GitFailure::outcome_unknown(
                "invalid write completion receipt",
            )),
        }
    }

    /// Whether the framed stream is still synchronized and can accept another request.
    pub(crate) fn is_connected(&self) -> bool {
        self.transport.is_some()
    }

    /// Send EOF, wait at most two seconds for helper exit, then close the channel (500ms).
    /// Failed cleanup retires the exact pooled session with a further two-second bound.
    pub fn close(&mut self) -> Result<(), String> {
        match self.transport.take() {
            Some(transport) => self.runtime.block_on(transport.shutdown(true)),
            None => Ok(()),
        }
    }
}

impl Drop for RemoteGitClient {
    fn drop(&mut self) {
        if let Some(transport) = self.transport.take() {
            self.runtime.spawn(async move {
                let _ = transport.shutdown(true).await;
            });
        }
    }
}

impl GitChannel {
    fn with_stderr(&self, mut error: String) -> String {
        if !self.stderr.is_empty() {
            error.push_str("; helper stderr: ");
            error.push_str(&String::from_utf8_lossy(&self.stderr));
            if self.stderr_truncated {
                error.push_str(" [truncated]");
            }
        }
        error
    }

    fn record_stderr(&mut self, data: &[u8]) {
        let count = data.len().min(STDERR_CAP - self.stderr.len());
        self.stderr.extend_from_slice(&data[..count]);
        self.stderr_truncated |= count != data.len();
    }

    async fn read_frame<T: DeserializeOwned>(&mut self) -> Result<T, String> {
        let mut bytes = Vec::with_capacity(4);
        let mut length = None;
        loop {
            match self.channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    let mut remaining = data.as_ref();
                    if bytes.len() < 4 {
                        let count = remaining.len().min(4 - bytes.len());
                        bytes.extend_from_slice(&remaining[..count]);
                        remaining = &remaining[count..];
                        if bytes.len() == 4 {
                            let size =
                                u32::from_be_bytes(bytes[..4].try_into().expect("four bytes"))
                                    as usize;
                            if size == 0 || size > MAX_FRAME_BYTES {
                                return Err(format!(
                                    "invalid Git frame length {size} (limit {MAX_FRAME_BYTES})"
                                ));
                            }
                            bytes.reserve(size);
                            length = Some(size);
                        }
                    }
                    if let Some(size) = length {
                        if remaining.len() > size + 4 - bytes.len() {
                            return Err(
                                "unexpected trailing stdout after Git protocol frame".into()
                            );
                        }
                        bytes.extend_from_slice(remaining);
                        if bytes.len() == size + 4 {
                            return mt_git_helper::decode_json(&bytes[4..]).map_err(|e| {
                                format!(
                                    "invalid Git protocol JSON ({:?}, line {}, column {})",
                                    e.classify(),
                                    e.line(),
                                    e.column()
                                )
                            });
                        }
                    }
                }
                Some(ChannelMsg::ExtendedData { data, ext: 1 }) => self.record_stderr(&data),
                Some(ChannelMsg::Success) | Some(ChannelMsg::WindowAdjusted { .. }) => {}
                Some(ChannelMsg::Failure) => return Err("remote Git exec request rejected".into()),
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    return Err(format!(
                        "remote Git helper exited with status {exit_status} before its response"
                    ));
                }
                Some(ChannelMsg::ExitSignal { .. }) => {
                    return Err("remote Git helper exited on a signal before its response".into());
                }
                Some(ChannelMsg::Eof | ChannelMsg::Close) | None => {
                    return Err("remote Git helper disconnected before a complete response".into());
                }
                Some(_) => {
                    return Err("unexpected SSH message on remote Git protocol channel".into());
                }
            }
        }
    }

    async fn shutdown(mut self, graceful: bool) -> Result<(), String> {
        let mut closed = false;
        let drain = tokio::time::timeout(self.timeouts.close, async {
            self.channel
                .eof()
                .await
                .map_err(|e| format!("closing Git stdin: {e}"))?;
            if graceful {
                loop {
                    match self.channel.wait().await {
                        None | Some(ChannelMsg::Close) => {
                            closed = true;
                            break;
                        }
                        Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                            self.record_stderr(&data)
                        }
                        Some(ChannelMsg::ExitStatus { exit_status }) if exit_status != 0 => {
                            return Err(format!(
                                "remote Git helper exited with status {exit_status}"
                            ));
                        }
                        Some(ChannelMsg::Data { .. }) => {
                            return Err("unexpected Git stdout during shutdown".into());
                        }
                        Some(ChannelMsg::ExitSignal { .. }) => {
                            return Err("remote Git helper exited on a signal".into());
                        }
                        _ => {}
                    }
                }
            }
            Ok(())
        })
        .await;
        // Always attempt close even if EOF/drain failed or timed out. Never signal/kill Git:
        // a dispatched commit may still be finishing and its outcome must be checked remotely.
        let close = tokio::time::timeout(Duration::from_millis(500), self.channel.close()).await;
        let cleanup = match drain {
            Ok(Ok(())) if closed || matches!(close, Ok(Ok(()))) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(format!(
                "remote Git helper shutdown timed out ({:?})",
                self.timeouts.close
            )),
            Ok(Ok(())) => Err("remote Git channel close failed or timed out (500ms)".into()),
        };
        self.session.touch();
        match cleanup {
            Ok(()) => Ok(()),
            result => {
                // Do not reuse a session whose channel cleanup could not be confirmed locally.
                self.session.mark_unhealthy(Duration::from_secs(30));
                let _ = tokio::time::timeout(
                    self.timeouts.close,
                    self.pool.evict_if_same(&self.connection_id, &self.session),
                )
                .await;
                let error = match result {
                    Err(error) => format!("{error}; session retired"),
                    Ok(()) => unreachable!(),
                };
                Err(self.with_stderr(error))
            }
        }
    }
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;

pub(crate) fn validate_absolute_path(path: &str, label: &str) -> Result<(), String> {
    if !path.starts_with('/') || path.contains('\0') {
        return Err(format!(
            "remote Git {label} must be an absolute POSIX path without NUL"
        ));
    }
    Ok(())
}

pub(crate) fn quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
