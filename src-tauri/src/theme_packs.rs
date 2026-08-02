//! 外置主题包（Dream Skin 兼容格式）的目录扫描与读取。
//!
//! 目录约定：`{app_data_dir}/themes/<themeId>/`，四件套平铺
//! （theme.json 必需；theme.css / background.jpg 可选）。
//! Rust 侧只负责读文件文本，theme.json 的校验/映射在前端 themePackManager.ts。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
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
    /// 包目录绝对路径（设置页卡片缩略图组背景 URL 用）
    pub dir: String,
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
            dir: path.to_string_lossy().into_owned(),
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

/// 把用户选择的主题文件夹拷入 themes/（四件套平铺，只拷顶层文件）。
/// 返回落库后的主题 id（目录名）。
#[tauri::command]
pub fn import_theme_pack(app: AppHandle, src_dir: String) -> Result<String, String> {
    let src = PathBuf::from(&src_dir);
    if !src.join("theme.json").is_file() {
        return Err("所选文件夹缺少 theme.json，不是主题包".into());
    }
    let theme_id = src
        .file_name()
        .ok_or("非法路径")?
        .to_string_lossy()
        .into_owned();
    let dest = themes_dir(&app)?.join(&theme_id);
    fs::create_dir_all(&dest).map_err(|e| format!("创建 {theme_id} 目录失败: {e}"))?;
    for entry in fs::read_dir(&src).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        fs::copy(&path, dest.join(entry.file_name()))
            .map_err(|e| format!("拷贝 {} 失败: {e}", entry.file_name().to_string_lossy()))?;
    }
    if let Err(e) = verify_manifest(&dest) {
        let _ = fs::remove_dir_all(&dest);
        return Err(e);
    }
    Ok(theme_id)
}

/// 从 zip 包导入：解压到临时目录，定位含 theme.json 的根（zip 根或唯一顶层目录），
/// 移入 themes/。返回主题 id。
#[tauri::command]
pub fn import_theme_pack_zip(app: AppHandle, zip_path: String) -> Result<String, String> {
    let file = fs::File::open(&zip_path).map_err(|e| format!("打开 zip 失败: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("zip 格式无效: {e}"))?;

    let themes = themes_dir(&app)?;
    let extract_dir = themes.join(".tmp-extract");
    let _ = fs::remove_dir_all(&extract_dir);
    fs::create_dir_all(&extract_dir).map_err(|e| e.to_string())?;
    let cleanup = |e: String| {
        let _ = fs::remove_dir_all(&extract_dir);
        e
    };
    archive
        .extract(&extract_dir)
        .map_err(|e| cleanup(format!("解压失败: {e}")))?;

    // 定位主题包根：zip 根平铺，或整包套在唯一顶层目录里
    let pack_root = if extract_dir.join("theme.json").is_file() {
        extract_dir.clone()
    } else {
        let entries: Vec<_> = fs::read_dir(&extract_dir)
            .map_err(|e| cleanup(e.to_string()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.join("theme.json").is_file())
            .collect();
        match entries.as_slice() {
            [single] => single.clone(),
            _ => return Err(cleanup("zip 内未找到含 theme.json 的主题包目录".into())),
        }
    };

    // 主题 id：优先用包根目录名；zip 根平铺时用 zip 文件名（去扩展名）
    let theme_id = if pack_root == extract_dir {
        Path::new(&zip_path)
            .file_stem()
            .ok_or_else(|| cleanup("非法 zip 文件名".into()))?
            .to_string_lossy()
            .into_owned()
    } else {
        pack_root.file_name().unwrap().to_string_lossy().into_owned()
    };

    verify_manifest(&pack_root).map_err(&cleanup)?;

    let dest = themes.join(&theme_id);
    let _ = fs::remove_dir_all(&dest);
    fs::create_dir_all(&dest).map_err(|e| cleanup(e.to_string()))?;
    for entry in fs::read_dir(&pack_root).map_err(|e| cleanup(e.to_string()))?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        fs::copy(&path, dest.join(entry.file_name())).map_err(|e| cleanup(e.to_string()))?;
    }
    let _ = fs::remove_dir_all(&extract_dir);
    Ok(theme_id)
}

/// 读取包内二进制资源（背景图）转 base64。
/// asset 协议加载失败时的前端兜底通道（CSS 背景图加载失败是静默的）。
#[tauri::command]
pub fn read_theme_asset(app: AppHandle, theme_id: String, file: String) -> Result<String, String> {
    for part in [&theme_id, &file] {
        if part.is_empty() || part.contains(['/', '\\']) || part.contains("..") {
            return Err(format!("非法路径分量: {part}"));
        }
    }
    let path = themes_dir(&app)?.join(&theme_id).join(&file);
    let data = fs::read(&path).map_err(|e| format!("读取 {theme_id}/{file} 失败: {e}"))?;
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(data))
}

/// 删除主题包目录。
#[tauri::command]
pub fn delete_theme_pack(app: AppHandle, theme_id: String) -> Result<(), String> {
    if theme_id.is_empty() || theme_id.contains(['/', '\\']) || theme_id.contains("..") {
        return Err(format!("非法主题 id: {theme_id}"));
    }
    let dir = themes_dir(&app)?.join(&theme_id);
    if !dir.is_dir() {
        return Err(format!("主题包不存在: {theme_id}"));
    }
    fs::remove_dir_all(&dir).map_err(|e| format!("删除失败: {e}"))
}

/// manifest.json 的 files 清单（只取校验需要的字段）。
#[derive(Deserialize)]
struct ManifestFile {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Deserialize)]
struct Manifest {
    files: Vec<ManifestFile>,
}

/// 有 manifest.json 时核对 files 的 bytes + sha256（防包损坏）；没有则跳过。
fn verify_manifest(dir: &Path) -> Result<(), String> {
    let manifest_path = dir.join("manifest.json");
    let Ok(text) = fs::read_to_string(&manifest_path) else {
        return Ok(());
    };
    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|e| format!("manifest.json 解析失败: {e}"))?;
    for f in &manifest.files {
        if f.path.contains(['/', '\\']) || f.path.contains("..") {
            return Err(format!("manifest files 含非法路径: {}", f.path));
        }
        let data = fs::read(dir.join(&f.path))
            .map_err(|e| format!("manifest 声明的文件 {} 读取失败: {e}", f.path))?;
        if data.len() as u64 != f.bytes {
            return Err(format!(
                "{} 大小不符: 期望 {} 实际 {}（包可能损坏）",
                f.path,
                f.bytes,
                data.len()
            ));
        }
        let digest = Sha256::digest(&data);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        if !hex.eq_ignore_ascii_case(&f.sha256) {
            return Err(format!("{} sha256 不符（包可能损坏）", f.path));
        }
    }
    Ok(())
}
