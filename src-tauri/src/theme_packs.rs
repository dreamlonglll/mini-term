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
        // 跳过导入过程的暂存目录（.tmp-extract / .tmp-install-* / .tmp-old-*）：
        // zip 根平铺时 .tmp-extract 里就有 theme.json，中途崩溃残留下来会被
        // 当成一个主题包列出来。真实主题 id 走 validate_theme_id，不会以 . 开头。
        if entry.file_name().to_string_lossy().starts_with('.') {
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

/// 主题 id 的路径分量校验：id 只能是 themes/ 下的**一层目录名**。
///
/// 少了它，`themes.join(id)` 会逃出主题目录，而导入路径紧接着就是
/// `remove_dir_all(&dest)`——`Path::new("...zip").file_stem()` 返回 `".."`
/// （`"..zip"` 返回 `"."`），于是删掉的是整个 app_data_dir（config.json 一并
/// 消失）或整个 themes/。Windows 上还要挡 `:`，`join("C:")` 会产生盘符相对路径。
fn validate_theme_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.contains(['/', '\\', ':']) || id.contains("..") || id == "." {
        return Err(format!("非法主题 id: {id}"));
    }
    Ok(())
}

#[tauri::command]
pub fn read_theme_pack(app: AppHandle, theme_id: String) -> Result<ThemePackData, String> {
    validate_theme_id(&theme_id)?;
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

/// 示例主题包：与仓库 `docs/theme-pack-example/` **同一份文件**，编译期嵌入。
/// 文档里的模板和用户点「生成示例」拿到的包因此永远不会漂开。
const EXAMPLE_THEME_ID: &str = "example";
const EXAMPLE_THEME_JSON: &str = include_str!("../../docs/theme-pack-example/theme.json");
const EXAMPLE_THEME_CSS: &str = include_str!("../../docs/theme-pack-example/theme.css");
const EXAMPLE_THEME_README: &str = include_str!("../../docs/theme-pack-example/README.md");

/// 在 themes/ 下生成一份示例主题包，供用户照着改（字段说明在包内 README.md）。
///
/// 目录已存在时**报错而非覆盖**：用户多半已经在那份上改过东西，静默覆盖等于
/// 删掉他的皮肤；要重来就先删掉或改名，语义清楚。
#[tauri::command]
pub fn create_example_theme_pack(app: AppHandle) -> Result<String, String> {
    let dir = themes_dir(&app)?.join(EXAMPLE_THEME_ID);
    if dir.exists() {
        return Err(format!(
            "示例主题已存在（themes/{EXAMPLE_THEME_ID}）：先删除或改名，再重新生成"
        ));
    }
    fs::create_dir_all(&dir).map_err(|e| format!("创建示例主题目录失败: {e}"))?;
    let written = (|| -> Result<(), String> {
        for (name, body) in [
            ("theme.json", EXAMPLE_THEME_JSON),
            ("theme.css", EXAMPLE_THEME_CSS),
            ("README.md", EXAMPLE_THEME_README),
        ] {
            fs::write(dir.join(name), body).map_err(|e| format!("写入 {name} 失败: {e}"))?;
        }
        Ok(())
    })();
    // 写到一半失败（盘满/权限/杀软锁文件）必须把目录收走：留下只有 theme.json 的
    // 残包，list_theme_packs 照样把它列成可选皮肤，而下一次「生成示例」又会撞
    // 上面那句「已存在」——用户从此自愈不了，只能手工去删目录
    if let Err(e) = written {
        let _ = fs::remove_dir_all(&dir);
        return Err(e);
    }
    Ok(EXAMPLE_THEME_ID.to_string())
}

/// 把 `pack_root` 下的顶层文件安装为 `themes/<theme_id>`。
///
/// 先拷进同目录下的暂存目录并在那里校验 manifest，通过后才用 rename 换掉既有
/// 目录。此前是 `create_dir_all(dest)` → 拷贝 → 校验失败再 `remove_dir_all(dest)`
/// （zip 路径更是先删后拷），导入一个同名的坏包会连带删掉用户手工调过的既有皮肤。
fn install_pack(themes: &Path, theme_id: &str, pack_root: &Path) -> Result<(), String> {
    let staging = themes.join(format!(".tmp-install-{theme_id}"));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("创建暂存目录失败: {e}"))?;

    let staged = (|| -> Result<(), String> {
        for entry in fs::read_dir(pack_root).map_err(|e| e.to_string())?.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            fs::copy(&path, staging.join(entry.file_name()))
                .map_err(|e| format!("拷贝 {} 失败: {e}", entry.file_name().to_string_lossy()))?;
        }
        verify_manifest(&staging)
    })();
    if let Err(e) = staged {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }

    // 换入：旧目录先挪到备份名，新目录就位后再删；rename 失败可原样回滚
    let dest = themes.join(theme_id);
    let backup = themes.join(format!(".tmp-old-{theme_id}"));
    let _ = fs::remove_dir_all(&backup);
    let had_old = dest.exists();
    if had_old {
        fs::rename(&dest, &backup).map_err(|e| {
            let _ = fs::remove_dir_all(&staging);
            format!("替换既有主题失败: {e}")
        })?;
    }
    if let Err(e) = fs::rename(&staging, &dest) {
        if had_old {
            let _ = fs::rename(&backup, &dest);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("安装主题失败: {e}"));
    }
    let _ = fs::remove_dir_all(&backup);
    Ok(())
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
    validate_theme_id(&theme_id)?;
    install_pack(&themes_dir(&app)?, &theme_id, &src)?;
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

    // 主题 id：优先用包根目录名；zip 根平铺时用 zip 文件名（去扩展名）。
    // 后者是**用户选中的任意文件名**，必须过 validate_theme_id：`...zip` 的
    // file_stem 是 `..`，没这道闸下面的安装会打到 app_data_dir 上。
    let theme_id = if pack_root == extract_dir {
        Path::new(&zip_path)
            .file_stem()
            .ok_or_else(|| cleanup("非法 zip 文件名".into()))?
            .to_string_lossy()
            .into_owned()
    } else {
        pack_root.file_name().unwrap().to_string_lossy().into_owned()
    };
    validate_theme_id(&theme_id).map_err(&cleanup)?;

    install_pack(&themes, &theme_id, &pack_root).map_err(&cleanup)?;
    let _ = fs::remove_dir_all(&extract_dir);
    Ok(theme_id)
}

/// 读取包内二进制资源（背景图）转 base64。
/// asset 协议加载失败时的前端兜底通道（CSS 背景图加载失败是静默的）。
#[tauri::command]
pub fn read_theme_asset(app: AppHandle, theme_id: String, file: String) -> Result<String, String> {
    validate_theme_id(&theme_id)?;
    if file.is_empty() || file.contains(['/', '\\', ':']) || file.contains("..") {
        return Err(format!("非法路径分量: {file}"));
    }
    let path = themes_dir(&app)?.join(&theme_id).join(&file);
    let data = fs::read(&path).map_err(|e| format!("读取 {theme_id}/{file} 失败: {e}"))?;
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(data))
}

/// 删除主题包目录。
#[tauri::command]
pub fn delete_theme_pack(app: AppHandle, theme_id: String) -> Result<(), String> {
    validate_theme_id(&theme_id)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_root(label: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mini-term-theme-test-{label}-{ts}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_pack(dir: &Path, theme_json: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("theme.json"), theme_json).unwrap();
    }

    /// 回归测试（PR #43 评审）：zip 根平铺时 theme_id 取自 zip 文件名，
    /// `Path::new("...zip").file_stem()` 返回 `".."`、`"..zip"` 返回 `"."`。
    /// 没有这道校验，`themes.join(id)` 会指向 app_data_dir 或 themes 本身，
    /// 而安装路径紧接着就要删掉那个目录——config.json 与全部皮肤一起没。
    #[test]
    fn theme_id_from_dotted_zip_name_is_rejected() {
        assert_eq!(Path::new("...zip").file_stem().unwrap(), "..");
        assert_eq!(Path::new("..zip").file_stem().unwrap(), ".");

        for bad in ["..", ".", "", "a/b", "a\\b", "..\\x", "C:", "x..y"] {
            assert!(validate_theme_id(bad).is_err(), "应拒绝: {bad:?}");
        }
        for ok in ["dracula", "my theme", "主题-1", "a.b"] {
            assert!(validate_theme_id(ok).is_ok(), "应放行: {ok:?}");
        }
    }

    /// 导入同名主题时若包校验不过，既有皮肤必须原样还在
    /// （此前是先删/先建 dest 再校验，坏包会连带删掉用户手工调过的主题）。
    #[test]
    fn failed_import_keeps_existing_pack_intact() {
        let root = unique_test_root("import-atomic");
        let themes = root.join("themes");
        let existing = themes.join("dracula");
        write_pack(&existing, r#"{"name":"用户改过的版本"}"#);

        // 坏包：manifest 声明的 sha256 对不上
        let src = root.join("src");
        write_pack(&src, r#"{"name":"坏包"}"#);
        fs::write(
            src.join("manifest.json"),
            r#"{"files":[{"path":"theme.json","bytes":999,"sha256":"00"}]}"#,
        )
        .unwrap();

        let err = install_pack(&themes, "dracula", &src).unwrap_err();
        assert!(err.contains("大小不符"), "实际错误: {err}");
        assert_eq!(
            fs::read_to_string(existing.join("theme.json")).unwrap(),
            r#"{"name":"用户改过的版本"}"#
        );
        assert!(!themes.join(".tmp-install-dracula").exists(), "暂存目录未清理");

        let _ = fs::remove_dir_all(&root);
    }

    /// 示例包是「文档模板」与「一键生成」共用的同一份文件，跑偏了两边一起坏。
    /// Rust 侧不做 theme.json 校验（在前端 parseThemePack），这里按它的必需
    /// 字段给嵌入的示例体检一遍，编译期就把坏模板挡住。
    #[test]
    fn embedded_example_pack_matches_frontend_contract() {
        assert!(validate_theme_id(EXAMPLE_THEME_ID).is_ok());
        let v: serde_json::Value = serde_json::from_str(EXAMPLE_THEME_JSON).unwrap();
        assert_eq!(v["id"].as_str(), Some(EXAMPLE_THEME_ID));
        assert!(v["name"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(matches!(v["appearance"].as_str(), Some("dark") | Some("light")));
        for key in ["background", "panel", "panelAlt", "accent", "text", "muted", "line"] {
            assert!(v["colors"][key].as_str().is_some(), "colors.{key} 缺失");
        }
        // tokens 逃生舱的键名必须是 -- 开头的 CSS 变量、值必须是字符串
        for (key, value) in v["tokens"].as_object().unwrap() {
            assert!(key.starts_with("--"), "tokens 键名非法: {key}");
            assert!(value.is_string(), "tokens.{key} 必须是字符串");
        }
        // 示例包不带背景图：写了 image 却没有图，终端会被透明化而氛围层挂不上
        assert!(v.get("image").is_none(), "示例包不应声明 image");
        // 与前端 sanitizeThemeCss 同序：先剥注释再查 —— 注释里那句「禁 @import」
        // 是说明不是规则，直接在原文上查会把自己的文档误判成违规
        let probe = strip_block_comments(EXAMPLE_THEME_CSS);
        assert!(!probe.contains("@import"), "theme.css 不允许 @import");
        assert!(!probe.contains("://"), "theme.css 不允许指向包外的引用");
    }

    /// 剥掉 `/* */` 块注释（对应前端 stripCssComments，取样用）
    fn strip_block_comments(css: &str) -> String {
        let mut out = String::new();
        let mut rest = css;
        while let Some(start) = rest.find("/*") {
            out.push_str(&rest[..start]);
            match rest[start + 2..].find("*/") {
                Some(end) => rest = &rest[start + 2 + end + 2..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }

    /// 成功路径：同名覆盖后是新包内容，且不留暂存/备份目录
    #[test]
    fn successful_import_replaces_pack_and_cleans_staging() {
        let root = unique_test_root("import-replace");
        let themes = root.join("themes");
        write_pack(&themes.join("dracula"), r#"{"name":"旧"}"#);
        // 旧包独有的文件在替换后不该残留（rename 换目录，不是逐文件覆盖）
        fs::write(themes.join("dracula").join("theme.css"), "/* 旧 */").unwrap();

        let src = root.join("src");
        write_pack(&src, r#"{"name":"新"}"#);

        install_pack(&themes, "dracula", &src).unwrap();
        assert_eq!(
            fs::read_to_string(themes.join("dracula").join("theme.json")).unwrap(),
            r#"{"name":"新"}"#
        );
        assert!(!themes.join("dracula").join("theme.css").exists());
        assert!(!themes.join(".tmp-install-dracula").exists());
        assert!(!themes.join(".tmp-old-dracula").exists());

        let _ = fs::remove_dir_all(&root);
    }
}
