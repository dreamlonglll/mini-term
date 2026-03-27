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

/// 判断 shell 下的 AI 进程状态：
///   - 无 AI 子进程 → "idle"
///   - AI 子进程无后代 → "ai-idle"（等待用户输入）
///   - AI 子进程有后代 → "ai-working"（正在执行工具）
fn detect_status(shell_pid: u32, snapshot: &[(u32, u32, String)]) -> &'static str {
    let mut ai_pids = vec![];

    for (pid, ppid, name) in snapshot {
        if *ppid == shell_pid && AI_PROCESS_NAMES.iter().any(|ai| name.contains(ai)) {
            ai_pids.push(*pid);
        }
    }

    if ai_pids.is_empty() {
        return "idle";
    }

    // AI 进程是否有子进程 → 正在执行工具/命令
    for ai_pid in &ai_pids {
        if snapshot.iter().any(|(_, ppid, _)| ppid == ai_pid) {
            return "ai-working";
        }
    }

    "ai-idle"
}

pub fn start_monitor(app: AppHandle, pty_manager: crate::pty::PtyManager) {
    thread::spawn(move || {
        let mut prev_statuses: HashMap<u32, String> = HashMap::new();

        loop {
            let pids = pty_manager.get_pids();
            let snapshot = if !pids.is_empty() { snapshot_processes() } else { vec![] };

            for (pty_id, child_pid) in &pids {
                let status = if let Some(pid) = child_pid {
                    detect_status(*pid, &snapshot).to_string()
                } else {
                    "idle".to_string()
                };

                let prev = prev_statuses.get(pty_id);
                if prev.map(|s| s.as_str()) != Some(&status) {
                    let _ = app.emit("pty-status-change", PtyStatusChangePayload {
                        pty_id: *pty_id,
                        status: status.clone(),
                    });
                    prev_statuses.insert(*pty_id, status);
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
    fn status_idle_no_children() {
        assert_eq!(detect_status(1, &[]), "idle");
    }

    #[test]
    fn status_idle_non_ai_children() {
        let snap = vec![(2, 1, "node.exe".to_string())];
        assert_eq!(detect_status(1, &snap), "idle");
    }

    #[test]
    fn status_ai_idle() {
        let snap = vec![(2, 1, "claude.exe".to_string())];
        assert_eq!(detect_status(1, &snap), "ai-idle");
    }

    #[test]
    fn status_ai_working() {
        let snap = vec![
            (2, 1, "claude.exe".to_string()),
            (3, 2, "bash.exe".to_string()),
        ];
        assert_eq!(detect_status(1, &snap), "ai-working");
    }
}
