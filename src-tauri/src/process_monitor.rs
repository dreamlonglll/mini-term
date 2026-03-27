use serde::Serialize;
use std::collections::HashMap;
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyStatusChangePayload {
    pub pty_id: u32,
    pub status: String,
}

const AI_PROCESS_NAMES: &[&str] = &["claude.exe", "codex.exe", "claude", "codex"];

/// 一次性快照所有进程，返回 (pid, parent_pid, name)
#[cfg(target_os = "windows")]
fn snapshot_processes() -> Vec<(u32, u32, String)> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32First, Process32Next, PROCESSENTRY32, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::Foundation::CloseHandle;

    unsafe {
        let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return vec![],
        };

        let mut entry = PROCESSENTRY32::default();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;

        let mut result = vec![];
        if Process32First(snapshot, &mut entry).is_ok() {
            loop {
                let name = entry.szExeFile
                    .iter()
                    .take_while(|&&c| c != 0)
                    .map(|&c| c as u8 as char)
                    .collect::<String>()
                    .to_lowercase();
                result.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
                if Process32Next(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        result
    }
}

#[cfg(not(target_os = "windows"))]
fn snapshot_processes() -> Vec<(u32, u32, String)> {
    vec![]
}

/// shell 的子进程中是否包含 AI 进程
fn has_ai_child(shell_pid: u32, snapshot: &[(u32, u32, String)]) -> bool {
    snapshot.iter().any(|(_, ppid, name)| {
        *ppid == shell_pid && AI_PROCESS_NAMES.iter().any(|ai| name.contains(ai))
    })
}

/// AI 输出活跃超时阈值
const AI_ACTIVE_TIMEOUT: Duration = Duration::from_secs(3);

pub fn start_monitor(app: AppHandle, pty_manager: crate::pty::PtyManager) {
    thread::spawn(move || {
        let mut prev_statuses: HashMap<u32, String> = HashMap::new();

        loop {
            let pids = pty_manager.get_pids();
            let snapshot = if !pids.is_empty() { snapshot_processes() } else { vec![] };

            for (pty_id, child_pid) in &pids {
                let status = if let Some(pid) = child_pid {
                    if has_ai_child(*pid, &snapshot) {
                        // AI 进程存在 → 看最近是否有输出
                        if pty_manager.has_recent_output(*pty_id, AI_ACTIVE_TIMEOUT) {
                            "ai-working"
                        } else {
                            "ai-idle"
                        }
                    } else {
                        "idle"
                    }
                } else {
                    "idle"
                };

                let prev = prev_statuses.get(pty_id);
                if prev.map(|s| s.as_str()) != Some(status) {
                    let _ = app.emit("pty-status-change", PtyStatusChangePayload {
                        pty_id: *pty_id,
                        status: status.to_string(),
                    });
                    prev_statuses.insert(*pty_id, status.to_string());
                }
            }

            prev_statuses.retain(|id, _| pids.contains_key(id));

            let sleep_ms = if pids.is_empty() { 2000 } else { 500 };
            thread::sleep(Duration::from_millis(sleep_ms));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_ai_child() {
        let snap = vec![(2, 1, "node.exe".to_string())];
        assert!(!has_ai_child(1, &snap));
    }

    #[test]
    fn has_claude_child() {
        let snap = vec![(2, 1, "claude.exe".to_string())];
        assert!(has_ai_child(1, &snap));
    }

    #[test]
    fn has_codex_child() {
        let snap = vec![(2, 1, "codex.exe".to_string())];
        assert!(has_ai_child(1, &snap));
    }

    #[test]
    fn no_children_at_all() {
        assert!(!has_ai_child(1, &[]));
    }
}
