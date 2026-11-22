//! 跨平台 PTY 子进程树探测：判断指定 shell 根进程下是否存在存活的 `ssh` 客户端。
//!
//! 仅由右键「SSH 连接」的后台轻量轮询按需调用（平时零开销）。
//! 进程表解析与防环 BFS 纯逻辑住在 `mt_app_core::quick_ssh::has_ssh_in_process_table`，
//! 本文件只做极薄的平台进程快照采集。

use mt_app_core::quick_ssh::{SshProcessProbe, has_ssh_in_process_table};

/// 检查 `root_pid` 的后代进程中是否有正在运行的 `ssh` / `ssh.exe`。
pub fn probe_ssh_descendant(root_pid: u32) -> SshProcessProbe {
    platform::probe(root_pid)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    pub(super) fn probe(root_pid: u32) -> SshProcessProbe {
        // SAFETY: Win32 标准工具帮助快照 API，按文档初始化 dwSize 并用 CloseHandle 释放。
        unsafe {
            let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return SshProcessProbe::Unavailable;
            };
            let mut owned: Vec<(u32, u32, String)> = Vec::new();
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            if Process32FirstW(snapshot, &mut entry).is_ok() {
                loop {
                    let len = entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                    owned.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
                    if Process32NextW(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snapshot);
            let refs: Vec<(u32, u32, &str)> = owned
                .iter()
                .map(|(pid, ppid, name)| (*pid, *ppid, name.as_str()))
                .collect();
            has_ssh_in_process_table(root_pid, &refs)
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    pub(super) fn probe(root_pid: u32) -> SshProcessProbe {
        let Ok(read_dir) = std::fs::read_dir("/proc") else {
            return SshProcessProbe::Unavailable;
        };
        let mut owned: Vec<(u32, u32, String)> = Vec::new();
        for entry in read_dir.flatten() {
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            if let Some((comm, ppid)) = parse_proc_stat(&stat) {
                owned.push((pid, ppid, comm.to_string()));
            }
        }
        let refs: Vec<(u32, u32, &str)> = owned
            .iter()
            .map(|(pid, ppid, name)| (*pid, *ppid, name.as_str()))
            .collect();
        has_ssh_in_process_table(root_pid, &refs)
    }

    /// 解析 `/proc/<pid>/stat` 中的 `(comm, ppid)`：用 `find('(')` 与 `rfind(')')`
    /// 界定 comm，避免进程名含空格或括号时切歪。
    fn parse_proc_stat(stat: &str) -> Option<(&str, u32)> {
        let lparen = stat.find('(')?;
        let rparen = stat.rfind(')')?;
        if rparen <= lparen {
            return None;
        }
        let comm = &stat[lparen + 1..rparen];
        let after = stat.get(rparen + 2..)?;
        // `after` 形如 `<state> <ppid> ...`
        let mut fields = after.split_whitespace();
        let _state = fields.next()?;
        let ppid = fields.next()?.parse::<u32>().ok()?;
        Some((comm, ppid))
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
mod platform {
    use super::*;
    use std::process::Command;

    pub(super) fn probe(root_pid: u32) -> SshProcessProbe {
        let Ok(output) = Command::new("ps").args(["-eo", "pid=,ppid=,comm="]).output() else {
            return SshProcessProbe::Unavailable;
        };
        if !output.status.success() {
            return SshProcessProbe::Unavailable;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut owned: Vec<(u32, u32, String)> = Vec::new();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let Some(pid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let Some(ppid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let Some(comm) = parts.next() else {
                continue;
            };
            owned.push((pid, ppid, comm.to_string()));
        }
        let refs: Vec<(u32, u32, &str)> = owned
            .iter()
            .map(|(pid, ppid, name)| (*pid, *ppid, name.as_str()))
            .collect();
        has_ssh_in_process_table(root_pid, &refs)
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    use super::*;

    pub(super) fn probe(_root_pid: u32) -> SshProcessProbe {
        SshProcessProbe::Unavailable
    }
}
