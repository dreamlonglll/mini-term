//! 外置主题包（Dream Skin 兼容格式）的目录扫描与读取。
//!
//! 目录约定：`{app_data_dir}/themes/<themeId>/`，四件套平铺
//! （theme.json 必需；theme.css / background.jpg 可选）。
//! 本模块只负责**文件层**：列举 / 导入 / 删除 / 取原文与二进制资源。
//! theme.json 的语义校验与"配色 → 运行时主题"的映射不在这里。
//!
//! # 与 gpui-component 主题层的关系（后续 wave，本次不做）
//!
//! 主题包里「配色」那一半将来可以映射到 `gpui_component::theme` 的 JSON schema
//! 加 `registry` 运行时切换，不必自己再造一套 token 系统；「背景图 / 字体 /
//! 终端配色」那一半是 mini-term 特有的，留在本 crate。
//! **本 crate 不依赖 gpui**，映射代码属于 `mt-ui`。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemePackEntry {
    /// themes/ 下的**目录名**,即主题包 id。
    ///
    /// ⚠️ 与 theme.json 里的 `id` 字段**不是一回事**:两者不一致时(用户把目录
    /// 改了名 / 从别处拷来的包)以目录名为准 —— [`ThemePacks::read`]、
    /// [`ThemePacks::delete`]、[`ThemePacks::read_asset`] 全按目录名定位,
    /// 上层拿这个字段当身份就永远对得上。原版口径同此
    /// (`themePackManager.ts:75-81` 的 `ThemePackMeta.themeId` +
    /// `parseThemePack` 的「以目录名为准」告警)。
    pub theme_id: String,
    /// theme.json 原文，由上层解析校验
    pub theme_json: String,
    /// 包目录绝对路径（设置页卡片缩略图组背景用）
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemePackData {
    pub theme_json: String,
    pub theme_css: Option<String>,
    /// 主题包目录绝对路径，上层据此拼背景图路径
    pub dir: PathBuf,
}

/// 示例主题包：与仓库 `docs/theme-pack-example/` **同一份文件**，编译期嵌入。
/// 文档里的模板和用户点「生成示例」拿到的包因此永远不会漂开。
const EXAMPLE_THEME_ID: &str = "example";
const EXAMPLE_THEME_JSON: &str = include_str!("../../../docs/theme-pack-example/theme.json");
const EXAMPLE_THEME_CSS: &str = include_str!("../../../docs/theme-pack-example/theme.css");
const EXAMPLE_THEME_README: &str = include_str!("../../../docs/theme-pack-example/README.md");

/// 主题包目录（`{app_data_dir}/themes`）的句柄。
///
/// 原实现是一组 `#[tauri::command] fn xxx(app: AppHandle)`，每个都自己从
/// `AppHandle` 现算 themes 目录；去 Tauri 化后目录成了显式状态，测试可以指向
/// 临时目录，不再需要一个 app。
pub struct ThemePacks {
    root: PathBuf,
}

impl ThemePacks {
    /// 指向 `{active_data_dir}/themes` —— 认 `MT_APP_DATA_DIR`
    /// (见 [`crate::paths::active_data_dir`])。
    ///
    /// 此前 [`crate::paths::themes_dir`] 钉死在装机版目录上,dev 实例只能
    /// 自己拼路径走 [`Self::at`] 绕开;现在两条路重合,宿主直接用这个入口。
    pub fn open() -> Result<Self> {
        Ok(Self::at(crate::paths::themes_dir()?))
    }

    /// 指向任意目录（测试用）。
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 主题包根目录路径（供设置页「打开主题目录」使用）。**不保证已存在**。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 根目录不存在则创建。每个入口都先过这一步——与原 `themes_dir()` 同语义，
    /// 保证全新安装时 `list()` 返回空列表而不是"目录不存在"的错误。
    fn ensure_root(&self) -> Result<&Path> {
        if !self.root.exists() {
            fs::create_dir_all(&self.root)
                .with_context(|| format!("创建主题目录失败: {}", self.root.display()))?;
        }
        Ok(&self.root)
    }

    /// 列举全部主题包（按 id 排序）。
    ///
    /// id = **目录名**(见 [`ThemePackEntry::theme_id`]);theme.json 的语义解析
    /// 归上层,本层连它的 `id` 字段都不看。
    pub fn list(&self) -> Result<Vec<ThemePackEntry>> {
        let dir = self.ensure_root()?;
        let mut out = Vec::new();
        for entry in fs::read_dir(dir)?.flatten() {
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
                dir: path,
            });
        }
        out.sort_by(|a, b| a.theme_id.cmp(&b.theme_id));
        Ok(out)
    }

    /// 读一个主题包的 theme.json / theme.css 原文。`theme_id` 是**目录名**
    /// (`themes/<theme_id>/`),不是 theme.json 里的 `id` 字段。
    pub fn read(&self, theme_id: &str) -> Result<ThemePackData> {
        validate_theme_id(theme_id)?;
        let dir = self.ensure_root()?.join(theme_id);
        let theme_json = fs::read_to_string(dir.join("theme.json"))
            .map_err(|e| anyhow!("读取 {theme_id}/theme.json 失败: {e}"))?;
        let theme_css = fs::read_to_string(dir.join("theme.css")).ok();
        Ok(ThemePackData {
            theme_json,
            theme_css,
            dir,
        })
    }

    /// 在 themes/ 下生成一份示例主题包，供用户照着改（字段说明在包内 README.md）。
    ///
    /// 目录已存在时**报错而非覆盖**：用户多半已经在那份上改过东西，静默覆盖等于
    /// 删掉他的皮肤；要重来就先删掉或改名，语义清楚。
    ///
    /// ⚠️ **设置页已无入口**：原「生成示例」按钮改成了跳转仓库皮肤库的外链
    /// （`pages_appearance.rs` 的 `THEME_GALLERY_URL`），本函数眼下没有 UI 调用方。
    /// 保留它是为了那份编译期对账 —— [`EXAMPLE_THEME_JSON`] 等三个 `include_str!`
    /// 钉着 `docs/theme-pack-example/`，`embedded_example_pack_matches_frontend_contract`
    /// 会在坏模板进仓库前就失败。连函数一起删掉，那份字段文档就没人看着了。
    pub fn create_example(&self) -> Result<String> {
        let dir = self.ensure_root()?.join(EXAMPLE_THEME_ID);
        if dir.exists() {
            bail!("示例主题已存在（themes/{EXAMPLE_THEME_ID}）：先删除或改名，再重新生成");
        }
        fs::create_dir_all(&dir).map_err(|e| anyhow!("创建示例主题目录失败: {e}"))?;
        let written = (|| -> Result<()> {
            for (name, body) in [
                ("theme.json", EXAMPLE_THEME_JSON),
                ("theme.css", EXAMPLE_THEME_CSS),
                ("README.md", EXAMPLE_THEME_README),
            ] {
                fs::write(dir.join(name), body).map_err(|e| anyhow!("写入 {name} 失败: {e}"))?;
            }
            Ok(())
        })();
        // 写到一半失败（盘满/权限/杀软锁文件）必须把目录收走：留下只有 theme.json 的
        // 残包，list() 照样把它列成可选皮肤，而下一次「生成示例」又会撞
        // 上面那句「已存在」——用户从此自愈不了，只能手工去删目录
        if let Err(e) = written {
            let _ = fs::remove_dir_all(&dir);
            return Err(e);
        }
        Ok(EXAMPLE_THEME_ID.to_string())
    }

    /// 把用户选择的主题文件夹拷入 themes/（四件套平铺，只拷顶层文件）。
    /// 返回落库后的主题 id（目录名）。
    pub fn import_dir(&self, src_dir: impl AsRef<Path>) -> Result<String> {
        let src = src_dir.as_ref();
        if !src.join("theme.json").is_file() {
            bail!("所选文件夹缺少 theme.json，不是主题包");
        }
        let theme_id = src
            .file_name()
            .ok_or_else(|| anyhow!("非法路径"))?
            .to_string_lossy()
            .into_owned();
        validate_theme_id(&theme_id)?;
        install_pack(self.ensure_root()?, &theme_id, src)?;
        Ok(theme_id)
    }

    /// 从 zip 包导入：解压到临时目录，定位含 theme.json 的根（zip 根或唯一顶层目录），
    /// 移入 themes/。返回主题 id。
    pub fn import_zip(&self, zip_path: impl AsRef<Path>) -> Result<String> {
        let zip_path = zip_path.as_ref();
        let file = fs::File::open(zip_path).map_err(|e| anyhow!("打开 zip 失败: {e}"))?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| anyhow!("zip 格式无效: {e}"))?;

        let themes = self.ensure_root()?;
        let extract_dir = themes.join(".tmp-extract");
        let _ = fs::remove_dir_all(&extract_dir);
        fs::create_dir_all(&extract_dir)?;
        let cleanup = |e: anyhow::Error| {
            let _ = fs::remove_dir_all(&extract_dir);
            e
        };
        archive
            .extract(&extract_dir)
            .map_err(|e| cleanup(anyhow!("解压失败: {e}")))?;

        // 定位主题包根：zip 根平铺，或整包套在唯一顶层目录里
        let pack_root = if extract_dir.join("theme.json").is_file() {
            extract_dir.clone()
        } else {
            let entries: Vec<_> = fs::read_dir(&extract_dir)
                .map_err(|e| cleanup(e.into()))?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && p.join("theme.json").is_file())
                .collect();
            match entries.as_slice() {
                [single] => single.clone(),
                _ => return Err(cleanup(anyhow!("zip 内未找到含 theme.json 的主题包目录"))),
            }
        };

        // 主题 id：优先用包根目录名；zip 根平铺时用 zip 文件名（去扩展名）。
        // 后者是**用户选中的任意文件名**，必须过 validate_theme_id：`...zip` 的
        // file_stem 是 `..`，没这道闸下面的安装会打到 app_data_dir 上。
        let theme_id = if pack_root == extract_dir {
            zip_path
                .file_stem()
                .ok_or_else(|| cleanup(anyhow!("非法 zip 文件名")))?
                .to_string_lossy()
                .into_owned()
        } else {
            pack_root.file_name().unwrap().to_string_lossy().into_owned()
        };
        validate_theme_id(&theme_id).map_err(&cleanup)?;

        install_pack(themes, &theme_id, &pack_root).map_err(&cleanup)?;
        let _ = fs::remove_dir_all(&extract_dir);
        Ok(theme_id)
    }

    /// 读取包内二进制资源（背景图）。
    ///
    /// 原实现返回 base64：那是 WebView 的 asset 协议加载失败时的兜底通道
    /// （CSS 背景图加载失败是静默的）。单进程里没有跨边界传输，直接给字节；
    /// `theme_id` / `file` 的路径分量校验一字未改。
    pub fn read_asset(&self, theme_id: &str, file: &str) -> Result<Vec<u8>> {
        validate_theme_id(theme_id)?;
        if file.is_empty() || file.contains(['/', '\\', ':']) || file.contains("..") {
            bail!("非法路径分量: {file}");
        }
        let path = self.ensure_root()?.join(theme_id).join(file);
        fs::read(&path).map_err(|e| anyhow!("读取 {theme_id}/{file} 失败: {e}"))
    }

    /// 删除主题包目录。
    pub fn delete(&self, theme_id: &str) -> Result<()> {
        validate_theme_id(theme_id)?;
        let dir = self.ensure_root()?.join(theme_id);
        if !dir.is_dir() {
            bail!("主题包不存在: {theme_id}");
        }
        fs::remove_dir_all(&dir).map_err(|e| anyhow!("删除失败: {e}"))
    }
}

/// 主题 id 的路径分量校验：id 只能是 themes/ 下的**一层目录名**。
///
/// 少了它，`themes.join(id)` 会逃出主题目录，而导入路径紧接着就是
/// `remove_dir_all(&dest)`——`Path::new("...zip").file_stem()` 返回 `".."`
/// （`"..zip"` 返回 `"."`），于是删掉的是整个 app_data_dir（config.json 一并
/// 消失）或整个 themes/。Windows 上还要挡 `:`，`join("C:")` 会产生盘符相对路径。
fn validate_theme_id(id: &str) -> Result<()> {
    if id.is_empty() || id.contains(['/', '\\', ':']) || id.contains("..") || id == "." {
        bail!("非法主题 id: {id}");
    }
    Ok(())
}

/// 把 `pack_root` 下的顶层文件安装为 `themes/<theme_id>`。
///
/// 先拷进同目录下的暂存目录并在那里校验 manifest，通过后才用 rename 换掉既有
/// 目录。此前是 `create_dir_all(dest)` → 拷贝 → 校验失败再 `remove_dir_all(dest)`
/// （zip 路径更是先删后拷），导入一个同名的坏包会连带删掉用户手工调过的既有皮肤。
fn install_pack(themes: &Path, theme_id: &str, pack_root: &Path) -> Result<()> {
    let staging = themes.join(format!(".tmp-install-{theme_id}"));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| anyhow!("创建暂存目录失败: {e}"))?;

    let staged = (|| -> Result<()> {
        for entry in fs::read_dir(pack_root)?.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            fs::copy(&path, staging.join(entry.file_name()))
                .map_err(|e| anyhow!("拷贝 {} 失败: {e}", entry.file_name().to_string_lossy()))?;
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
            anyhow!("替换既有主题失败: {e}")
        })?;
    }
    if let Err(e) = fs::rename(&staging, &dest) {
        if had_old {
            let _ = fs::rename(&backup, &dest);
        }
        let _ = fs::remove_dir_all(&staging);
        bail!("安装主题失败: {e}");
    }
    let _ = fs::remove_dir_all(&backup);
    Ok(())
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
fn verify_manifest(dir: &Path) -> Result<()> {
    let manifest_path = dir.join("manifest.json");
    let Ok(text) = fs::read_to_string(&manifest_path) else {
        return Ok(());
    };
    let manifest: Manifest =
        serde_json::from_str(&text).map_err(|e| anyhow!("manifest.json 解析失败: {e}"))?;
    for f in &manifest.files {
        if f.path.contains(['/', '\\']) || f.path.contains("..") {
            bail!("manifest files 含非法路径: {}", f.path);
        }
        let data = fs::read(dir.join(&f.path))
            .map_err(|e| anyhow!("manifest 声明的文件 {} 读取失败: {e}", f.path))?;
        if data.len() as u64 != f.bytes {
            bail!(
                "{} 大小不符: 期望 {} 实际 {}（包可能损坏）",
                f.path,
                f.bytes,
                data.len()
            );
        }
        let digest = Sha256::digest(&data);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        if !hex.eq_ignore_ascii_case(&f.sha256) {
            bail!("{} sha256 不符（包可能损坏）", f.path);
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

        let err = install_pack(&themes, "dracula", &src).unwrap_err().to_string();
        assert!(err.contains("大小不符"), "实际错误: {err}");
        assert_eq!(
            fs::read_to_string(existing.join("theme.json")).unwrap(),
            r#"{"name":"用户改过的版本"}"#
        );
        assert!(
            !themes.join(".tmp-install-dracula").exists(),
            "暂存目录未清理"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// 示例包是「文档模板」与「一键生成」共用的同一份文件，跑偏了两边一起坏。
    /// 本 crate 不做 theme.json 语义校验，这里按上层的必需字段给嵌入的示例
    /// 体检一遍，编译期就把坏模板挡住。
    #[test]
    fn embedded_example_pack_matches_frontend_contract() {
        assert!(validate_theme_id(EXAMPLE_THEME_ID).is_ok());
        let v: serde_json::Value = serde_json::from_str(EXAMPLE_THEME_JSON).unwrap();
        assert_eq!(v["id"].as_str(), Some(EXAMPLE_THEME_ID));
        assert!(v["name"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(matches!(
            v["appearance"].as_str(),
            Some("dark") | Some("light")
        ));
        for key in [
            "background",
            "panel",
            "panelAlt",
            "accent",
            "text",
            "muted",
            "line",
        ] {
            assert!(v["colors"][key].as_str().is_some(), "colors.{key} 缺失");
        }
        // tokens 逃生舱的键名必须是 -- 开头的 CSS 变量、值必须是字符串
        for (key, value) in v["tokens"].as_object().unwrap() {
            assert!(key.starts_with("--"), "tokens 键名非法: {key}");
            assert!(value.is_string(), "tokens.{key} 必须是字符串");
        }
        // 示例包不带背景图：写了 image 却没有图，终端会被透明化而氛围层挂不上
        assert!(v.get("image").is_none(), "示例包不应声明 image");
        // 与 sanitize 同序：先剥注释再查 —— 注释里那句「禁 @import」
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

    /// 回归测试(用户真机 v0.13.x GPUI 版):目录名与 theme.json 的 `id` 不一致的
    /// 包(`themes/ember-new/` 里写着 `"id": "ember-dusk"`)必须**按目录名**被列出
    /// 与读取 —— 上层若拿 json 的 `id` 去 `read`,就是「列表看得见、一点应用就报
    /// 皮肤应用失败」那个 bug。
    #[test]
    fn 目录名与_json_里的_id_不一致时以目录名为准() {
        let root = unique_test_root("id-mismatch");
        let packs = ThemePacks::at(root.join("themes"));
        write_pack(
            &packs.root().join("ember-new"),
            r#"{"id":"ember-dusk","name":"Ember Dusk"}"#,
        );

        let listed = packs.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].theme_id, "ember-new", "身份是目录名");
        assert!(listed[0].theme_json.contains("ember-dusk"), "原文原样带出");

        // 目录名读得到,json 的 id 读不到(没有这个目录)
        assert!(packs.read("ember-new").is_ok());
        assert!(packs.read("ember-dusk").is_err());
        // 资源读取与删除同一把尺子
        fs::write(packs.root().join("ember-new").join("background.png"), b"x").unwrap();
        assert_eq!(packs.read_asset("ember-new", "background.png").unwrap(), b"x");
        assert!(packs.read_asset("ember-dusk", "background.png").is_err());
        assert!(packs.delete("ember-dusk").is_err());
        assert!(packs.delete("ember-new").is_ok());

        let _ = fs::remove_dir_all(&root);
    }

    /// 去 Tauri 化后的入口体检:列举跳过暂存目录与无 theme.json 的目录、
    /// 读取/删除/资源读取的路径校验仍在、生成示例不覆盖既有目录。
    #[test]
    fn packs_entry_points_round_trip() {
        let root = unique_test_root("packs-api");
        let packs = ThemePacks::at(root.join("themes"));
        // 全新安装:目录还不存在,list 应给空列表而不是报错
        assert!(packs.list().unwrap().is_empty());

        let id = packs.create_example().unwrap();
        assert_eq!(id, EXAMPLE_THEME_ID);
        // 已存在 → 报错而非覆盖
        assert!(packs.create_example().is_err());

        // 暂存目录与无 theme.json 的目录都不算主题包
        fs::create_dir_all(packs.root().join(".tmp-extract")).unwrap();
        fs::write(packs.root().join(".tmp-extract/theme.json"), "{}").unwrap();
        fs::create_dir_all(packs.root().join("not-a-pack")).unwrap();

        let listed = packs.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].theme_id, EXAMPLE_THEME_ID);
        assert_eq!(listed[0].dir, packs.root().join(EXAMPLE_THEME_ID));

        let data = packs.read(EXAMPLE_THEME_ID).unwrap();
        assert_eq!(data.theme_json, EXAMPLE_THEME_JSON);
        assert!(data.theme_css.is_some());

        // 资源读取:拿到原始字节;路径分量非法一律拒绝
        let asset = packs.read_asset(EXAMPLE_THEME_ID, "README.md").unwrap();
        assert_eq!(asset, EXAMPLE_THEME_README.as_bytes());
        assert!(packs.read_asset(EXAMPLE_THEME_ID, "../config.json").is_err());
        assert!(packs.read_asset("..", "theme.json").is_err());

        // 导入一个目录 → 目录名即 id
        let src = root.join("dracula");
        write_pack(&src, r#"{"name":"新"}"#);
        assert_eq!(packs.import_dir(&src).unwrap(), "dracula");
        assert_eq!(packs.list().unwrap().len(), 2);

        packs.delete("dracula").unwrap();
        assert_eq!(packs.list().unwrap().len(), 1);
        // 不存在 / 非法 id 都要报错,不能静默成功
        assert!(packs.delete("dracula").is_err());
        assert!(packs.delete("..").is_err());

        let _ = fs::remove_dir_all(&root);
    }

    /// 仓库 `theme/` 下的成品皮肤是**给用户下载的分发物**,坏了却没有任何运行时
    /// 信号 —— 解析不了的包在列表里是静默跳过的,用户只会看到「装了没反应」。
    /// 这里把每一份都真的导入一遍:`import_dir` 内部要跑 manifest 的
    /// bytes + sha256 核对,导入成功即证明包既没缺件、也没在提交/签出途中损坏。
    ///
    /// ⚠️ manifest 只登记二进制资源。文本文件会被 Git 按 `core.autocrlf` 改写
    /// 换行,字节数与哈希随平台漂,登记进去等于让每个下载者都撞「大小不符」。
    #[test]
    fn 仓库分发的成品皮肤能被导入且不缺件() {
        let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../theme");
        let root = unique_test_root("shipped-packs");
        let packs = ThemePacks::at(root.join("themes"));

        let mut count = 0;
        for entry in fs::read_dir(&shipped).unwrap().flatten() {
            let src = entry.path();
            if !src.is_dir() {
                continue; // theme/README.md 这类说明文件不是皮肤
            }
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            let id = packs
                .import_dir(&src)
                .unwrap_or_else(|e| panic!("{dir_name} 导入失败: {e:#}"));
            assert_eq!(id, dir_name, "皮肤身份就是目录名");

            // 声明了背景图就必须真的跟着进包:导入只拷**顶层文件**,
            // 图搁子目录里会被静默丢下,装完只剩一张纯色皮
            let data = packs.read(&id).unwrap();
            let def: serde_json::Value = serde_json::from_str(&data.theme_json)
                .unwrap_or_else(|e| panic!("{dir_name}/theme.json 不是合法 JSON: {e}"));
            if let Some(image) = def["image"].as_str().filter(|s| !s.trim().is_empty()) {
                assert!(
                    data.dir.join(image).is_file(),
                    "{dir_name} 声明了背景图 {image},包里却没有"
                );
            }
            // 目录名与 json 的 `id` 不一致是踩过的坑(见上一条测试):自家分发的
            // 包不许再留这个雷,否则文档里写的 id 和实际装出来的对不上
            assert_eq!(
                def["id"].as_str(),
                Some(dir_name.as_str()),
                "{dir_name}/theme.json 的 id 应与目录名一致"
            );
            count += 1;
        }
        assert!(count > 0, "theme/ 下一个成品皮肤都没有 —— 路径写错了?");

        // 一键下载的 zip 与同名文件夹必须是**同一份东西**:网页上下 zip 的人
        // 和 clone 仓库的人拿到的皮肤不能不一样。zip 是手工打的,改了文件夹忘了
        // 重打包,两边就会静静地漂开。
        let zip_root = unique_test_root("shipped-zips");
        let zip_packs = ThemePacks::at(zip_root.join("themes"));
        for entry in fs::read_dir(&shipped).unwrap().flatten() {
            let path = entry.path();
            let is_zip = path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("zip"))
                .unwrap_or(false);
            if !is_zip {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = zip_packs
                .import_zip(&path)
                .unwrap_or_else(|e| panic!("{name} 导入失败: {e:#}"));

            let from_zip = zip_packs.read(&id).unwrap();
            let from_dir = packs
                .read(&id)
                .unwrap_or_else(|e| panic!("{name} 没有对应的同名文件夹({id}): {e:#}"));
            assert_eq!(
                from_zip.theme_json, from_dir.theme_json,
                "{name} 与文件夹版的 theme.json 已漂开,重新打包"
            );

            // 背景图同样要逐字节对齐 —— theme.json 一致但图不同,装出来是两个皮肤
            let def: serde_json::Value = serde_json::from_str(&from_zip.theme_json).unwrap();
            if let Some(image) = def["image"].as_str().filter(|s| !s.trim().is_empty()) {
                assert_eq!(
                    fs::read(from_zip.dir.join(image)).unwrap(),
                    fs::read(from_dir.dir.join(image)).unwrap(),
                    "{name} 与文件夹版的 {image} 不是同一张图,重新打包"
                );
            }
        }
        let _ = fs::remove_dir_all(&zip_root);

        let _ = fs::remove_dir_all(&root);
    }
}
