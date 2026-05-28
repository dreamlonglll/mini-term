# toml_edit 处理 array-of-tables (`[[...]]`)

> 编辑 TOML **数组表**(`[[projects]]` / `[[platforms]]` / `[[providers]]` 等)时,
> 与既有单一 nested table(`[features]` / `[mcp_servers.<name>]`)写法不同的标准模式。
> 沿用本 spec 可避免:类型推断歧义、误用 `Item::Table` 替代 `Item::ArrayOfTables`、
> 序列化后用户注释 / 字段顺序丢失等问题。

## Scope / Trigger

需要往 TOML 文件**追加 / 删除 / 列出**多个同名顶层 section(`[[xxx]]`),且必须
**保留用户既有注释和字段顺序**。

适用范围:
- `src-tauri/src/cc_connect.rs::cc_connect_import_project`(往 `~/.cc-connect/config.toml`
  追加 `[[projects]]`)
- 后续任何 mini-term 写第三方工具配置文件且涉及表数组的场景

不适用:单一 `[features]` / `[mcp_servers.<name>]` 这类 nested table,直接走
`hook_registry.rs` / `ssh_mcp_registry.rs` 既有模式(`doc["features"]["codex_hooks"] = value(true)`)。

## Signatures

```rust
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

// 1. 解析(失败转中文 error)
let mut doc: DocumentMut = std::fs::read_to_string(&path)
    .map_err(|e| format!("读取 {} 失败: {}", path.display(), e))?
    .parse::<DocumentMut>()
    .map_err(|e| format!("解析 TOML 失败: {}", e))?;

// 2. 取 array-of-tables(不存在则自动创建空数组)
let projects_item = doc
    .entry("projects")
    .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
let projects = projects_item
    .as_array_of_tables_mut()
    .ok_or("projects 不是 array of tables")?;

// 3. 构造新条目(嵌套子表)
let mut new_proj = Table::new();
new_proj["name"] = value("foo");

let mut agent = Table::new();
agent["type"] = value("claudecode");
let mut options = Table::new();
options["work_dir"] = value("/path/to/proj");
agent["options"] = Item::Table(options);
new_proj["agent"] = Item::Table(agent);

projects.push(new_proj);

// 4. 写回(注释和原有顺序自动保留)
std::fs::write(&path, doc.to_string())?;
```

## Contracts

| 操作 | API | 关键约束 |
|---|---|---|
| 读已存在表数组 | `doc.get("projects").and_then(\|i\| i.as_array_of_tables())` | 返 `Option<&ArrayOfTables>` |
| 按下标访问 | `projects.get(idx)` / `get_mut(idx)` | **没有 `Index<usize>`**;`projects[1]` 编译错 E0608 |
| 嵌套字段读 | `t["agent"]["options"]["work_dir"].as_str()` | `Table` 实现 `Index<&str>`;链式索引干净 |
| 嵌套字段写 | `t["agent"] = Item::Table(new_subtable)` | 整个子表替换;增量改单字段也用同样索引赋值 |
| 删除条目 | `projects.remove(idx)` | 按下标删,删除后后续下标变化,迭代删用 `retain` |

## Validation & Error Matrix

| 条件 | 错误处理 |
|---|---|
| 文件不存在 | `std::fs::read_to_string` IO error,封装中文 message |
| TOML 解析失败 | `.parse::<DocumentMut>()` 返 syntax error,保留 toml_edit 自带的定位信息 |
| 用户把 `projects` 改成 inline table | `as_array_of_tables_mut()` 返 None → 报"projects 不是 array of tables" |
| 重名追加 | 写盘**前** `.iter().any(\|t\| t.get("name").and_then(\|i\| i.as_str()) == Some(target))` 检测,返用户友好错误,**不动 doc** |
| 类型推断歧义(E0282) | `Option<&Item>` 链式 `.and_then(\|i\| i.as_str())` 触发;改用直接索引或显式 `\|i: &Item\| ...` |

## Good / Base / Bad Cases

- **Good**:读 → 检重 → 追加 → 写回 + **立即调外部 reload / restart 让生效**(参考 `cc-connect-integration.md`)
- **Base**:只追加,跳过重名检测 → 配置文件可能出现重复 entry,外部工具行为未定义
- **Bad**:用 `serde::Serialize` + `toml::to_string`(BurntSushi/toml)序列化整份 → **完全丢失用户注释和顺序**;BurntSushi/toml 不保留 trivia(comments/whitespace),toml_edit 才行

## Tests Required

每个新增 array-of-tables 操作必须有以下单测(模块内 `#[cfg(test)] mod tests`):

1. **追加保留注释**:输入字符串含 `# user comment`,追加后 `doc.to_string()` 仍含该注释
2. **追加保留既有 entry**:输入已有 `[[projects]] name = "existing"`,追加完第 0 项 name 仍 = "existing"
3. **不存在时自动创建数组**:输入完全没有 `projects` 段,`or_insert(ArrayOfTables::new())` 应能写入并产生 `[[projects]]` 头
4. **重名检测**:重名追加返 Err,且写盘前**不动 doc**
5. **Round-trip**:写完重新 `parse::<DocumentMut>()`,读出关键字段必须 = 原始值(不关心字符串引号风格 ── basic / literal 都允许)

参考已有:`src-tauri/src/cc_connect.rs::tests`(5 个测试覆盖前 4 项 + round-trip)。

## Wrong vs Correct

### Wrong

```rust
// E0608: ArrayOfTables 没有 Index<usize>
let projects = doc["projects"].as_array_of_tables().unwrap();
let first_name = projects[0]["name"].as_str().unwrap();  // ❌ 编译错

// E0282: as_str() 在 &Item 上类型推断歧义
let work_dir = projects.get(0).unwrap()
    .get("work_dir")
    .and_then(|i| i.as_str())  // ❌ 编译错
    .unwrap();

// 字符串引号风格 hardcode 到 assertion
assert!(serialized.contains(r#"work_dir = "D:\\Git\\proj""#));
// ❌ toml_edit 可能用 basic string(转义)或 literal string(不转义),assertion 脆
```

### Correct

```rust
// 用 .get(idx),Table 之后再用 [] 索引
let projects = doc["projects"].as_array_of_tables().unwrap();
let first = projects.get(0).unwrap();           // ✓
let first_name = first["name"].as_str().unwrap();   // ✓
let work_dir = first["agent"]["options"]["work_dir"].as_str().unwrap();  // ✓

// Round-trip 断言不关心引号风格
let reparsed: DocumentMut = serialized.parse().unwrap();
let actual = reparsed["projects"].as_array_of_tables().unwrap()
    .get(0).unwrap()["agent"]["options"]["work_dir"]
    .as_str().unwrap();
assert_eq!(actual, "D:\\Git\\proj");  // ✓ 比较解析后的值
```

## Related

- `hook_registry.rs` / `ssh_mcp_registry.rs`:nested table(非数组)模式,用于改 Claude / Codex 配置
- [`agent-config-injection.md`](./agent-config-injection.md):nested table 幂等写入约定
- [`cc-connect-integration.md`](./cc-connect-integration.md):本 spec 在 cc-connect 项目同步流程中的应用
