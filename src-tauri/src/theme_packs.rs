//! 外置主题包（Dream Skin 兼容格式）的目录扫描与读取。
//!
//! 目录约定：`{app_data_dir}/themes/<themeId>/`，四件套平铺
//! （theme.json 必需；theme.css / background.jpg 可选）。
//! Rust 侧只负责读文件文本，theme.json 的校验/映射在前端 themePackManager.ts。

use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

/// 主题包根目录，不存在则创建。
fn themes_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("themes");
    if !dir.exists() {
        fs::create_dir_all(&dir).map_err(|e| format!("创建主题目录失败: {e}"))?;
    }
    Ok(dir)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemePackEntry {
    /// themes/ 下的目录名，作为主题包 id
    pub theme_id: String,
    /// theme.json 原文，由前端解析校验
    pub theme_json: String,
}

#[tauri::command]
pub fn list_theme_packs(app: AppHandle) -> Result<Vec<ThemePackEntry>, String> {
    let dir = themes_dir(&app)?;
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // 无 theme.json 的目录直接跳过，不视为主题包
        let Ok(theme_json) = fs::read_to_string(path.join("theme.json")) else {
            continue;
        };
        out.push(ThemePackEntry {
            theme_id: entry.file_name().to_string_lossy().into_owned(),
            theme_json,
        });
    }
    out.sort_by(|a, b| a.theme_id.cmp(&b.theme_id));
    Ok(out)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemePackData {
    pub theme_json: String,
    pub theme_css: Option<String>,
    /// 主题包目录绝对路径，前端用 convertFileSrc 拼背景图 URL（Phase 2）
    pub dir: String,
}

#[tauri::command]
pub fn read_theme_pack(app: AppHandle, theme_id: String) -> Result<ThemePackData, String> {
    if theme_id.is_empty() || theme_id.contains(['/', '\\']) || theme_id.contains("..") {
        return Err(format!("非法主题 id: {theme_id}"));
    }
    let dir = themes_dir(&app)?.join(&theme_id);
    let theme_json = fs::read_to_string(dir.join("theme.json"))
        .map_err(|e| format!("读取 {theme_id}/theme.json 失败: {e}"))?;
    let theme_css = fs::read_to_string(dir.join("theme.css")).ok();
    Ok(ThemePackData {
        theme_json,
        theme_css,
        dir: dir.to_string_lossy().into_owned(),
    })
}

/// 供设置页「打开主题目录」使用。
#[tauri::command]
pub fn get_themes_dir(app: AppHandle) -> Result<String, String> {
    Ok(themes_dir(&app)?.to_string_lossy().into_owned())
}
