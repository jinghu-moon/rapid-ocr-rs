# rapid-ocr-rs 本地 Web 评估界面（`rapidocr serve`）里程碑执行记录

> 本文件按 `docs/05-local-web-demo-implementation.md` §12「证据要求」逐里程碑追加执行
> 记录：**命令、关键输出、与验收标准对照、未覆盖风险**。记录格式沿用
> `docs/04-windows-phase-reports.md`。
>
> 原始日志保存在 `target/`（不随仓库提交，可用文中命令复现）：
> `target/m0a-baseline-test.log`、`target/m0a-after-test.log`、`target/m0a-verify.log`、
> `target/m0a-formula-integration.log`、`target/m0a-dicts/hashes.csv`。
>
> 关联文档：`docs/05-local-web-demo-implementation.md`（协议与里程碑）、
> `docs/03-windows-only-optimization-tasks.md`（Windows 优化主线）、
> `AGENTS.md`（开发期工程规则：不考虑向后兼容、根因优先、禁止伪完成）。

---

## M0a：库内前置（`ModelSet` / 单一权威来源 / 字典 SHA-256）

**阶段**：M0a —— 对应 `docs/05` §11「M0」中**库内**的部分（§1.2、§5、§11 的
`ModelSet` / `ModelManifest` 通用化 / 字典哈希）。HTTP 服务器、`tiny_http`、`serve`
feature、CLI 参数与加固下载器属于 **M0b/M0c/M1**，本阶段**不做**（见文末「本阶段不做的事」）。
**日期**：2026-10-03
**提交**：`（未提交：按要求不 commit）`
**变更摘要**：

- 新增 `src/model_set.rs`：模型清单的唯一抽象（`ModelRole` / `ModelFileSpec` /
  `ModelSet` / `ModelFileState` / `ModelSetStatus`）与**唯一**的逐文件校验实现
  `validate_model_files`（一次返回**每个**文件的状态，不在第一个错误处返回）。
- 新增 `src/model_source.rs`：通用清单 `ModelManifest`（`schema_version` + `files`）与
  **单一来源选择规则**（`manifest.json` 存在则默认表完全不参与，无合并、无“可选覆盖”）。
- `src/model_registry.rs`：字典从裸 `dict_url` 改为一等 `dict:` 文件（URL + SHA256 +
  size）；新增 `formula:` 段与 `ModelRegistry::text_model_set` / `formula_model_set`，
  默认表由此可以直接产出 `ModelSet`。
- `assets/default_models.yaml`：30 个字典条目全部补上**实测** SHA-256 与体积；
  新增 `formula:` 段（含公式识别模型的 URL、SHA-256、体积）。
- `src/ocr/rec/recognizer.rs`：字典下载不再传 `None` 哈希（§1.2 记录的“哈希可选”缺口）。
- `src/api.rs`：删除死的 `ModelArtifact` / 四字段 `ModelManifest` / 未使用的笼统
  `ModelSource`（后者与新概念同名，且全仓库无任何使用者）。
- `assets/manifest.example.json`、`README.md`、`THIRD_PARTY_NOTES.md` 同步到新契约。

### 环境

| 项目 | 值 |
| --- | --- |
| target | Windows x64 + MSVC ABI：`x86_64-pc-windows-msvc` |
| OS | Microsoft Windows 11 IoT 企业版 LTSC 10.0.26100 |
| 工具链 | rustc 1.98.1 (48a229cea 2026-09-01)，cargo 1.98.1 (797e8a9bc 2026-08-05) |
| HTTP 客户端（仅用于取字典哈希） | curl 8.19.0 (Windows) libcurl/8.19.0 Schannel |
| crate | `rapid-ocr-rs` 0.7.0（`crates/rapid-ocr-rs`，独立 git 仓库，本阶段未提交） |
| 真实资产 | `OCR-Model/`（v6 tiny/small/medium + 公式模型）、`Formula-TestSet/` |

---

### 交付物 1：`ModelSet`，唯一的模型清单（§5.1 / §5.2）

**新增文件**：`src/model_set.rs`（643 行，含 10 个单元测试）；`src/exports.rs` 导出。

```rust
pub enum ModelRole { Detector, Classifier, Recognizer, Dictionary, Tokenizer, FormulaDetector, FormulaRecognizer }
impl ModelRole { pub const ALL: [Self; 7]; pub const fn as_str(self) -> &'static str }

pub struct ModelFileSpec { pub name: String, pub role: ModelRole, pub size_bytes: Option<u64>,
                           pub sha256: String, pub source_url: String }
impl ModelFileSpec {
    pub fn new(name: impl Into<String>, role: ModelRole, size_bytes: Option<u64>,
               sha256: impl Into<String>, source_url: impl Into<String>) -> Result<Self>;
    pub fn has_hash(&self) -> bool;
    pub fn has_source_url(&self) -> bool;
    pub fn validate_name(&self) -> Result<()>;
    pub fn state_in(&self, root: &Path) -> ModelFileState;
}

pub enum ModelFileState { Missing, Present, Corrupt { expected: String, actual: String } }

pub struct ModelSet { pub id: String, pub family: String, pub version: String, pub files: Vec<ModelFileSpec> }
impl ModelSet {
    pub fn validate(&self) -> Result<()>;
    pub fn has_role(&self, role: ModelRole) -> bool;
    pub fn declared_roles(&self) -> Vec<ModelRole>;
    pub fn missing_roles(&self, required: &[ModelRole]) -> Vec<ModelRole>;
    pub fn require_roles(&self, required: &[ModelRole]) -> Result<()>;
    pub fn status(&self, root: &Path) -> ModelSetStatus;
}

pub struct ModelSetStatus { pub set_id: String, pub files: Vec<(ModelFileSpec, ModelFileState)>,
                            pub complete: bool, pub download_bytes_total: Option<u64> }

pub fn validate_model_file_name(name: &str) -> Result<()>;
pub fn validate_model_files(files: &[ModelFileSpec], root: &Path) -> Vec<(ModelFileSpec, ModelFileState)>;
pub fn model_set_status(set: &ModelSet, root: &Path) -> ModelSetStatus;
```

**规则实现要点**（每条都有测试）：

| 规则（§5.1/§5.2） | 实现 | 测试 |
| --- | --- | --- |
| `name` 必须是裸相对文件名 | `validate_model_file_name`：拒绝绝对路径、`/`、`\`、盘符前缀、`.`、`..` | `path_escape_is_rejected_by_the_shared_rule` |
| `sha256` 必填 | `sha256: String`（空串 = 没有哈希），下载入口只接受具体哈希 | `a_manifest_file_must_carry_a_hash_and_a_bare_name` |
| 无哈希的文件不得参与 `complete` | `complete = !files.is_empty() && all(Present) && all(has_hash)` | `a_set_with_an_unhashed_file_can_never_be_complete`、`an_empty_set_is_never_complete` |
| 逐文件状态**不提前返回** | `validate_model_files` 与 `files` 同序等长 | `every_file_is_reported_in_order_and_not_stopped_at_the_first_error` |
| `download_bytes_total` | 缺失文件体积之和；任一缺失文件体积未知（或求和溢出）→ `None` | `download_bytes_total_sums_only_missing_files_with_known_sizes` |
| `ModelRole` 可序列化 | `#[serde(rename_all = "snake_case")]` + `as_str()` | `roles_round_trip_through_snake_case_json` |
| 缺 role 必须报错并列出缺失 role | `ModelSet::require_roles`（`ModelResolve`，列出缺失 + 已声明 role） | `require_roles_lists_every_missing_role` |
| `ModelManifest::validate_files` 改为薄包装 | 见交付物 2 | `the_current_schema_converts_to_a_model_set` |

**修改前后行为对比（交付物 1）**

| 项目 | 修改前 | 修改后 | 预期 |
| --- | --- | --- | --- |
| 逐文件校验 | `ModelManifest::validate_files` 遇到第一个错误（`Io(NotFound)` / `HashMismatch`）即返回，且只知道“第一个坏文件” | 共享函数一次返回**每个**文件 `missing/present/corrupt`；`validate_files` 仍是“首个非 `Present` 即 Err” | 前端需要整张状态表（§5.4），单文件提前返回无法支撑 |
| 缺失文件错误 | `sha256_file` 的 `Io(NotFound)`（不写路径语义，只有 OS 错误串） | `RapidOcrError::FileNotFound(<root>/<name>)` | 可定位错误，且不改变“首个错误返回”的调用语义 |
| 路径规则 | 只拒绝绝对路径与 `..`（允许 `sub/x.onnx`） | 拒绝绝对路径、分隔符、`..`、盘符前缀 | 模型目录是**扁平**的（`ensure_downloaded` 只按 URL 末段落盘），带分隔符的名字在磁盘上不可能对应下载产物；这与 §5.1「禁止路径分隔符与 `..`」一致，比旧实现更严 |
| 越界名字（结构体字面量绕过构造器） | 直接被 `validate_files` 用来 `root.join` 并读盘（可能读到目录外文件） | 不读盘，报 `Corrupt{ actual: "invalid file name ... escapes the model directory" }`，集合不可能 `complete` | 不在“提示下载”与“看起来正常”之间二选一 |
| 无哈希文件 | `ModelManifest` 的每个字段都强制有哈希，但默认表里的字典**根本没有哈希**（下载时传 `None`，完全跳过校验） | 空哈希是显式状态：状态可为 `Present`（只证明“存在”），但集合永不 `complete` | §1.2／§5.2 的硬规则 |

> **设计取舍（记录在案）**：`ModelFileSpec` 的 `sha256`／`source_url` 在协议里是
> `String`（不是 `Option<String>`），因此“没有哈希／没有来源”由**空串**表达，并且只有
> `model_set` 模块定义其含义（`has_hash()` / `has_source_url()`）。这是被协议签名约束的
> 结果，已用文档与测试固定，调用方不得自行比较空串。

---

### 交付物 2：通用、带版本的 `ModelManifest` 与单一权威来源（§5.3）

**新增文件**：`src/model_source.rs`（758 行，含 11 个单元测试）。

```rust
pub const MANIFEST_FILE_NAME: &str = "manifest.json";
pub const SUPPORTED_MANIFEST_SCHEMA_VERSION: u32 = 1;

pub struct ManifestFile { pub name: String, pub role: ModelRole, pub sha256: String,
                          pub size_bytes: Option<u64>, pub source_url: Option<String> }
pub struct ModelManifest { pub schema_version: u32, pub id: String, pub family: String,
                           pub version: String, pub languages: Vec<String>,
                           pub files: Vec<ManifestFile> }
impl ModelManifest {
    pub fn from_json_str(json: &str) -> Result<Self>;
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self>;
    pub fn load(path: &Path) -> Result<Self>;
    pub fn load_from_dir(dir: &Path) -> Result<Option<Self>>;
    pub fn to_model_set(&self) -> Result<ModelSet>;
    /// 薄包装：`validate_model_files` 的唯一实现，遇到第一个非 `Present` 即 Err。
    pub fn validate_files(&self, root: impl AsRef<Path>) -> Result<()>;
}

pub enum ModelSourceKind { DefaultTable, LocalManifest }   // "default_table" | "local_manifest"
pub struct ModelRequest { pub text: Option<DefaultModelSelection>, pub formula: bool }
impl ModelRequest {
    pub fn text_only(text: DefaultModelSelection) -> Self;
    pub fn text_and_formula(text: DefaultModelSelection) -> Self;
    pub fn formula_only() -> Self;
    pub fn text_roles(&self) -> Vec<ModelRole>;
    pub fn formula_roles(&self) -> Vec<ModelRole>;
    pub fn required_roles(&self) -> Vec<ModelRole>;
}
pub struct ModelSource { /* manifest: Option<ModelManifest> */ }
impl ModelSource {
    pub fn select(model_dir: &Path) -> Result<Self>;
    pub const fn kind(&self) -> ModelSourceKind;
    pub fn manifest(&self) -> Option<&ModelManifest>;
    pub fn model_sets(&self, request: &ModelRequest) -> Result<Vec<ModelSet>>;
}
```

**默认表 → `ModelSet`（`src/model_registry.rs`）**

```rust
pub struct DefaultModelSelection { pub ocr_version: OcrVersion, pub det_lang: LangDet,
    pub rec_lang: LangRec, pub cls_lang: LangCls, pub model_type: ModelType,
    pub include_classifier: bool }
impl ModelRegistry {
    pub fn text_model_set(&self, selection: &DefaultModelSelection) -> Result<ModelSet>;
    pub fn formula_model_set(&self) -> Result<ModelSet>;
}
```

- 文本集合：`detector`（+ 需要时 `classifier`）+ `recognizer` + `dictionary`；文件名由
  **URL 末段**推导（唯一实现 `model_store::extract_file_name`，已提为 `pub(crate)`），
  因为默认表的键不是文件名（键 `multi_PP-OCRv6_det_small` ↔ 文件
  `PP-OCRv6_det_small.onnx`），而下载器落盘用的就是 URL 末段——两处必须一致，否则状态
  校验会指向不存在的路径。
- `include_classifier` 必须显式：`GlobalConfig::use_cls` 默认 `false`，无条件把 cls 算进
  集合会把“没下载 cls”变成“模型不齐备”，挡住本来能跑的管线。
- 公式集合：`assets/default_models.yaml` 新增 `formula:` 段（`id`/`family`/`version`/
  `files[]`，每个文件显式写 `name` + `role`）。**为什么放在同一张表而不是另开一个资产
  文件**：§5.3 第 3 条要求 `default_models.yaml` 继续作为“可下载来源表（URL + SHA256）”
  的唯一载体，另开一张表就等于又造出一个默认权威；公式模型与 `ocr_version`／语言选择无关，
  因此在同一文件里用**独立的顶层段**（固定集合）而不是塞进 `onnxruntime` 选择树。
- `formula:` 段**只登记管线必需的文件**：`pp_formulanet_plus_m.onnx`
  （`role: formula_recognizer`，SHA-256 `71b6d389…d9493b`，体积 593,915,961）。
  页面公式检测模型 `pix2text-mfd-1.5.onnx` 在 `FormulaPolicy` 里是**可选**的，且没有可信
  的公开下载来源（其许可条款本身存在 MIT 与 AGPL-3.0 的冲突，见 `THIRD_PARTY_NOTES.md`），
  因此它不是集合成员——否则“可选能力”会变成“必须先拿到 80 MB 才能启用公式”。

**单一来源规则实测**（`ModelSource::select` + `model_sets`）：

| 场景 | 结果 | 测试 |
| --- | --- | --- |
| 目录内无 `manifest.json` | `kind() == DefaultTable`，只查默认表 | `the_selected_source_is_the_local_manifest_when_present` |
| 目录内有 `manifest.json` | `kind() == LocalManifest`，默认表完全不参与 | 同上 + `a_local_manifest_is_never_merged_with_the_default_table` |
| 清单只有公式模型，请求文本管线 | 报错并列出**全部**缺失 role（`detector, recognizer, dictionary`），**不从默认表补** | `a_local_manifest_is_never_merged_with_the_default_table` |
| 清单 JSON 损坏 | 报错（含 `manifest.json` 路径），**不回落默认表** | `a_broken_manifest_is_an_error_not_a_fallback_to_the_default_table` |
| 旧四字段清单（无 `schema_version`） | 可定位错误 + 迁移提示（点明 `files` 与 `schema_version`） | `legacy_four_field_manifest_is_rejected_with_a_migration_hint` |
| `schema_version: 2` | `unsupported schema_version 2; this build supports 1` | `an_unknown_schema_version_is_rejected` |
| `schema_version` 缺失且非旧形状 / 非整数 | 分别报“缺 schema_version”与“必须是 unsigned integer” | 同上 |
| 清单含未知字段（如 `classifier`、拼错的 `file`） | `deny_unknown_fields` → `unknown field` 可定位错误 | `unknown_manifest_fields_are_rejected` |
| 清单文件哈希为空 / 名字越界 | 分别报 `empty sha256` 与 `not a bare relative file name` | `a_manifest_file_must_carry_a_hash_and_a_bare_name` |
| 仓库内示例清单 | 能被当前加载器解析并转换（示例＝契约的一部分） | `the_shipped_manifest_example_matches_the_current_schema` |

**真实文件端到端校验**（需要 `RAPID_OCR_MODEL_ROOT`）：
`the_default_table_set_is_complete_against_real_v6_medium_models` 用默认表构造
`PP-OCRv6-medium-ch` 文本集合，对真实目录 `OCR-Model/medium` 逐文件算哈希：

```text
test model_source::tests::the_default_table_set_is_complete_against_real_v6_medium_models ... ok
（断言 status.files 全为 Present、complete == true、download_bytes_total == Some(0)）
```

**修改前后行为对比（交付物 2）**

| 项目 | 修改前 | 修改后 | 预期 |
| --- | --- | --- | --- |
| 清单结构 | `{id, family, version, languages, detector, recognizer, dictionary, classifier?}`，无法表达 tokenizer／公式模型 | `{schema_version, …, files: [{name, role, sha256, size_bytes?, source_url?}]}` | 覆盖全部 `ModelRole`（§5.3） |
| 版本 | 无 | `schema_version`（缺失/未知 → 可定位错误） | 旧格式必须报错并给迁移提示 |
| 来源 | 两份权威并存（清单与默认表语义重叠，且**没有任何代码读清单**） | `manifest.json` 存在 → 唯一来源；否则默认表 | 无合并、无“可选覆盖” |
| 缺 role | 无概念（固定字段决定一切） | `ModelSet::require_roles` 报错并列出缺失 role | 不静默降级 |
| 默认表 → 清单 | `ModelRegistry::resolve_*` 只给单个 URL（无 role、无集合） | `text_model_set` / `formula_model_set` → `ModelSet`（含公式 role） | HTTP 层永不解析 YAML |
| 公开 API | `ModelArtifact`、四字段 `ModelManifest`、未被使用的 `api::ModelSource` | 删除 `ModelArtifact` 与 `api::ModelSource`；`ModelManifest` 换新结构（导出路径仍是 `rapid_ocr_rs::ModelManifest`） | 开发期允许破坏性修改；删除死代码，避免与新 `ModelSource` 同名冲突 |

> **注意（破坏性修改的外部影响）**：`src-tauri/src/ocr/rapid.rs` 直接
> `serde_json::from_slice::<rapid_ocr_rs::ModelManifest>` 并调用 `validate_files`。
> 它只用这两个符号（签名未变，因此**仍可编译**），但**运行期**要求磁盘上的清单是
> `schema_version: 1` 形状。该调用点指向 `app_local_data_dir()/models/ocr/ppocrv6-medium`
> （不是 `OCR-Model/`），本阶段没有那个目录的清单文件，因此无需改动调用方；
> 若将来部署目录里放旧格式清单，会得到可定位的“旧格式”错误而不是静默通过。

---

### 交付物 3：字典 SHA-256（§1.2 / §5.2）

**资产 schema 变化**（`assets/default_models.yaml`，每个 rec 条目）：

```yaml
      ch_PP-OCRv4_rec_infer.onnx:
        model_dir: https://…/onnx/PP-OCRv4/rec/ch_PP-OCRv4_rec_infer.onnx
        SHA256: 48fc40f2…
        dict:                                   # 旧：dict_url: https://…（没有哈希）
          model_dir: https://…/ppocr_keys_v1.txt
          SHA256: 28b2362ad4ab2dc38769aa72feb535e3a9ddb3fd2a7585a05920e6393b1dc7f7
          size_bytes: 26249
```

**使用过的命令**（对 30 个 `dict_url` 指向的 URL 逐个下载并取哈希；`num_redirects` 全部为
`0`，即这些 URL **不依赖重定向**，与 §6.1「禁止自动重定向」不冲突）：

```powershell
# 1) 从资产表里抽出所有 dict_url（30 条）
$yaml = Get-Content assets\default_models.yaml   # 旧版本
# 逐行状态机解析 onnxruntime → <version> → det/cls/rec → <模型键> → dict_url
# 2) 每个 URL 下载到 target\m0a-dicts\<文件名>，并记录 http_code / size / num_redirects
curl.exe -sS -L --max-time 90 -o $dest -w "%{http_code} %{size_download} %{num_redirects} %{url_effective}" $url
# 3) 计算 SHA-256 与体积，导出 CSV（target\m0a-dicts\hashes.csv）
Get-FileHash -Algorithm SHA256 -Path $dest      # → target\m0a-dicts\hashes.csv
# 4) 用 CSV 重写资产表：dict_url → dict{model_dir, SHA256, size_bytes}
```

**结果：30/30 个字典条目全部拿到实测哈希，剩余未哈希条目 = 0**（27 个不同文件内容，
其中 `ppocr_keys_v1.txt` / `ppocrv5_dict.txt` / `ppocrv6_dict.txt` 各由 2 个 URL 提供，
两两字节相同、哈希相同）。

| 字典文件 | 体积 | SHA-256 | 覆盖的 rec 条目 |
| --- | ---: | --- | ---: |
| `arabic_dict.txt` | 405 | `637c27c88512c22089bef927b34ada08f748dc132ac70facd68d8202384c2726` | 1 |
| `chinese_cht_dict.txt` | 33443 | `832551fee1f2fbc97508772d81ebdc8dba12c00de97a35c71c9ddf43ddac1a83` | 1 |
| `cyrillic_dict.txt` | 410 | `369a82c6c8c479784a5d726448b83b1eafb5fef0a4129a5eaa3929625ddcd132` | 1 |
| `devanagari_dict.txt` | 508 | `b5f1be6d8bbff1a19fb96c5d4ca96a423380234bb7d2ce0e07b5838adb4d18ea` | 1 |
| `en_dict.txt` | 190 | `5662df9d2d03f0e8ca0d3b0649d6acbab904b6a14b3d3521463c71c37c668ce3` | 1 |
| `japan_dict.txt` | 17332 | `1dcfcb41eec90576a945b3084f22ade11ced506e24f14879245b071698f308e8` | 1 |
| `ka_dict.txt` | 452 | `fad3f765943275eec2134a6fbc9b52785426641104d4e7db3a159dc73f341dde` | 1 |
| `korean_dict.txt` | 14480 | `aa1fdc8ae8f7cd40a0ec4edb472eb0421e11427e6ccfee9915440742c18b0a20` | 1 |
| `latin_dict.txt` | 468 | `8e6d4e3629788c35c31f7e530287d6147b549bb7a265bd6708bb281134429e2c` | 1 |
| `ppocr_keys_v1.txt` | 26249 | `28b2362ad4ab2dc38769aa72feb535e3a9ddb3fd2a7585a05920e6393b1dc7f7` | 2 |
| `ppocrv4_doc_dict.txt` | 62345 | `cf98472458ea87e020c0da18475a04bbeec2952b2b4e6c35c559b624182e3669` | 1 |
| `ppocrv5_arabic_dict.txt` | 2369 | `7f92f7dbb9b75a4787a83bfb4f6d14a8ab515525130c9d40a9036f61cf6999e9` | 1 |
| `ppocrv5_cyrillic_dict.txt` | 2781 | `db40aa52ceb112055be80c694afdf655d5d2c4f7873704524cc16a447ca913ba` | 1 |
| `ppocrv5_devanagari_dict.txt` | 1943 | `09c7440bfc5477e5c41052304b6b185aff8c4a5e8b2b4c23c1c706f6fe1ee9fc` | 1 |
| `ppocrv5_dict.txt` | 74012 | `d1979e9f794c464c0d2e0b70a7fe14dd978e9dc644c0e71f14158cdf8342af1b` | 2 |
| `ppocrv5_el_dict.txt` | 1103 | `31defc62c0c3ad3674a82da6192226a2ba98ef4ff014a7045cb88d59f9c3de31` | 1 |
| `ppocrv5_en_dict.txt` | 1416 | `e025a66d31f327ba0c232e03f407ae8d105e1e709e7ccb3f408aa778c24e70d6` | 1 |
| `ppocrv5_eslav_dict.txt` | 1663 | `3e95f1581557162870cacdba5af91a4c6be2890710d395b0c3c7578e7ee5e6eb` | 1 |
| `ppocrv5_korean_dict.txt` | 47451 | `a88071c68c01707489baa79ebe0405b7beb5cca229f4fc94cc3ef992328802d7` | 1 |
| `ppocrv5_latin_dict.txt` | 1634 | `3c0a8a79b612653c25f765271714f71281e4e955962c153e272b7b8c1d2b13ff` | 1 |
| `ppocrv5_ta_dict.txt` | 1723 | `85b541352ae18dc6ba6d47152d8bf8adff6b0266e605d2eef2990c1bf466117b` | 1 |
| `ppocrv5_te_dict.txt` | 1831 | `42f83f5d3fdb50778e4fa5b66c58d99a59ab7792151c5e74f34b8ffd7b61c9d6` | 1 |
| `ppocrv5_th_dict.txt` | 1767 | `57f5406f94bb6688fb7077f7be65f08bbd71cecf48c01ea26c522cb5c4836b7a` | 1 |
| `ppocrv6_dict.txt` | 74947 | `b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d` | 2 |
| `ppocrv6_tiny_dict.txt` | 27156 | `c5cbe34ef40c29c4df07ed012bf96569cb69a2d2a01a07027e9f13cb832bd9cd` | 1 |
| `ta_dict.txt` | 352 | `e93e694814afd9ff1e918b6f4bb4267fb4c65a9344ee5d7464f9135b90c0a270` | 1 |
| `te_dict.txt` | 429 | `ee9946a60d7701977474e89883694a4ca20eaa3868a23334bf7940dcd008f0f9` | 1 |

**独立交叉校验**（不是自己证明自己）：把下载结果与**本地已有文件**比对，
6/6 命中，其中 `ppocrv6_dict.txt` 的哈希与 `OCR-Model/manifest.json`（旧格式清单，
本阶段之前就存在的独立记录）**逐位相同**：

| 文件 | 体积 | 结果 |
| --- | ---: | --- |
| `OCR-Model/medium/ppocrv6_dict.txt` | 74947 | MATCH（＝`b5f2bfe2…`，与旧清单一致） |
| `OCR-Model/small/ppocrv6_dict.txt` | 74947 | MATCH |
| `OCR-Model/tiny/ppocrv6_tiny_dict.txt` | 27156 | MATCH（＝下载值 `c5cbe34e…`） |
| `OCR-Model/medium/PP-OCRv6_det_medium.onnx` | 62119454 | MATCH（表内既有哈希） |
| `OCR-Model/medium/PP-OCRv6_rec_medium.onnx` | 76629984 | MATCH（表内既有哈希） |
| `OCR-Model/Formula-Recognition-Models/onnx/pp_formulanet_plus_m.onnx` | 593915961 | MATCH（`71b6d389…d9493b`） |

**同时实测并写入的体积**：v6 的 6 个权重（tiny/small/medium × det/rec，哈希均已与本地文件
比对 MATCH）与 30 个字典、1 个公式权重。v4/v5 的**权重**体积未测（没有下载几十个
20 MB～80 MB 的权重），因此这些集合的 `download_bytes_total` 是 `None`——这是**如实报告**，
不是“已知部分之和”；`docs/05` §6.5 已规定未知项按 `max_bytes` 计入预算。

**缺口修正的连带改动**：`src/ocr/rec/recognizer.rs::resolve_character_path` 以前
`ensure_downloaded(dict_url, None, …)`——字典下载**完全不校验**。现在传
`dictionary.sha256.as_deref()`，字典与权重一样校验；这是 §1.2「哈希可选 → 模型/字典文件
不允许 `None`」在本阶段能落地的最小根因改动（把 `ensure_downloaded` 的签名改成必填
哈希属于 §6.4 的加固下载器，留给 M0b）。

---

### 验证命令与结果（`docs/05` §12 要求）

在本 crate（`crates/rapid-ocr-rs`）执行，完整日志：`target/m0a-verify.log`。

| # | 命令 | 结果 | 退出码 |
| --- | --- | --- | --- |
| 1 | `cargo fmt --all` | 无输出（已格式化） | 0 |
| 2 | `cargo fmt --all -- --check` | 无输出 | 0 |
| 3 | `cargo clippy --all-targets -- -D warnings` | `Finished dev profile`，无 warning | 0 |
| 4 | `cargo test --all-targets` | 334 + 2 + 4 + 14 + 0 = **354 passed, 0 failed** | 0 |
| 5 | `cargo build --release --bins` | `Finished release profile [optimized] target(s) in 22.12s`；产出 `rapidocr.exe` 33,400,832 B、`bench_warm_e2e.exe` 33,250,304 B、`formula_eval.exe` 28,418,560 B、`formula_bench.exe` 27,724,288 B | 0 |
| 6 | `cargo test --lib formula_integration_tests -- --test-threads=1`（`RAPID_OCR_MODEL_ROOT` / `RAPID_OCR_FORMULA_TEST_ROOT` 已设置） | **11 passed, 0 failed**（79.65 s；日志里 `skipping test` 出现 0 次 → 真的加载了真实模型与真实页面，不是被 skip 掉的“绿”） | 0 |
| 7 | `cargo test --lib -- model_source:: model_registry:: model_set:: --nocapture`（真实资产，`RAPID_OCR_MODEL_ROOT` 已设置） | **34 passed, 0 failed** | 0 |

**基线对比（AGENTS.md §6：修改前建立基线 → 实施 → 验证）**

| 项目 | 修改前（`target/m0a-baseline-test.log`） | 修改后（`target/m0a-after-test.log` / `m0a-verify.log`） |
| --- | --- | --- |
| `cargo test --all-targets` | 308 + 2 + 4 + 14 + 0 = **328 passed, 0 failed** | 334 + 2 + 4 + 14 + 0 = **354 passed, 0 failed** |
| 新增测试 | — | **+26**（`model_set` 10、`model_source` 11、`model_registry` 5） |
| 修改的既有测试 | — | 1 个：`model_registry::tests::resolve_ppocr_v6_size_models`（断言从 `rec.dict_url` 改为 `rec.dictionary`，**断言强度不变**：仍然断言“v6 rec 能解析出字典 URL”，并新增“哈希是 64 位十六进制、体积 74947”两条更强断言）。删除/跳过/弱化的测试：**0** |

### 12 图硬门槛

**本阶段没有重跑 12 图基准**：本阶段只改模型元数据、清单结构与字典**下载**校验，
推理链路（`ImageInput → OcrRequest → OcrOutput`）未改动，因此硬门槛
（mean CER `0.44765135645866394`、区域均值 `34.833333333333336`）在数值上不可能变化。
`src/ocr/rec/recognizer.rs` 唯一改动的分支是“未显式给出 `rec_keys_path` 时的下载路径”，
而 12 图基准与 `formula_integration_tests` 都通过配置显式给出 `model_path` /
`rec_keys_path`（`OCR-Model/test-config-small.yaml`、`formula_integration_tests::engine_config`），
不进入该分支。**结论：没有可测量的性能/质量影响；也未把“没跑基准”伪装成“基准通过”。**

---

### 关键行为对比（AGENTS.md §9）

| 项目 | 修改前 | 修改后 | 预期结果 |
| --- | --- | --- | --- |
| 模型清单（原有功能） | 四字段 `ModelManifest`，无 `schema_version`，无任何生产调用方 | `schema_version` + `files[]` + `role`；`ModelSource` 是唯一入口 | 符合 §5.3；旧格式给出可定位迁移错误 |
| 逐文件校验（修改功能） | 首个错误即返回（`Io(NotFound)`/`HashMismatch`） | 共享函数返回**每个**文件状态；`validate_files` 仍是首个非 `Present` 即 Err（缺失 → `FileNotFound`） | 满足 §5.2；“首个错误”语义不变，错误更可定位 |
| `ModelSet` / role / 状态表（新增功能） | 不存在 | 一次给出 `missing/present/corrupt` + `complete` + `download_bytes_total` | 满足 §5.1/§5.2/§5.4 的数据需求 |
| 单一来源（新增规则） | 两份权威并存（且清单无人读取） | `manifest.json` 存在 → 唯一来源；否则默认表；缺 role → 报错列出缺失 role | 无合并、无静默降级（§5.3） |
| 字典哈希（边界/缺口） | `dict_url` 无哈希；下载时传 `None`（不校验） | 30/30 字典有实测 SHA-256 + 体积；下载时校验哈希 | 修正 §1.2 记录的真实缺口 |
| 无哈希文件 | 字典属于这一类，但被当作“可用” | `Present` 但集合永不 `complete` | §5.2 硬规则 |
| 路径安全（边界） | 拒绝绝对路径与 `..` | 额外拒绝路径分隔符/盘符/`.`；越界名字不读盘 | 与 §5.1 一致，且更严（已在文中说明） |
| 性能表现 | — | 无新增热路径：`ModelSet` 只在模型管理/校验处构造；哈希计算只在显式校验时发生（与旧 `validate_files` 相同） | 无退化 |
| 依赖 | 无网络/UI 依赖进库 | 未新增任何依赖（`Cargo.toml` 未改） | 库边界不变（§2.1） |

---

### 与并发工作流的边界（证据时间戳）

本阶段的验证是在**同一个工作树**里跑的，而 M0b/M0c/M1 的工作流同时在改这个 crate。为了让上面的
数字可被正确归属，这里记录时间戳（本机时钟）：

| 时间 | 事件 |
| --- | --- |
| 21:07–21:09 | 本阶段写完全部 `src/` 改动（`model_set.rs` 21:07、`model_source.rs`/`model_registry.rs` 21:09） |
| 21:09–21:11:20 | 本阶段的 1–5 号命令（fmt / fmt --check / clippy / test / release build）全部通过，日志 `target/m0a-verify.log` |
| 21:11:37 起 | 另一个工作流开始提交 `src/bin/serve/mod.rs`、`src/bin/serve/cli.rs`…（M0c 骨架） |
| 21:11:50–21:12:08 | 同一个工作流修改了**共享文件**：`src/bin/rapidocr.rs`（`#[cfg(feature = "serve")] #[path = "serve/mod.rs"] mod serve;`）、`Cargo.toml`（空 feature `serve = []`）、`src/exports.rs`（导出 `format_provider_preference` / `resolve_execution_providers`）、`src/runtime/provider.rs` |
| 21:12:51 | 本阶段 6 号命令（公式集成测试）通过，日志 `target/m0a-formula-integration.log` |

因此**在 21:12 之后再跑** `cargo fmt --all -- --check` 会看到新的失败，但那与本阶段无关：

```text
$ cargo fmt --all -- --check        # 21:15 复检
Diff in …\src\bin\serve\admit.rs / cli.rs / error.rs / jobs.rs / limits.rs / queue.rs /
        security.rs / state.rs      ← M0c 工作流的未格式化新文件（rustfmt 会跟随
                                       `#[path = "serve/mod.rs"] mod serve;`，即使该 feature 默认关闭）
Diff in …\src\exports.rs:85         ← M0c 工作流新增的那条 `pub use crate::runtime::provider::{…}`
                                       （需要把 `ort_runtime_version,` 与 `resolve_execution_providers,`
                                       合并到同一行）；本阶段新增的两个 `pub use` 块没有 diff
```

**本阶段刻意不修这两处**：它们属于另一个工作流正在写的文件（`cargo fmt --all` 会直接改写
对方的在途代码），且 `cargo clippy --all-targets -- -D warnings`（21:15 复检 exit 0）与
`cargo test --all-targets`（21:15 复检 **354 passed, 0 failed**）在同一个树上仍然是绿的——
`serve` 不在 `default` 里，默认构建不编译这些文件。日志：`target/m0a-recheck-fmt.log`、
`target/m0a-recheck-clippy.log`、`target/m0a-recheck-test.log`。

另外：`cargo doc --no-deps` 有 3 条 `private_intra_doc_links` 警告，全部来自既有代码
（`src/api.rs:1095` 与 `src/runtime/session.rs:40` 指向私有模块），**新增模块产生 0 条警告**。

### 未覆盖风险

1. **未提交、未在干净 clone 上验证**：按要求不 commit。`crates/` 在本仓库被
   `.gitignore` 忽略（它是独立仓库），因此本阶段的证据来自当前工作树，而不是一次
   可复现的提交。
2. **v4/v5 权重的哈希未重新验证**：只验证了 v6 的 6 个权重与 1 个公式权重（本地文件
   比对 MATCH）。表里 v4/v5 的 `SHA256` 沿用原值，本阶段没有下载它们做独立核对。
3. **字典哈希依赖下载当时的上游内容**：ModelScope 上 `resolve/<revision>/...` 的
   revision 固定（`v3.6.0` / `v3.9.1` / `v1.0.0`），但若上游替换同名文件，下载校验会
   **失败**（这正是期望行为）；本阶段没有断言上游内容长期不变。
4. **`download_bytes_total` 对 v4/v5 是 `None`**：未知体积按 §6.5 应由下载层按
   `max_bytes` 计入预算，该逻辑属于 M0b；本阶段只保证“未知即 `None`”，不猜数字。
5. **并发工作流改到了共享文件**：另一个工作流在 21:11:50–21:12:08 修改了
   `src/bin/rapidocr.rs`（接入 `#[path = "serve/mod.rs"] mod serve;`）、`Cargo.toml`
   （空 feature `serve = []`）、`src/exports.rs` 与 `src/runtime/provider.rs`，并持续新增
   `src/bin/serve/*.rs`。本阶段的 1–5 号命令（21:09–21:11:20）跑在**未包含**这些改动之前的
   树上；6–7 号命令（21:12）跑在包含它们的树上，仍全绿。21:15 的复检显示
   `clippy`/`test` 仍绿，而 `cargo fmt --all -- --check` 失败——失败**全部**落在对方的
   `src/bin/serve/*.rs` 与 `src/exports.rs:85`（对方新增的 `pub use` 行），本阶段新增的代码与
   `pub use` 块没有任何 fmt diff（详见「与并发工作流的边界」）。复现本阶段结论时需要
   本阶段那一刻的 `src/` 状态（提交号无法给出，见风险 1）。
6. **`OCR-Model/manifest.json` 仍是旧四字段形状**（未跟踪的本地文件）。它在本阶段
   **之前**就已经与磁盘布局不一致（声明的是 `OCR-Model/manifest.json` 同级的扁平
   `PP-OCRv6_det_medium.onnx`，而真实文件在 `OCR-Model/medium/`），因此旧代码
   `validate_files(OCR-Model)` 也是失败的（`Io(NotFound)`）；本阶段没有改动它，
   行为没有回归，新代码给出的错误反而更明确（“旧格式清单”）。**未做**：没有迁移或删除
   这个文件（它不在 crate 内、也不被任何当前调用点使用；迁移它仍会因为目录布局而校验失败）。
7. **`src-tauri` 未构建**：只确认它使用的两个符号（`ModelManifest`、`validate_files`）
   签名未变、且没有使用被删除的类型（`ModelArtifact`、`api::ModelSource`）。`src-tauri`
   当前处于另一条重构线上（工作树里有大量未完成的增删），构建它不属于本阶段范围。
8. **`ModelFileState` 的 JSON 形状**：`Corrupt { expected, actual }` 序列化为
   `{"corrupt": {...}}`，而 `docs/05` §5.4 的示例是扁平 `"state": "missing"`。
   `ModelFileState::as_str()` 已提供稳定标签，最终扁平化由 M0b 的响应层决定；本阶段
   不预先固定 HTTP 形状（避免把 HTTP 语义渗进库）。

---

### 与 `docs/05` §11「M0」验收清单的对照（仅本阶段范围内的条目）

| §11 M0 条目 | 本阶段 | 证据 |
| --- | --- | --- |
| `ModelSet`/`ModelFileSpec`/`ModelRole`/`ModelSetStatus` + **共享逐文件校验函数**（§5.1/5.2） | ✅ 完成 | 交付物 1；10 个单元测试；`validate_model_files` 是唯一实现 |
| `ModelManifest` 通用化（`schema_version` + `files: Vec<ManifestFile>`）+ **单一来源选择规则**（§5.3） | ✅ 完成 | 交付物 2；11 个单元测试（含旧四字段 / 未知版本 / 不合并 / 坏清单不回落） |
| 字典补 SHA-256；无哈希不得 `complete`（§1.2、§5.2） | ✅ 完成 | 交付物 3：30/30 条目有实测哈希；`a_set_with_an_unhashed_file_can_never_be_complete` |
| **M0 验收**：以上每项都有单元测试；`cargo test` 全绿；文档与实现一致 | ✅ 本阶段范围内成立 | 354 passed / 0 failed；本文件 + `README.md`「Model integrity」+ `THIRD_PARTY_NOTES.md` 已同步 |
| 删除 `--host`；监听地址硬编码（§7.1） | ⛔ 不在 M0a（属 M0b/M0c） | — |
| tombstone 表 + 404/410（§4.5） | ⛔ 不在 M0a | — |
| 加固下载器 + `--max-download-mb` + `MoveFileExW` + 迁移 CLI 调用方（§6） | ⛔ 不在 M0a | 字典侧只做了“传真实哈希”这一步（见交付物 3） |
| provider 回退语义 + 启动期配置校验（§7.5） | ⛔ 不在 M0a | 并发工作流正在做（`src/runtime/provider.rs` 导出 `format_provider_preference`） |
| `ServiceState`/`EngineState`、`ServeError`、准入顺序、双队列公平调度 | ⛔ 不在 M0a | 并发工作流正在做（`src/bin/serve/*`） |

### 本阶段**不做**的事（范围边界，全部留给 M0b/M0c/M1）

- 加固下载器（§6）：`DownloadRequest` / `download_verified`、禁用自动重定向（M2b 起改为
  **手工逐跳校验**，见文末 M2b）、host 白名单、
  `Content-Length` 预检、`take(max+1)`、唯一 `.part` 名、`MoveFileExW` 原子替换、单飞、
  磁盘空间预检、错误分类；`ensure_downloaded` 的 `Option<&str>` 签名仍然存在
  （§6.4 的“删除可传 `None` 哈希的入口”留给 M0b）。
- `serve` 子命令、`tiny_http`、`serve` feature、CLI 参数（`--model-dir`、
  `--max-download-mb`、`--allow-download`…）、`ServiceState`/`EngineState`、
  `ServeError`、CSP/静态页、双队列与公平调度。
- 未编辑 `docs/03-windows-only-optimization-tasks.md`（按要求）。
- 未提交任何 commit。

---

## M0c：`serve` 核心纯逻辑 + `serve` feature 骨架（无 HTTP 服务器）

**阶段**：M0c —— `docs/05` §11「M0」里 **HTTP 层之前**的条目：`ServiceState`/`EngineState`
状态机（§7.6）、`ServeError` 与状态码/`code` 映射（§11.1）、准入顺序与上限算术
（§4.4/§4.6/§6.2）、双队列**双向**公平调度（§8.3）、有界任务存储 + TTL + tombstone
（§4.5）、仅本机安全助手（§7.1–§7.3、§9）、`serve` 的 CLI 选项面（§3），以及
**空 `serve` feature** 的骨架（`serve = []`，不在 `default` 里）。
**不做**（属 M1）：HTTP 服务器 / `tiny_http` / 路由表 / `GET /` / 前端 / 静态页。
**不做**（属 M0b）：加固下载器本体。
**日期**：2026-10-03
**提交**：`（未提交：按要求不 commit）`

**新增文件**（全部在二进制侧，库零改动）：

```text
src/bin/serve/mod.rs        132 行  1 test   范围文档 + 脚手架范围断言
src/bin/serve/limits.rs     453 行  6 tests  MiB→字节换算与全部取值校验（唯一实现）
src/bin/serve/error.rs      884 行  7 tests  ServeError / DownloadError / 错误体
src/bin/serve/state.rs      830 行 21 tests  ServiceState / EngineState / 启动期配置校验
src/bin/serve/queue.rs      736 行 13 tests  双队列容量 + 双向公平调度
src/bin/serve/jobs.rs       865 行 20 tests  有界存储 / TTL / tombstone / 404-410
src/bin/serve/security.rs   550 行 13 tests  loopback / Host / Origin / token / 注入契约
src/bin/serve/admit.rs      705 行 15 tests  准入顺序 / 有界读取 / 媒体类型
src/bin/serve/cli.rs        490 行  9 tests  serve 选项面与默认值（无行为）
                         ────────────────
                         5645 行 105 tests
```

**共享文件的改动（4 处，均为最小必要面）**：

| 文件 | 改动 | 理由 |
| --- | --- | --- |
| `Cargo.toml` | 新增空 feature `serve = []`（**不进 `default`**） | §2.1 的 feature 隔离；M1 才加 `tiny_http`（必须 optional） |
| `src/bin/rapidocr.rs` | `#[cfg(feature = "serve")] #[path = "serve/mod.rs"] mod serve;` | 设计 §2.1：HTTP 不进库，serve 代码挂在二进制上 |
| `src/exports.rs` | 导出 `resolve_execution_providers` 与 `format_provider_preference` | §7.5/§7.6 要求启动期复用库里的**既有**判定与措辞，而不是另写一套 |
| `src/runtime/provider.rs` | `format_provider_preference`：`fn` → `pub fn`（签名未变） | 同上；它是 provider 展示文本的唯一实现 |

未触碰 `src/model_set.rs` / `src/model_source.rs` / `src/model_registry.rs` /
`src/model_store.rs` / `src/api.rs` / `assets/default_models.yaml`（M0a 在途文件）。

---

### 交付物 1：`ServiceState` / `EngineState` 状态机（§7.6）

**文件**：`src/bin/serve/state.rs`。**状态类型可序列化**（`#[serde(tag = "state")]`，
直接供 `/api/status` 使用）。

```rust
pub enum ServiceState { Starting, Ready }                       // 监听成功即为 Ready
pub enum EngineState {
    BlockedModelsMissing { missing: Vec<String> },
    Loading,
    Ready { requested: String, selected_ep: String, fallback_to_cpu: bool },
    Failed { reason: String },
    Rebuilding,                                                 // M3
}
impl ServiceState { pub fn listening_succeeded(self) -> Result<Self, TransitionError> }
pub struct EngineStateMachine { /* private state */ }
impl EngineStateMachine {
    pub fn start(readiness: ModelReadiness) -> Self;             // 启动期（§7.6 第 3 步）
    pub fn state(&self) -> &EngineState;
    pub fn begin_loading(&mut self) -> Result<(), TransitionError>;
    pub fn load_succeeded(&mut self, requested: impl Into<String>,
                          selected_ep: impl Into<String>, fallback_to_cpu: bool) -> Result<(), TransitionError>;
    pub fn load_failed(&mut self, reason: impl Into<String>) -> Result<(), TransitionError>;
    pub fn models_still_missing(&mut self, missing: Vec<String>) -> Result<(), TransitionError>;
    pub fn begin_rebuild(&mut self) -> Result<(), TransitionError>;        // Ready -> Rebuilding（M3）
}
impl EngineState {
    pub fn provider_status(&self, requested: &str) -> ProviderStatus;      // 三字段，未知为 null
    pub fn ocr_admission(&self) -> OcrAdmission;                          // Run/Queue/ModelsMissing/Unavailable
}
```

**合法转换（唯一表，逐条有测试）**：

| from | to | 触发 | 测试 |
| --- | --- | --- | --- |
| — | `BlockedModelsMissing` | 启动时模型不齐备 | `complete_models_start_in_loading_and_incomplete_models_start_blocked` |
| — | `Loading` | 启动时模型齐备（预加载） | 同上 |
| `BlockedModelsMissing` | `BlockedModelsMissing` | 重载时仍缺（刷新缺失清单） | `blocked_models_missing_can_be_refreshed_or_enter_loading` |
| `BlockedModelsMissing` | `Loading` | 模型齐备（reload / 惰性创建） | 同上 |
| `Failed` | `Loading` | `POST /api/engine/reload` | `failed_can_only_be_left_through_loading` |
| `Ready` | `Loading` | `POST /api/engine/reload` | `ready_reload_goes_ready_loading_ready` |
| `Loading` | `Ready` | 会话创建成功 | `loading_transitions_to_ready_with_the_three_provider_fields` |
| `Loading` | `Failed` | 会话创建失败（含 provider 不可用） | `loading_transitions_to_failed_and_records_the_reason` |
| `Ready` | `Rebuilding` | M3 运行期切换 provider | `rebuilding_is_reachable_from_ready_and_back_to_ready_or_failed` |
| `Rebuilding` | `Ready` / `Failed` | M3 切换成功 / 失败且无法恢复 | 同上 |
| 其他任意组合 | — | **拒绝** | `illegal_transitions_report_from_and_to_and_legal_predecessors`、`rebuilding_cannot_be_entered_from_loading_or_blocked`、`service_state_starts_then_becomes_ready_once` |

非法转换返回可定位的 `TransitionError`，例如：

```text
illegal engine state transition blocked_models_missing -> ready:
  ready may only be entered from [loading, rebuilding]
```

**OCR 准入（§7.6）**：`Ready → Run`；`Loading`/`Rebuilding → Queue`（**不失败**）；
`BlockedModelsMissing → 409 models_missing`；`Failed → 503 engine_unavailable` 且
`detail.reason` 必存在。测试：`ocr_admission_queues_while_loading_and_rebuilding_and_never_fails`、
`ocr_admission_reports_missing_models_and_engine_failure`、`ready_admission_is_run`。

**`/api/status` 三字段不得伪装未知态（§7.5/§9）**：`ProviderStatus` 始终序列化出
`requested`/`selected_ep`/`fallback_to_cpu` 三个键；未 `Ready` 时后两者是 `null`，
**不是** `false`。测试 `provider_status_never_disguises_unknown_as_false` 直接断言 JSON 里
`fallback_to_cpu` 为 `null`。

**启动期配置校验（§7.5/§7.6 第 2 步）**：

```rust
pub struct ServeStartup { pub limits: ServeLimits, pub plan: ServeConfigPlan }
impl ServeStartup {
    pub fn validate(raw_limits: RawServeLimits, engine: EngineConfig,
                    cli_provider: Option<ProviderPreference>, cli_max_side: Option<usize>,
                    allow_provider_fallback: bool) -> Result<Self, StartupConfigError>;
}
pub struct ServeConfigPlan { pub engine: EngineConfig, pub requested: ProviderPreference,
                             pub fail_if_provider_unavailable: bool }
```

- **provider 名称/feature 非法 ⇒ 启动即失败**，错误文本来自
  `rapid_ocr_rs::resolve_execution_providers`（"…is not compiled in; rebuild with
  `--features directml-provider`"），**不新写措辞**：
  `startup_rejects_a_provider_whose_feature_is_not_compiled_in`；
- **运行库不可用不在启动期判定**（§7.5 第 4 条）：探测固定用
  `fail_if_provider_unavailable = false`，因此 `is_available()` 报告的运行期事实
  留给 `Loading → Failed`，只有 `UnsupportedProvider`（名称/feature 级配置错误）才致命；
- **回退语义冻结**：`--provider directml|cuda` ⇒ `fail_if_provider_unavailable = true`；
  仅 `--allow-provider-fallback` 时为 `false`（`provider_fallback_policy_is_frozen_by_default`）；
- **优先级 CLI > YAML > 内建默认**：`cli_overrides_beat_yaml_and_yaml_beats_the_builtin_default`；
- `--max-side=0` 由库内既有 `EngineConfig::validate` 拒绝（措辞含 `max_side_len`，
  不另写一套）：`cli_max_side_zero_is_rejected_by_the_library_validation`。

**接缝（M0a/M1 必须补）**：状态机**不**解析模型清单，只接收
`ModelReadiness::{Complete, Incomplete{missing}}`。M1 的填充点是用 M0a 的
`validate_model_files` / `ModelSetStatus` 得到"缺哪些文件"，把 `Missing`/`Corrupt`
的**文件名**（与 `/api/models` 同字段）传进状态机。

---

### 交付物 2：`ServeError`、状态码映射与错误体（§11.1）

**文件**：`src/bin/serve/error.rs`。**禁止字符串匹配**：状态码与 `code` 只由类型匹配产生，
`RapidOcrError → (状态码, code, kind)` 只有一处（`classify_ocr_error`）。

```rust
pub enum ServeError {
    BadRequest, PayloadTooLarge, ResultTooLarge, ExportTooLarge, BadHost, BadOrigin,
    Unauthorized, RequestTimeout, Busy, JobNotFound, JobEvicted, JobNotFinished,
    NotCancellable, ModelsMissing, ModelsCorrupt, DownloadsDisabled, InsufficientDiskSpace,
    UnsupportedInput, EngineUnavailable { reason: String }, Download(DownloadError),
    Ocr(RapidOcrError), Internal,
}
impl ServeError {
    pub fn status_code(&self) -> u16;
    pub fn code(&self) -> &'static str;
    pub fn message(&self) -> String;
    pub fn detail(&self) -> serde_json::Value;
    pub fn body(&self) -> ErrorBody;          // { code, message, detail } 三键固定顺序
    pub fn render_body(&self) -> String;      // JSON 文本
    pub fn from_ocr_admission(OcrAdmission) -> Result<(), Self>;
}
pub enum DownloadError {  // §6.1 第 11 条的十类
    Scheme{scheme}, Redirect{location}, Host{host}, TooLarge{limit_bytes, observed_bytes},
    Network{detail}, ConnectTimeout{timeout_ms}, ReadTimeout{timeout_ms},
    InsufficientSpace{required_bytes, available_bytes}, HashMismatch{expected, actual}, Cancelled,
}
```

**两处相对 §11.1 变体清单的**新增**（都是文档别处明确要求的行为，已在模块文档逐条记录）**：

| 变体 | 状态码 / `code` | 为什么必须有 |
| --- | --- | --- |
| `ExportTooLarge` | 413 / `export_too_large` | §9.5 要求导出超 `--max-export-mb` 返回 413 `export_too_large`；§11.1 的清单里没有任何变体能产生这个 `code`（`ResultTooLarge` 的 code 是 `result_too_large`，两者是**不同预算**、不同原因） |
| `RequestTimeout` | 408 / `request_timeout` | §4.4 第 6 步要求"有界流式读取 + **读取超时**"；把它降级成 400 会掩盖真实原因 |

其余 20 个变体与 §11.1 逐项一致（`job_not_found`(404) 来自 §4.5，`engine_unavailable`(503)
来自 §7.6，`download_cancelled`(409) 见下表）。**全表**（每行都有断言）：

| 变体 | 状态码 | `code` | 出处 |
| --- | --- | --- | --- |
| `BadRequest` | 400 | `bad_request` | §4.4（`Content-Length` 与实际不符、媒体类型不符） |
| `Unauthorized` | 401 | `unauthorized` | §11.1 |
| `BadOrigin` | 403 | `bad_origin` | §7.2 |
| `DownloadsDisabled` | 403 | `downloads_disabled` | §11.1 |
| `RequestTimeout` | 408 | `request_timeout` | §4.4 第 6 步 |
| `JobNotFound` | 404 | `job_not_found` | §4.5 |
| `JobNotFinished` | 409 | `job_not_finished` | §4.3 |
| `NotCancellable` | 409 | `not_cancellable` | §4.3 |
| `ModelsMissing` / `ModelsCorrupt` | 409 | `models_missing` / `models_corrupt` | §11.1 |
| `JobEvicted` | 410 | `job_evicted` | §4.5 |
| `PayloadTooLarge` | 413 | `payload_too_large` | §4.4 |
| `ResultTooLarge` | 413 | `result_too_large` | §4.6 |
| `ExportTooLarge` | 413 | `export_too_large` | §9.5 |
| `BadHost` | 421 | `bad_host` | §7.2 |
| `UnsupportedInput` | 422 | `unsupported_input` | §11.1 |
| `Busy` / `EngineUnavailable` | 503 | `busy` / `engine_unavailable` | §4.5 / §7.6 |
| `InsufficientDiskSpace` | 507 | `insufficient_disk_space` | §11.1 |
| `Internal` | 500 | `internal` | §11.1 |

**下载错误的映射**（`ServeError::Download` 委托 `DownloadError`，`detail.kind` 给出精确原因）：

| `DownloadError` | 状态码 | `code` | `detail.kind` |
| --- | --- | --- | --- |
| `Scheme` / `Redirect` / `Host` / `Network` / `HashMismatch` | 502 | `download_failed` | `scheme` / `redirect` / `host` / `network` / `hash_mismatch` |
| `ConnectTimeout` / `ReadTimeout` | 504 | `download_timeout` | `connect_timeout` / `read_timeout` |
| `TooLarge` | 413 | `payload_too_large` | `too_large` |
| `InsufficientSpace` | 507 | `insufficient_disk_space` | `insufficient_space` |
| `Cancelled` | 409 | `download_cancelled` | `cancelled` |

> **设计取舍**：§11.1 只给下载规定了 `download_failed`(502) / `download_timeout`(504) /
> `insufficient_disk_space`(507) 三个 `code`。方案 A 是"其余八类都塞进 502 + 一个字符串
> 说明"，方案 B 是"给每类造一个新 `code`"。本阶段选**中间**：`code` 只用到文档已有的
> 三个（加 `TooLarge` 落 413、`Cancelled` 落 409），**具体原因进 `detail.kind`**，
> 因此客户端既能按 `code` 分支，也能按 `kind` 精确区分，且没有造出一堆文档外的 `code`。

`RapidOcrError` 的 14 个变体逐条映射（含 `UnsupportedProvider`/`UnsupportedBackend` →
503 `engine_unavailable` 且**必带** `reason`；`HashMismatch`/`Tokenizer` → 409
`models_corrupt`；`ModelResolve`/`FileNotFound` → 409 `models_missing`；
`InvalidImage`/`InvalidInput`/`Decode` → 422 `unsupported_input`；`Config`/`Io`/`Yaml` →
500 `internal`；`Download`/`Reqwest` → 502 `download_failed`）。
测试 `every_rapid_ocr_error_variant_is_mapped` 逐变体构造实例并断言三元组（`Reqwest`
用端口越界的 URL 构造，**不产生任何网络 I/O**）。**没有一处字符串匹配**。

**错误体形状**：`{"code": …, "message": …, "detail": …}` —— 三个键**始终**存在
（无附加信息时 `detail` 为 `null`），键顺序固定，测试
`error_body_always_has_code_message_and_detail_keys` 对每个变体解析 JSON 并断言键集合与顺序。

---

### 交付物 3：任务存储、TTL、字节预算与 tombstone（§4.3/§4.5）

**文件**：`src/bin/serve/jobs.rs`。

```rust
pub type Millis = u64;                       // 时间由调用方注入；存储内部不读时钟
pub enum JobKind { Ocr, ModelDownload }
pub enum JobState { Queued, Running, Succeeded, Failed, Cancelled }
pub struct JobView { id, kind, queue, state, position, queued_ms, started_ms, elapsed_ms, error }
pub struct TickReport { expired_jobs, evicted_for_count, evicted_for_bytes, expired_tombstones }
pub struct JobIdGenerator;                   // `job-0000000000000001`（单调，可复现）
pub impl JobStore {
    pub fn insert(&mut self, id, kind, class, original_bytes, now) -> Result<(), ServeError>;
    pub fn set_position(&mut self, id, &str, Option<usize>) -> Result<(), ServeError>;
    pub fn start/succeed/fail(...) -> Result<(), ServeError>;
    pub fn cancel(&mut self, id: &str, now: Millis) -> Result<QueueClass, ServeError>;
    pub fn record(&self, id) -> Result<&JobRecord, ServeError>;   // 404 / 410 / 记录
    pub fn view(&self, id, now) -> Result<JobView, ServeError>;
    pub fn tick(&mut self, now: Millis) -> TickReport;            // 显式时间驱动，无需 sleep
    pub fn retained_bytes(&self) -> u64; pub fn terminal_count(&self) -> usize;
    pub fn tombstone_len(&self) -> usize; pub fn evicted_at(&self, id) -> Option<Millis>;
}
```

| 规则（§4.5） | 实现 | 测试 |
| --- | --- | --- |
| `Queued → Running → Succeeded\|Failed\|Cancelled` | 逐转换守卫，非法转换 `Internal` | `queued_to_running_to_succeeded_carries_every_documented_field`、`illegal_store_transitions_are_internal_errors` |
| 队列内 position | `set_position`；`start` 时清空 | 同上 |
| `queued_ms` / `started_ms` / `elapsed_ms` | `elapsed = (finished \| now) - (started \| queued)` | 同上（含"结束时冻结"） |
| **取消语义（§4.3）** | `Queued → Cancelled` 成功并返回队列；`Running` 与终态 → 409 `not_cancellable`，**状态不变** | `cancelling_a_queued_job_is_reliable`、`cancelling_a_running_job_returns_not_cancellable_and_keeps_running`、`cancelling_a_finished_job_is_also_not_cancellable` |
| 数量上限 + "最旧终态优先" | `oldest_terminal()` 按 `(finished_ms, seq)` 取最小 | `eviction_by_count_removes_the_oldest_terminal_job_first` |
| 字节上限（原件 + 结果） | `retained_bytes` 账本，淘汰时扣减 | `eviction_by_bytes_uses_the_original_plus_result_budget`、`byte_accounting_tracks_originals_and_results_across_eviction` |
| 活跃任务**永不**淘汰 | 上限只作用于终态任务 | `active_jobs_are_never_evicted` |
| TTL 由 `tick(now)` 驱动 | 注入时间，零睡眠 | `ttl_expiry_is_driven_by_tick_with_injected_time`、`ttl_never_expires_a_queued_or_running_job` |
| tombstone 容量（FIFO） | 上限外淘汰最旧记录 | `tombstone_capacity_eviction_keeps_only_the_newest_entries` |
| tombstone TTL | 到期即移除 → 410 变回 404 | `tombstone_ttl_expiry_turns_410_back_into_404` |
| **404 vs 410** | `record()`：存储里没有 → 看 tombstone → 410，否则 404；对已淘汰任务做 `start`/`cancel` 同样是 410 | `unknown_jobs_are_404_while_evicted_jobs_are_410` |
| 两类任务同库 | `JobKind::{Ocr, ModelDownload}` + 各自队列 | `model_download_jobs_are_stored_with_their_own_kind_and_queue` |

**`id` 为什么不是随机令牌**：不可猜测性由 §7.2 的 token 提供（所有 `/api/*` 都要 token），
单调序号让日志、tombstone 与排序可复现；`JobIdGenerator` 因此只用进程内计数器。

---

### 交付物 4：双队列调度与**双向**公平性（§8.2/§8.3）

**文件**：`src/bin/serve/queue.rs`。

```rust
pub enum QueueClass { Text, Formula }
pub struct SchedulerConfig { max_queue_text, max_queue_formula,
                             max_consecutive_text, max_consecutive_formula }
impl SchedulerConfig {
    pub fn new(...) -> Result<Self, ServeConfigError>;   // 容量与配额都必须 ≥ 1
    pub fn from_limits(&ServeLimits) -> Self;
    pub fn round_len(&self) -> usize;
    pub fn capacity(&self, QueueClass) -> usize;
    pub fn consecutive_quota(&self, QueueClass) -> usize;
    pub fn wait_bound(&self, QueueClass) -> usize;       // = capacity × 对方配额（可证明上界）
}
pub struct DualQueueScheduler;
impl DualQueueScheduler {
    pub fn enqueue(&mut self, class, id) -> Result<usize, ServeError>;  // 满 → 立即 Busy(503)
    pub fn take_next(&mut self) -> Option<ScheduledJob>;
    pub fn remove(&mut self, class, id: &str) -> bool;                 // 取消排队中的任务
    pub fn position_of(&self, class, id) -> Option<usize>;
    pub fn queued_len(&self, class) -> usize;  pub fn is_empty(&self) -> bool;
}
```

**调度策略（§8.3 的五条）**：独立容量；每轮先取最多 `T=--max-consecutive-text` 个普通、
再取最多 `F=--max-consecutive-formula` 个公式；配额 ≥ 1 使"每个非空队列每轮至少服务一次"
成为**结构性质**；某队列为空时另一队列自由连续处理；配额只在"两者都满足"时清零。

**等待上界的口径与结论**：一个任务"等了多久"＝**从入队到被服务之间，另一类被服务的次数**
（与推理耗时无关，因此单测完全确定）。可证明上界 `capacity × 对方配额`：

| 场景（400 步定向洪水，确定性 id 序列） | 实测 `max_wait_text` | 实测 `max_wait_formula` | 上界（普通/公式） |
| --- | ---: | ---: | --- |
| 普通队列持续灌满 + 公式每步到 1 个（默认 4/1） | 3 | 1 | 4 / 8 |
| 公式队列持续灌满 + 普通每步到 1 个（默认 4/1） | 3 | 1 | 4 / 8 |
| 非默认配额 `2/3`，普通洪水 | 3 | 1 | 12 / 4 |
| 非默认配额 `2/3`，公式洪水 | 3 | 1 | 12 / 4 |

两个方向各 320/80（默认）与 80/120（2/3）个任务被服务，**没有被永久饿死的任务**。

**M0c 实测修正（重要）**：最初实现里有一条"空队列重新有任务就清零轮次计数"的规则，
本意是让刚到的任务不必等满一整个配额。洪水测试当场证伪：**持续到达的细流队列每步都会
重开一轮**，于是普通队列（取法里优先）永远轮不到公式队列——

```text
# 修正前（有"重新有任务即清零"规则）：
formula flood: FairnessRun { max_wait_text: 0, max_wait_formula: 0,
                             served_text: 400, served_formula: 0,
                             pending_text: 0, pending_formula: 2 }
formula never ran at all: ...
```

这正是 §8.3 要消灭的那种饿死（方向相反而已）。删掉该清零分支后两个方向同时有界；
不清零是安全的，因为 `served_text >= T` 与 `served_formula >= F` 不可能持续同时成立
（成立即刻清零），因此新到的普通任务最多等 `F` 个公式任务、新到的公式任务最多等 `T` 个普通任务。

#### 朴素策略失败演示（要求的证据）

按要求把 `peek_class` 临时换成朴素策略，跑测试，再还原。两次演示都保留了原始日志：

**(a) 朴素"先做普通，普通为空才做公式"（`target/m0c-naive-text-first.log`）**：5 个测试失败。

```text
---- serve::queue::tests::a_text_flood_does_not_starve_formula_jobs stdout ----
text flood: FairnessRun { max_wait_text: 3, max_wait_formula: 0, served_text: 400,
                          served_formula: 0, pending_text: 3, pending_formula: 2 }
panicked: formula work must actually run: ...

---- serve::queue::tests::a_single_formula_job_waits_at_most_the_text_quota stdout ----
panicked: the formula job must be served even while the text queue is never empty (§8.3)

test result: FAILED. 8 passed; 5 failed        # 5 个失败全部是公平性相关
```

即：400 步里公式任务被服务 **0** 次，公式队列一直积压 2 个 —— 无界等待。

**(b) 镜像的朴素"先做公式，公式为空才做普通"（`target/m0c-naive-formula-first.log`）**：
同样 5 个失败，这次是普通任务被饿死：

```text
formula flood: FairnessRun { max_wait_text: 0, max_wait_formula: 1, served_text: 0,
                             served_formula: 400, pending_text: 4, pending_formula: 1 }
panicked: text work must actually run: ...
```

**还原证据**：`queue.rs` 还原后 SHA-256 与演示前的备份**逐位相同**
（`3067820EAE35FA3C5DA8D3743A74F5422A1628365CAE9BF6CA13EAFDEED14C37`），
文件中 `TEMP-M0C-EVIDENCE` 出现 **0** 次，13 个 queue 测试全绿。

> **方法学注记（避免误读证据）**：`Copy-Item` 会把目标文件的 `LastWriteTime` 设成备份的
> 旧时间，cargo 因此**没有重编译**，还原后第一次跑测试仍显示朴素策略的失败。必须先
> `(Get-Item queue.rs).LastWriteTime = Get-Date` 再跑。上面的"还原后全绿"是 touch 之后的
> 结果；文件哈希与 `TEMP-` 计数是同一次检查里的独立证据。

---

### 交付物 5：准入顺序与上限算术（§4.4/§4.6）

**文件**：`src/bin/serve/admit.rs`（顺序）+ `src/bin/serve/limits.rs`（算术）。

```rust
pub enum HttpMethod { Get, Head, Post, Put, Delete, Other }
impl HttpMethod { pub fn parse(&str) -> Self; pub fn is_state_changing(self) -> bool }
pub enum RouteDecision { Matched, NotFound, MethodNotAllowed }
pub struct RequestDescriptor<'a> { method, path, route, has_token, host, origin,
                                   queue_full, content_length, body_so_far, content_type, is_chunked }
pub enum AdmissionError { Route { path, method, decision }, Rejected(ServeError) }
pub struct Admit { pub max_body: u64, pub expected_body: Option<u64> }
pub fn admit(&RequestDescriptor, &LocalOrigin, max_body: u64) -> Result<Admit, AdmissionError>;

pub struct BodyBudget;   // accept(chunk_len) → 413；finish(expected) → 长度不符 400
pub trait BodySource { fn read_timed_out(&self) -> bool; fn next_chunk(&mut self) -> ChunkOutcome; }
pub enum ChunkOutcome { Data(Vec<u8>), End, Failed(String), TimedOut }
pub fn read_body(&mut dyn BodySource, expected: Option<u64>, max_body: u64) -> Result<Vec<u8>, ServeError>;
pub fn check_content_type(Option<&str>) -> Result<(), ServeError>;   // §4.4 第 7 步
```

**顺序逐条被断言**（`the_admission_order_is_enforced_step_by_step`）：一次构造同时违反
**全部**后续条件的请求，断言"最先失败的那一步"赢：

| 步 | 条件 | 结果 | 测试断言 |
| --- | --- | --- | --- |
| 1 | 路由未命中（404/405） | `AdmissionError::Route` | token 错 + Host 错 + Origin 错 + 队列满 + 500 MB `Content-Length` 同时存在，仍然返回路由结论 |
| 2 | token | 401 | 上面这些错误同时存在 → 401 |
| 3 | `Host` | 421 | Origin 错 + 队列满 + 500 MB 同时存在 → 421 |
| 3b | `Origin`（仅 POST/PUT/DELETE） | 403 | 队列满 + 500 MB 同时存在 → 403 |
| 4 | **队列容量（尚未读 body）** | 503 `busy` | 同时给出 500 MB `Content-Length` → **仍然是 503**，证明没有先读/先算长度 |
| 5 | `Content-Length` 预检 | 413 `payload_too_large` | 恰好等于上限放行、上限 +1 拒绝 |
| 6 | 有界流式读取 | 413 / 408 / 400 | 见下 |
| 7 | 媒体类型 | 400 | 见下 |

**第 1 步为什么不返回 `ServeError`**：M0c 的 `ServeError` 变体清单（§11.1）里没有 404/405
的 `code`，本阶段**不伪造**一个，而是把路由结论原样回传（`AdmissionError::Route`），
由 M1 的路由层决定状态码。这是唯一一处"没有变成 `ServeError`"的第 1 步失败。

**第 6 步（有界读取）**：

| 场景 | 结果 | 测试 |
| --- | --- | --- |
| 分块拼接、恰好到上限 | 通过 | `read_body_concatenates_chunks_and_checks_the_declared_length` |
| 单块或累计超过上限（chunked 同样受限） | 413 `payload_too_large` | `read_body_rejects_a_stream_that_exceeds_the_cap`、`chunked_requests_skip_the_length_pre_check_but_keep_the_stream_cap` |
| 实际长度 ≠ 声明长度（多或少） | 400 `bad_request` | 同上 |
| 判定前缓冲区已超限 / 声明长度小于已到达字节 | 413 / 400 | `a_body_already_buffered_beyond_the_limit_is_rejected_before_reading` |
| 读取超时（首块前 / 中途 / 显式 `TimedOut`） | 408 `request_timeout`，**不返回部分 body** | `read_body_reports_a_read_timeout` |
| 连接中断 | 400 | `read_body_maps_a_broken_stream_to_bad_request` |
| chunked 谎报 `Content-Length` | 忽略声明值，`expected_body = None`，只受流式上限 | `chunked_requests_skip_the_length_pre_check_but_keep_the_stream_cap` |

**上限算术（`limits.rs`，唯一实现）**：

```rust
pub const MIB: u64 = 1024 * 1024;
pub struct ServeConfigError;   // Display = "invalid --max-body-mb=0: the limit must be greater than zero (MiB)"
pub struct RawServeLimits { max_body_mb, max_result_mb, max_export_mb, max_download_mb,
                            max_queue_text, max_queue_formula, max_consecutive_text,
                            max_consecutive_formula, max_retained, max_retained_mb,
                            max_tombstones, job_ttl_secs }
impl RawServeLimits { pub fn validate(self) -> Result<ServeLimits, ServeConfigError> }
pub struct ServeLimits { /* 全部已换算成字节 / 毫秒 */ }
```

| 边界（§12「准入参数边界」） | 结果 | 测试 |
| --- | --- | --- |
| `--max-body-mb=0`（四个 MiB 开关 + `--max-retained-mb`） | 可定位配置错误，**带开关名** | `zero_mib_limits_are_rejected_with_the_flag_name` |
| `--max-body-mb=0` 从 CLI 进来 | clap 能解析（不管语义），`validate()` 拒绝 | `zero_mib_limits_parse_but_fail_validation_with_the_flag_name`（`cli.rs`） |
| 荒谬大整数（> u64） | clap 解析期拒绝并点名开关 | `absurd_mib_values_are_rejected_and_never_wrap` |
| 乘法溢出（`2^44` MiB = `2^64` B） | 拒绝并说明"…overflows the byte limit (u64)"；`u64::MAX` MiB 同样拒绝，**绝不 wrap** | `mib_multiplication_overflow_is_rejected_and_never_wraps` |
| 边界内（`2^44 - 1` MiB、`1` MiB、`7` MiB） | 精确换算 | 同上、`mib_to_bytes_is_exact` |
| 队列/连续配额/保留数/tombstone/TTL = 0 | 拒绝（0 会让 §8.3、§4.5 的语义失效） | `zero_queue_and_retention_parameters_are_rejected`、`job_store_limits_reject_zero`、`zero_capacities_and_quotas_are_rejected_with_the_flag_name` |
| `--job-ttl-secs` 秒→毫秒溢出 | 拒绝 | `job_ttl_seconds_overflow_is_rejected` |

**第 7 步（媒体类型）**：接受 `application/octet-stream`（大小写/参数不敏感）与 `image/*`，
**缺省头按原始体处理**；拒绝 `multipart/form-data`、`application/x-www-form-urlencoded`、
`text/plain`、`application/json`、空串（400 `bad_request`），测试
`only_raw_bodies_and_images_are_accepted_for_decoding`。

> **文档接缝**：`docs/05` §13 的参考 curl 命令用 `--data-binary` 而没有显式设 `Content-Type`，
> curl 会给它加上 `application/x-www-form-urlencoded`，按上面的规则会被 400 拒绝。
> M1 落文档时应把参考命令写成
> `-H "Content-Type: application/octet-stream"`（或在 M1 决定把该类型也纳入白名单）。
> 本阶段**不**为了让示例过而放宽白名单。

---

### 交付物 6：仅本机安全助手（§7.1–§7.3、§9）

**文件**：`src/bin/serve/security.rs`。

```rust
pub const LOOPBACK_BIND_ADDRESS: &str = "127.0.0.1";
pub fn bind_address(port: u16) -> SocketAddr;               // 签名里没有地址参数
pub fn assert_loopback(SocketAddr) -> Result<SocketAddr, NonLoopbackBindError>;
pub struct LocalOrigin;                                     // 由实际端口计算的允许集合
impl LocalOrigin {
    pub fn for_port(port: u16) -> Self;
    pub fn from_bound(SocketAddr) -> Result<Self, NonLoopbackBindError>;
    pub fn allowed_hosts/allowed_origins(&self) -> &[String];
    pub fn primary_origin(&self) -> &str;
    pub fn check_host(Option<&str>) -> Result<(), ServeError>;                 // 421 bad_host
    pub fn check_origin(Option<&str>, is_state_changing: bool) -> Result<(), ServeError>; // 403
}
pub struct ServeToken;   // generate()/as_str()/matches()（常量时间），Debug 自动脱敏
pub fn generate_nonce() -> String;
pub const SECURITY_HEADERS: [(&str, &str); 3];
pub const CSP_NONCE_PLACEHOLDER: &str = "__CSP_NONCE__";
pub const TOKEN_PLACEHOLDER: &str = "__SRV_TOKEN__";
pub fn inject(html, nonce, token) -> String;
pub fn assert_no_placeholders_left(html) -> Result<(), PlaceholderError>;   // 严格：两个字面量
```

| 要求（§7.1/§7.2） | 实现与测试 |
| --- | --- |
| 监听地址硬编码、无参数/配置/环境变量可改 | `bind_address(port)` 只接受端口；`the_bind_address_is_hardcoded_loopback_ipv4` 断言四个端口的 IP 恒为 `127.0.0.1`；`cli.rs` 断言 `--host/--address/--bind/--listen/--ip` 均**无法解析** |
| 启动断言拒绝非 loopback | `assert_loopback`：接受 `127.0.0.1`，拒绝 `192.168.1.5`、`0.0.0.0`、**`[::1]`**、`127.0.0.2`（后者是刻意的：允许的 Host 只有 `127.0.0.1`，放行就会重现"绑定地址与 Host 校验互相矛盾"的老问题）；错误文本含地址与"there is deliberately no --host option" |
| 允许集合由**实际端口**计算、IPv4 only | `LocalOrigin::from_bound` / `for_port`；`allowed_hosts_are_ipv4_only_and_bound_to_the_actual_port` 断言 `{127.0.0.1:8760, localhost:8760}`，拒绝 `[::1]:8760`、端口不匹配、无端口写法、`evil.example:8760`、`127.0.0.1:8760.evil`、同形异义写法、空串与**缺失 Host** |
| 端口 80 的浏览器行为 | `for_port(80)` 额外放行不带端口的写法（浏览器不会发 `:80`）：`port_80_also_accepts_the_bare_host_...` |
| `Origin` 仅 `POST/PUT/DELETE`，缺失/`null`/不匹配 → 403 | `origin_is_required_and_must_match_exactly_on_state_changing_methods`（拒绝 `null`、端口不匹配、`https://`、其他 origin、带路径、`[::1]`、空串、缺失）；`read_requests_do_not_require_an_origin` |
| 每进程令牌 + 常量时间比较 | `ServeToken::generate`（256 bit 十六进制，两次生成必不同）+ `matches`（循环次数只取决于期望值长度，越界补 0，长度差与逐字节差一起累加，无提前返回）；`Debug` 只输出长度：`the_token_is_redacted_in_debug_output` |
| 响应头（§7.3） | `SECURITY_HEADERS` = `X-Content-Type-Options: nosniff`、`Referrer-Policy: no-referrer`、`Cache-Control: no-store` |
| 注入契约（§9/§9.5） | `inject` 只替换 `__CSP_NONCE__` / `__SRV_TOKEN__` 两个字面量；`assert_no_placeholders_left` 严格匹配并**报出是哪一个**残留（`the_placeholder_literals_are_exactly_the_two_documented_strings`、`inject_replaces_both_placeholders...`、`assert_no_placeholders_left_names_the_leftover_placeholder`） |

**威胁模型（如实记录）**：token 的熵来自系统时间（秒 + 纳秒）、进程 ID、进程内计数器与一个
栈地址（ASLR），经 SHA-256 兑成十六进制。它**不是**密码学 RNG，但满足"每进程一次、
本机其他进程难以预测"；本项目**不**为它新增依赖（§2.2 只允许 `tiny_http`）。
它防的是"本机浏览器里的恶意页面触发 loopback 请求"，不承担会话/身份语义。

---

### 交付物 7：CLI 选项面（§3，无行为）

**文件**：`src/bin/serve/cli.rs`。

```rust
pub enum ProviderChoice { Cpu, Directml, Cuda }   // clap::ValueEnum
pub struct ServeArgs { /* §3 的全部 21 个选项 */ }
impl ServeArgs {
    pub fn raw_limits(&self) -> RawServeLimits;
    pub fn provider_preference(&self) -> Option<ProviderPreference>;
    pub fn allow_provider_fallback(&self) -> bool;
    pub fn model_dir(&self) -> PathBuf;                       // 缺省 = 库的 default_model_store_dir()
    pub fn engine_config(&self) -> Result<EngineConfig, StartupConfigError>;
    pub fn uses_documented_defaults(&self) -> bool;
}
```

**选项名清单被逐项枚举断言**（`the_option_surface_is_exactly_the_documented_list`）：从
`clap::Args::augment_args` 取出全部 long 名，与 21 个文档名的集合**完全相等**，
且显式断言不含 `host` 与 `ocr-workers`；`neither_host_nor_ocr_workers_can_be_parsed`
进一步证明这些开关**无法解析**（含 `--address/--bind/--listen/--ip`）。

**默认值逐项对照 §3**（`every_default_matches_the_document`）：端口 8760、
body/result/export/download = 32/8/32/1024 MiB、队列 4/2、连续配额 4/1、
保留 32 个 / 64 MiB、tombstone 256、TTL 600 s、`--allow-download` 与
`--allow-provider-fallback` 默认 `false`（后两者用 `const { assert!(…) }` 编译期锁住）。

> **一处刻意的"不写默认值"**：`--provider` 与 `--max-side` **没有** clap 默认值。
> 原因是 §3 冻结的优先级是 **CLI flag > `--config` YAML > 内建默认**：若给 clap 写死
> `default_value`，YAML 里的 `provider_preference` / `max_side_len` 会被静默丢弃，
> 优先级规则不可能实现。它们的"默认"由内建默认承担（`cpu` / 库内 `max_side_len=2000`），
> 并由 `ServeConfigPlan::validate` 在启动期解析出**生效值**。

---

### 验证命令与结果（`docs/05` §12 要求）

在本 crate（`crates/rapid-ocr-rs`）执行；日志：`target/m0c-final-sweep.log`（下表 1–7 号的
单次连续执行，全部 exit 0）、`target/m0c-final-test-default.log`、`target/m0c-final-test-serve.log`、
`target/m0c-clippy-default.log`、`target/m0c-clippy-serve.log`、`target/m0c-build-release.log`、
`target/m0c-tree-*.txt`、`target/m0c-gate/`、`target/m0c-naive-text-first.log`、
`target/m0c-naive-formula-first.log`、`target/m0c-baseline-test.log`（开工基线）。

| # | 命令 | 结果 | 退出码 |
| --- | --- | --- | --- |
| 1 | `cargo fmt --all` | 无输出 | 0 |
| 2 | `cargo fmt --all -- --check` | 无输出（M0a 报告里提到的 `serve/*` 与 `src/exports.rs` 的 fmt diff 已由本次 `cargo fmt --all` 消除） | 0 |
| 3 | `cargo clippy --all-targets -- -D warnings` | `Finished dev profile`，无 warning | 0 |
| 4 | `cargo clippy --features serve --all-targets -- -D warnings` | `Finished dev profile`，无 warning（先修掉 3 条：`collapsible_if` ×1、`assertions_on_constants` ×2） | 0 |
| 5 | `cargo test --all-targets` | 334 + 2 + 4 + 14 + 0 = **354 passed, 0 failed** | 0 |
| 6 | `cargo test --features serve --all-targets` | 354 + **105** = **459 passed, 0 failed** | 0 |
| 7 | `cargo build --release --bins` | `Finished release profile in 33.07s`；`rapidocr.exe` 33,401,344 B、`bench_warm_e2e.exe` 33,249,792 B、`formula_eval.exe` 28,418,560 B、`formula_bench.exe` 27,724,288 B | 0 |
| 8 | `cargo build --release --bins --features serve` | `Finished release profile in 20.47s`（feature 打开也能出 release 二进制） | 0 |
| 9 | `cargo tree -e normal -p rapid-ocr-rs` / `… --features serve` | 各 606 行，**SHA-256 完全相同**（`BD2AB5E4…C3F6FC`） | 0 |
| 10 | `cargo tree -e normal --no-default-features` | 正常；三份树里 `tiny_http` 出现 **0** 次 | 0 |
| 11 | `bench_warm_e2e.exe --max-side-len 2000 --warmup-rounds 1 --rounds 3 --intra-threads 16 --output target/m0c-gate/bench-cpu-2000.json` | 12 图；`regions.avg = 34.833333333333336` | 0 |
| 12 | `rapidocr.exe evaluate --manifest <golden> --config test-config-small.yaml --output target/m0c-gate/evaluation-cpu.json` | 12 例；`mean_cer = 0.44765135645866394` | 0 |

**基线对比（AGENTS.md §6）**

| 项目 | 修改前（M0c 开工基线 `target/m0c-baseline-test.log`） | 修改后 |
| --- | --- | --- |
| `cargo test --all-targets` | 308 + 2 + 4 + 14 + 0 = **328 passed, 0 failed** | **354 passed, 0 failed**（+26 来自并发的 M0a） |
| `cargo test --features serve --all-targets` | 与上面相同（当时还没有 `serve` feature） | **459 passed, 0 failed**（+105 M0c） |
| 删除/跳过/弱化的测试 | — | **0** |
| 默认构建依赖图 | 606 行 | 606 行，SHA-256 未变 |

---

### 依赖隔离（默认构建的依赖图）

- `serve` 是**空 feature**（`serve = []`），不在 `default` 里：`cargo tree -e normal -p
  rapid-ocr-rs` 与 `… --features serve` 的输出**逐字节相同**（606 行，SHA-256
  `BD2AB5E41B1A6D649E2F80B0D3D3E55327B96EB7C6F861E55DFC7C8501C3F6FC`），
  即"打开 `serve` 今天不新增任何依赖"。
- `tiny_http` 在三份依赖树里都**不存在**（M1 才引入，且必须 optional）。
- 另一条独立证据是编译期的：`src/bin/serve/mod.rs` 的
  `the_scaffold_does_not_contain_an_http_server_yet` 扫描 `serve/**/*.rs` 的**代码行**
  （去掉行注释后）不得含 `tiny_http::` / `TcpListener` / `TcpStream`，并断言
  `Cargo.toml` 的 `[dependencies]` 段**没有**声明 `tiny_http`。
- `git diff Cargo.toml` 只有那一个空 feature 块（+6 行注释），依赖清单本身未被改动。

---

### 12 图硬门槛

**本阶段重跑了门槛，并得到逐位相同的结果**（输出写在 `target/m0c-gate/`，
**没有覆盖** `tests/baseline/` 下已提交的基线，`git status -- tests/baseline` 为空）：

| 门槛 | 文档要求 | 本次实测（release 二进制） | 已提交基线 | 结论 |
| --- | --- | --- | --- | --- |
| 12 图 mean CER | `0.44765135645866394` | **`0.44765135645866394`**（12 例） | `0.44765135645866394` | 字面量逐位相同 |
| 12 图区域数均值 | `34.833333333333336` | **`34.833333333333336`**（12 图 × 3 轮） | `34.833333333333336` | 字面量逐位相同 |

比较方式是对两份 JSON 的**原始数字字面量**做精确字符串比较（不是浮点近似）：
`regions literal identical: True`、`mean_cer literal identical: True`。
本阶段没有改动任何推理代码（`ImageInput → OcrRequest → OcrOutput` 一行未动），
库侧改动只有 `format_provider_preference` 的可见性与两条 `pub use`，因此这一结果与
"未改推理"是一致的、也是可预期的。

---

### 关键行为对比（AGENTS.md §9）

| 项目 | 修改前 | 修改后 | 预期结果 |
| --- | --- | --- | --- |
| `serve` feature（新增） | 不存在 | 空 feature，**不进 `default`**；默认依赖图逐字节不变 | §2.1 的 feature 隔离 |
| 引擎状态（新增） | 无概念（文档承认"启动期建引擎与模型缺失仍可访问"矛盾） | `ServiceState`/`EngineState` 五态 + 9 条合法转换 + 非法转换可定位错误；`Loading` 期间 OCR 排队不失败、`Failed` 期间 503 带 `reason` | §7.6 |
| `/api/status` provider 三字段（新增） | 无 | 三键始终存在；未知态是 `null`（**不是** `false`） | §7.5/§9 |
| 错误契约（新增） | `RapidOcrError` 直接被 `?` 冒泡，没有 HTTP 语义 | `ServeError` 22 变体 + 唯一映射表 + 固定三键错误体 | §11.1 |
| 准入顺序（新增） | 无 | 1→7 步顺序被展开断言；**队列容量判定在读取 body 之前**，500 MB 的 `Content-Length` 也不会被读 | §4.4 |
| 上限参数（新增） | 无 | MiB→字节唯一实现；0 / 溢出 / 荒谬大整数全部是可定位配置错误，**绝不 wrap** | §4.4/§4.6/§6.2 |
| 双队列公平（新增） | 无（文档记录了上一版"公式队列被永久饿死"） | 双向配额 + 可证明上界 `capacity × 对方配额`；朴素策略下 400 步内被饿死方向服务 0 次（已实测失败） | §8.3 |
| 结果存储（新增） | 无 | 数量 + 字节 + TTL 三重上限；tombstone 容量 + TTL 双重上限；404 与 410 可区分 | §4.5 |
| 安全模型（新增） | 无 | 监听地址硬编码（无任何开关可改）；`[::1]` 明确拒绝；`Origin` 缺失/`null`/不匹配 → 403；token 常量时间比较 | §7.1/§7.2 |
| CLI 面（新增） | 无 | 21 个文档选项，无 `--host`、无 `--ocr-workers`；默认值逐项对照 | §3 |
| 依赖 | 无 HTTP/网络依赖进库 | 库依赖清单未变；`serve` 代码全部在二进制侧且 feature 关闭时不编译 | §2.1 |
| 性能表现 | — | 无热路径改动：推理链路一行未动，12 图门槛逐位相同；serve 侧纯逻辑无 I/O | 无退化 |

---

### 接缝（本阶段**没有**填、必须由 M0a/M0b/M1 补的地方）

1. **模型完备性 → 状态机**（M0a 已就绪，M1 接线）：`state::ModelReadiness` 只接收
   "缺哪些文件"。M1 必须用 M0a 的 `validate_model_files` / `ModelSetStatus`
   （`rapid_ocr_rs::{ModelFileState, model_set_status}`）填充，并把缺失/损坏文件名
   与 `/api/models` 的字段保持一致。
2. **`ServeError::Download(DownloadError)` 与库侧下载器统一**（M0b）：本阶段按 §6.1 第 11 条
   定义了 serve 侧需要的十类错误，`DownloadError` 目前**只存在于 `src/bin/serve/error.rs`**，
   构造函数只有 `From<DownloadError> for ServeError`（加固下载器 M0b 才会产生它）。
   M0b 的加固下载器落地后，两者必须统一（把类型移进库，或让库返回可转换的错误），
   否则同一件事会有两套错误表示。旧入口 `RapidOcrError::Download(String)` 目前**留在
   `ServeError::Ocr(...)` 里**（保留原始错误文本），只由 `classify_ocr_error` 归到
   502 `download_failed` / `kind: "download"`；M0b 删除旧入口后这条分类分支应当消失。
3. **§4.3 的 `job_finished`**：终态任务再取消在 §4.3 里有一个独立的 409 `job_finished` 文案，
   但 §11.1 的变体清单（也就是本阶段被要求实现的清单）里没有任何变体能产生该 `code`，
   因此本阶段对"运行中不可取消"与"已结束"共用 `not_cancellable`。
   M1 若要在 UI 上区分，需要新增变体，或直接用 `/api/jobs/{id}` 的 `state` 判断。
4. **§7.6 的 OCR 409 体**：`BlockedModelsMissing` 要求"字段与 `/api/models` 一致"，
   这与 §11.1 的通用三键错误体不是同一个形状。本阶段保留 `OcrAdmission::ModelsMissing
   { missing }` 承载清单、`ServeError::ModelsMissing` 承载 `code`，**形状决策留给 M1**。
5. **路径脱敏（§7.4）**：`Ocr(RapidOcrError)` 的 `detail.error` 直接来自库（`ModelResolve`
   等可能包含本机绝对路径）。M0c 未做脱敏（它是响应层策略），M1 在拼响应时必须处理。
6. **读取超时的实体**：`BodySource::read_timed_out` 由 M1 用 `Instant` 实现
   （`tiny_http` 本身不提供 socket 读超时）；本阶段只固定策略与错误映射。
7. **`Content-Type` 与 §13 参考命令的冲突**：见交付物 5 的注记。
8. **`#![allow(dead_code)]`**：只作用于 `serve` 子树，用于"纯逻辑已写完、HTTP 层未接线"
   的过渡期；**M1 接线时必须删除**（M1 的验收要求默认 lint 集下无警告）。

---

### 未覆盖风险

1. **未提交、未在干净 clone 上验证**：按要求不 commit；证据来自当前工作树。
2. **`src/bin/serve/**` 只有单元测试，没有任何端到端验证**：本阶段没有 HTTP 服务器，
   因此"浏览器闭环""Host/Origin/token 的真实请求路径""202 → queued → running → succeeded"
   这些 §12 的验收项**一条都没有跑**，全部属于 M1。它们不是"通过"，是"未开始"。
3. **`elapsed_ms` / `queued_ms` 的口径**：定义为"进程内单调毫秒"，由调用方注入；
   M1 必须选一条真实的单调时钟（文档没有规定起点），本阶段只固定相对关系。
4. **token 熵不是密码学 RNG**（见交付物 6 的威胁模型）。若要提升，需要引入
   `getrandom`/`BCryptGenRandom` 之类的依赖，那与 §2.2"只允许 `tiny_http`"冲突，需先改文档。
5. **`DownloadError` 只有映射与构造，没有下载行为**：重定向/host/磁盘空间等**实际**拦截
   属于 M0b，本阶段只保证"错误能被正确分类并映射"。
6. **并发工作流仍在同一个工作树上写入**（M0a 的文件、`src/exports.rs` 等）。
   本阶段的最终验证（`fmt/clippy/test/release/tree/门槛`）记录在上表，全部在
   M0a 的改动已在树上的状态下取得；`src/bin/serve/**` 是 M0c 独占的新文件
   （M0a 的报告里只观察到它们造成的 fmt diff，其 M0a 记录明确说明"刻意不修"）。
7. **`cargo doc --no-deps` 未重跑**：M0a 报告过 3 条既有 `private_intra_doc_links` 警告；
   本阶段新增模块的文档链接只指向 `serve` 子树内的项与库的公开项，但**没有实测**，
   不作为"0 警告"的依据。
8. **`--max-download-mb` 与 566 MB 公式模型的关系**：只断言默认值 > 0（§6.2 的字面要求）
   与默认 1024 MiB > 566 MB；没有在启动期强制"必须大于 566 MB"（那会让只跑普通模型的
   用户无法把上限调小）。

---

### 与 `docs/05` §11「M0」验收清单的对照

| §11 M0 条目 | 本阶段 | 证据 |
| --- | --- | --- |
| 删除 `--host`；监听地址硬编码；启动断言属 loopback；Host/Origin 允许集合含实际端口（§7.1） | ✅ 完成 | 交付物 6；`bind_address(port)` 无地址参数；`--host/--address/--bind/--listen/--ip` 全部无法解析 |
| tombstone 表（容量 + TTL）+ 404/410 区分（§4.5） | ✅ 完成 | 交付物 3；`tombstone_capacity_eviction_*`、`tombstone_ttl_expiry_*`、`unknown_jobs_are_404_while_evicted_jobs_are_410` |
| `ModelSet`/… + 共享逐文件校验（§5.1/5.2） | ✅ M0a | 见上文 M0a 记录 |
| `ModelManifest` 通用化 + 单一来源（§5.3） | ✅ M0a | 同上 |
| 字典补 SHA-256；无哈希不得 `complete`（§1.2、§5.2） | ✅ M0a | 同上 |
| 加固下载器 + `--max-download-mb` + `MoveFileExW` + 迁移 CLI（§6） | ⛔ 不在 M0c（M0b）；**`--max-download-mb` 的解析与校验已完成** | 交付物 5 |
| provider 回退语义 + 启动期配置校验 + 引擎创建期可用性 + `/api/status` 三字段（§7.5） | ✅ 完成（除"引擎创建期"本身，属 M1 的 `Loading → Ready\|Failed`） | 交付物 1 |
| `ServiceState` / `EngineState` 状态机与全部转换（§7.6） | ✅ 完成 | 交付物 1；21 个测试含 `Rebuilding`（M3） |
| `ServeError` 与状态码映射（§11.1，含 `engine_unavailable` / `export_too_large`） | ✅ 完成 | 交付物 2；`every_variant_*`、`every_download_error_*`、`every_rapid_ocr_error_variant_is_mapped` |
| 准入顺序（§4.4）与 `--max-result-mb`（§4.6） | ✅ 完成 | 交付物 5 |
| 双队列容量与**双向**公平调度参数（§8.3） | ✅ 完成 | 交付物 4（含朴素策略失败演示） |
| **M0 验收**：以上每项都有单元测试；`cargo test` 全绿；文档与实现一致 | ✅ 本阶段范围内成立 | 459 passed / 0 failed（含 105 个 M0c 测试）；本文件 |
| M1 的条目（`serve` 子命令行为、`tiny_http`、路由、`GET /`、前端、CSP 响应、真实 12 图经 HTTP） | ⛔ 不在 M0c | 见"未覆盖风险"第 2 条 |

### 本阶段**不做**的事（范围边界）

- HTTP 服务器、`tiny_http`、路由表、任何 socket；`GET /`、静态页、前端与 CSP 响应头拼接；
- `POST /api/ocr` 等端点的实际行为与 `202 → queued → running → succeeded` 的端到端闭环；
- 加固下载器本体（重定向/host 白名单/磁盘空间/原子替换/单飞）；
- `serve` 子命令接入 `rapidocr` 的 `Command` 枚举、未启用 feature 时的可定位错误（M1）；
- 未编辑 `src/model_set.rs` / `src/model_source.rs` / `src/model_registry.rs` /
  `src/model_store.rs` / `src/api.rs` / `assets/default_models.yaml`（M0a 在途）；
- 未提交任何 commit。

---

## M0b：加固下载器（`download_verified` / `DownloadError` / 预算 / 调用方迁移）

**阶段**：M0b —— `docs/05` §11「M0」里 §6（加固下载器）+ §6.2（`--max-download-mb`
语义）+ §6.4（迁移并删除旧入口）+ §6.7（Windows 原子替换）的全部条目。
**不做**（属 M1/M2）：HTTP 服务器、路由、serve 的下载 worker 与取消（§6.6 的
"文件边界取消"只定义错误类，不接线）、下载进度上报。
**开工基线**：`1144ddb`（M0a「模型清单」+ M0c「serve 核心」已提交；本阶段在其上继续）。
**日期**：2026-10-03
**提交**：`（未提交：按要求不 commit）`

**变更摘要**（15 个文件，`git diff --numstat`：+3098 / −192，其中本记录的追加占
+411/−0；`src/model_store.rs` +2130/−61、`src/test_support.rs` +351/−0）：

| 文件 | 改动 | 说明 |
| --- | --- | --- |
| `src/model_store.rs` | 182 → 2251 行（1 → **35** 测试） | `download_verified` 唯一入口、`DownloadError` 十二类、`DownloadBudget`、`download_model_set`、`MoveFileExW`/`GetDiskFreeSpaceExW` 绑定；删除 `ensure_downloaded` |
| `src/test_support.rs` | 164 → 515 行（+1 测试） | 本机 HTTP fixture 服务器（`HttpFixture`）与其自身回归测试 |
| `src/bin/serve/error.rs` | 923 → 906 行（7 测试） | 删除 serve 侧重复的 `DownloadError`，改为库类型上的 `DownloadErrorMapping` 映射 |
| `src/bin/serve/limits.rs` | 485 → 531 行（6 → **7** 测试） | `DEFAULT_MAX_DOWNLOAD_MB` 改为引用库常量；新增 `ServeLimits::download_budget()` |
| `src/error.rs` | `RapidOcrError::Download(String)` → `Download(DownloadError)`（`#[error(transparent)]` / `#[from]`） | 分类信息不再被压成字符串 |
| `src/exports.rs` | 导出下载器公开面；`ensure_downloaded` 从公开面消失 | — |
| `src/ocr/det/detector.rs`、`src/ocr/cls/classifier.rs`、`src/ocr/rec/recognizer.rs` | 4 个下载调用点迁移；`&PathBuf` → `&Path` | 见交付物 2 |
| `src/input/image_loader.rs` | 远端图片取回失败的分类从 `Download(String)` 改为 `DownloadError::Network` | 见交付物 2 的发现 1 |
| `src/bin/serve/mod.rs`、`src/model_set.rs`、`src/model_registry.rs`、`src/evaluation/formula/report.rs` | 文档/注释同步（不再指向已删除的入口） | 无行为变化 |
| `docs/05-local-web-demo-implementation.md` | §13 参考命令补 `Content-Type` | 交付物 5 |
| `docs/06-local-web-demo-reports.md` | 本记录（追加） | — |

---

### 交付物 1：`download_verified` 取代 `ensure_downloaded`（§6）

```rust
pub struct DownloadRequest<'a> {
    pub url: &'a str,
    pub expected_sha256: &'a str,   // 必填
    pub save_dir: &'a Path,
    pub max_bytes: u64,             // 来自 --max-download-mb（§6.2）
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
}
impl<'a> DownloadRequest<'a> {
    /// 库内调用方（EngineConfig::allow_download 分支）的默认请求。
    pub fn new(url: &'a str, expected_sha256: &'a str, save_dir: &'a Path) -> Self;
}
pub fn download_verified(req: &DownloadRequest<'_>) -> Result<PathBuf>;

pub const ALLOWED_DOWNLOAD_HOSTS: [&str; 1] = ["www.modelscope.cn"];
pub const DEFAULT_MAX_DOWNLOAD_MB: u64 = 1024;
pub const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = DEFAULT_MAX_DOWNLOAD_MB * 1024 * 1024;
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

pub fn require_model_hash<'a>(expected: Option<&'a str>, source: &str) -> Result<&'a str>;

pub struct DownloadBudget { /* total_bytes, spent_bytes */ }
impl DownloadBudget {
    pub const fn new(total_bytes: u64) -> Self;
    pub const fn total_bytes(&self) -> u64;
    pub const fn spent_bytes(&self) -> u64;
    pub const fn remaining_bytes(&self) -> u64;
    pub const fn per_file_cap(&self) -> u64;      // = 剩余额度
    pub fn charge(&mut self, bytes: u64) -> Result<()>;   // 超出 → TooLarge，且不记账
}

pub fn download_model_set(set: &ModelSet, root: &Path, budget: &mut DownloadBudget,
                          connect_timeout: Duration, read_timeout: Duration)
                          -> Result<Vec<PathBuf>>;

pub enum DownloadError {
    SchemeRejected { scheme: String },
    RedirectRejected { location: Option<String> },
    HostRejected { host: String },
    TooLarge { limit_bytes: u64, observed_bytes: Option<u64> },
    Network { detail: String },
    ConnectTimeout { timeout_ms: u64 },
    ReadTimeout { timeout_ms: u64 },
    InsufficientSpace { required_bytes: u64, available_bytes: u64 },
    HashMismatch { expected: String, actual: String },
    Cancelled,
}
impl DownloadError { pub const fn kind(&self) -> &'static str }   // 机器可读标签（进 detail.kind）
```

`sha256_file` / `verify_existing_file` / `default_model_store_dir` **签名与实现未改**。

**§6.1 的十二条要求 → 实现 → 测试**（每条测试都在 `src/model_store.rs`，全部通过）：

| # | 要求 | 实现 | 测试（实测结果） |
| --- | --- | --- | --- |
| 1 | 仅 HTTPS | `url.scheme() != "https"` → `SchemeRejected`，在**任何**文件系统/网络动作之前 | `the_public_entry_point_rejects_scheme_and_host_before_any_side_effect`（走公开入口，断言 `save_dir` **未被创建**） |
| 2 | 禁止自动重定向（**M2b 起改为逐跳跟随**，见文末 M2b 记录） | `ClientBuilder::redirect(Policy::none())`；3xx → `RedirectRejected{location}` | `a_redirect_is_rejected_and_nothing_is_written`（302 + Location，目录为空，请求数 1；**该用例已在 M2b 被 6 条 fixture 重定向用例取代**） |
| 3 | host 白名单来自可信配置 | `ALLOWED_DOWNLOAD_HOSTS` 编译期常量；整串大小写不敏感精确比较 | `the_allowed_download_hosts_are_exactly_the_declared_set`（逐项锁死 + 8 个越界 host）、`a_local_manifest_cannot_widen_the_download_host_allow_list`（`manifest.json` 声明 `https://evil.example/...` → 仍 `HostRejected`） |
| 4 | `Content-Length` 预检 | 在 `File::create` **之前**判定 | `a_declared_length_above_the_cap_is_rejected_before_anything_is_written`（目录里连 `.part` 都没有） |
| 5 | 流式上限 | `response.take(max_bytes + 1)`，累计超限即停 | `a_chunked_body_above_the_cap_...`（`observed_bytes == Some(1025)`）、`a_close_delimited_body_above_the_cap_...`，两者都断言临时文件已删除 |
| 6 | 唯一临时文件名 | `.part-<pid>-<seq>`（`AtomicU64` 序号） | `a_verified_download_writes_the_file_and_leaves_no_temp_file`、并发测试 |
| 7 | Windows 原子替换 | `MoveFileExW(src, dst, MOVEFILE_REPLACE_EXISTING \| MOVEFILE_WRITE_THROUGH)` | `an_existing_corrupt_target_is_replaced`（§6.7 的回归测试）、`a_failed_replace_keeps_the_original_file_and_drops_the_temp_file`、`a_replace_onto_a_directory_fails_and_keeps_the_directory` |
| 8 | 同文件单飞 | 目标路径 → `Arc<Mutex<()>>` 锁表 | `two_concurrent_downloads_of_one_target_fetch_exactly_once`（2 线程 + Barrier，**恰好 1 次**网络请求） |
| 9 | 哈希必填 | `&str` 必填 + 空串在 I/O 前拒绝；不匹配删除临时文件 | `a_hash_mismatch_deletes_the_temp_file_and_leaves_no_target`、`an_empty_expected_hash_is_refused_before_any_io` |
| 10 | 磁盘空间预检 | `GetDiskFreeSpaceExW`；空间来源经 `FreeSpaceProbe` 注入 | `insufficient_disk_space_is_reported_before_anything_is_written`（注入 1000 B）、`an_unknown_length_download_is_budgeted_at_the_streaming_cap`（注入 4000 B / 上限 4096 B） |
| 11 | 分项超时 | `connect_timeout()` 与 `timeout()`（阻塞客户端按"每次等待"计时）分开配置 | `a_stalled_body_read_is_a_read_timeout`、`a_server_that_never_answers_is_a_read_timeout_not_a_connect_timeout`、`a_connect_phase_timeout_and_a_read_phase_timeout_are_distinct_classes`、`a_refused_connection_is_a_network_error` |
| 12 | 错误分类唯一 | `DownloadError`（十二类）+ `kind()`；serve 只做 HTTP 映射 | `every_download_error_class_has_a_stable_kind`（10 个变体 × kind + 包进 `RapidOcrError` 后消息不丢） |

**另外两条与安全/可诊断性相关的新增测试**：
`a_url_whose_last_segment_is_not_a_bare_file_name_is_rejected`（URL 末段 `..` 不能变成
落盘路径：复用 `model_set::validate_model_file_name` 的唯一实现）、
`the_downloader_issues_a_plain_get_with_its_own_user_agent`（请求行与 UA 的形状，
fixture 侧记录 `index/method/target/headers`）。

#### 临时文件的生命周期（结构性保证，而不是逐分支手写清理）

临时文件由 `PartFile` 持有，**任何**提前返回（拒绝、超限、超时、哈希不符、替换失败）
都在 `Drop` 里删除它。因此"校验没过但文件还在"这条路径在结构上不存在：
替换成功才 `keep = true`。§6.1 第 5、9 条要求的"删除临时文件"由此覆盖，而不是靠
在每个 `return` 前手动 `remove_file`。

#### 关于 §6.7 的 `fs::rename` 降级路径：**没有保留**

§6.7 允许"把 `fs::rename` 作为**降级**路径，但必须记录崩溃窗口"。本阶段**不保留**降级
路径，只留一条 Win32 调用：std 的 `fs::rename` 在 Windows 上目前也走
`MoveFileExW(MOVEFILE_REPLACE_EXISTING)`，但它既不请求 `MOVEFILE_WRITE_THROUGH`，
也不把 Win32 错误码交给调用方——两条路径并存只会让"失败时发生了什么"无法定位。
失败时返回 `RapidOcrError::Io`，文本含 `MoveFileExW(<src> -> <dst>) failed with Win32
error <code>; the existing file was left untouched`（`src/model_store.rs` 的
`replace_file`）。

#### 连接超时 vs 读取超时：为什么判定用"实测耗时"

reqwest 0.12 的 blocking 客户端**不区分**"连接阶段超时"与"等待响应头超时"：
两者都是 `Kind::Request` + `TimedOut`，而 `is_connect()` 对连接**超时**为 `false`
（只对连接**错误**，例如目标拒绝连接，为 `true`）。本机实测（见下方"做不到的事"）：

| 场景 | `is_timeout()` | `is_connect()` | 说明 |
| --- | --- | --- | --- |
| 目标拒绝连接（端口无人监听） | `false` | `true` | → `Network` |
| 服务器只接受、不响应 | `true` | `false` | 阻塞层的读取预算到期 → `ReadTimeout` |
| 服务器发头后停住 | `true`（io 层包装） | `false` | → `ReadTimeout` |
| `connect_timeout = 1 ns` + 环回监听 | `true` | `false` | 真实原因是**读取**预算（连接 ~0.4 ms 内已完成） |

因此 `classify_send_timeout(is_connect, elapsed, connect_timeout, read_timeout)` 的规则是：
`is_connect` **或** 耗时未达到读取预算 → `ConnectTimeout`（报告连接预算）；否则
`ReadTimeout`（报告读取预算）。前提是 `connect_timeout < read_timeout`（库内默认
10 s / 30 s），已由 `the_library_defaults_match_the_documented_download_limits`
用 `const { assert!(...) }` 锁住。

---

### 交付物 2：迁移全部调用方并删除旧入口（§6.4）

`ensure_downloaded` **已删除**，`Option<&str>` 哈希入口**不存在**，也没有兼容 shim
（`src/` 里对它的引用只剩注释；`exports.rs` 的公开面已换成新的下载器）。

| 调用方 | 迁移前 | 迁移后 | 现在提供的哈希 |
| --- | --- | --- | --- |
| `src/ocr/det/detector.rs::Detector::new` | `ensure_downloaded(&resolved.model_url, resolved.sha256.as_deref(), …)` | `require_model_hash(...)` + `DownloadRequest::new(...)` + `download_verified` | 默认表 `resolve_det` 的 `SHA256`（v4/v5/v6 全部 40 个 det/cls/rec 条目都有，`every_default_table_entry_carries_a_sha256` 锁住） |
| `src/ocr/cls/classifier.rs::Classifier::new` | 同上 | 同上 | 默认表 `resolve_cls` 的 `SHA256` |
| `src/ocr/rec/recognizer.rs::resolve_model_path` | 同上 | 同上 | 默认表 `resolve_rec` 的 `SHA256` |
| `src/ocr/rec/recognizer.rs::resolve_character_path` | `ensure_downloaded(&dictionary.url, dictionary.sha256.as_deref(), …)`（M0a 起已是 `Some`，但**签名**允许 `None`） | 同上，字典哈希同样经 `require_model_hash` | 字典条目（M0a 补的 30 个 `dict.SHA256`） |
| `src/input/image_loader.rs::read_url` | `RapidOcrError::Download(format!(...))`（远端图片取回失败） | `RapidOcrError::Download(DownloadError::Network { detail })` | 不涉及哈希 |

**发现 1（需要记录）**：`RapidOcrError::Download(String)` 原来有**两个**来源——模型
下载器（本阶段删除）与"远端图片取不回"（`ImageInput::Url` 路径，本阶段保留）。
后者现在用同一个 `DownloadError::Network`，serve 侧状态码/`code` **不变**
（502 `download_failed`），只有 `detail.kind` 从 `"download"` 变成 `"network"`；
`src/evaluation/formula/report.rs::classify_error` 的 `Download → ImageDecode`
也保持不变（该层只从本地路径加载模型，不会触发模型下载），并加了注释说明原因。

**发现 2**：`src/bin/rapidocr.rs` **没有**独立的下载子命令（命令面是
`run` / `report` / `evaluate` / `check`）。CLI 触发的下载全部经由管线
（`EngineConfig::allow_download` → detector/classifier/recognizer 的构造），也就是上表的
4 个调用点。因此"CLI 下载路径"的迁移已经由上表覆盖，不存在第五个入口。

**发现 3**：`resolve_model_path` / `resolve_character_path` 的参数类型从 `&PathBuf`
改为 `&Path`（clippy `ptr_arg`；新下载器接受 `&Path`，`&PathBuf` 只会多一个间接层）。
这是纯类型收窄，调用点不变。

---

### 交付物 3：`--max-download-mb` 语义（§6.2）

- **默认值同源**：`serve::limits::DEFAULT_MAX_DOWNLOAD_MB` 现在等于
  `rapid_ocr_rs::DEFAULT_MAX_DOWNLOAD_MB`（1024），库内调用方与 CLI 不再各写一个 1024；
  `cli::tests::every_default_matches_the_document` 仍逐项断言 1024（测试未放松）；
- **0 值在启动期被拒**：既有 `RawServeLimits::validate` 的 `mib_to_bytes` 已覆盖
  （`zero_mib_limits_are_rejected_with_the_flag_name` 含 `--max-download-mb`），本阶段未改；
- **单文件上限 = 整批总量额度**：`ServeLimits::download_budget()` →
  `DownloadBudget::new(max_download_bytes)`；`download_model_set` 对每个文件用
  `budget.per_file_cap()`（= 剩余额度）作为该文件的 `max_bytes`，下载完成后再
  `charge(实际字节数)`，因此**剩余额度按文件递减**；
- **已知体积的提前拒绝**：`size_bytes > 剩余额度` → 在**发请求之前**返回 `TooLarge`
  （错误里带 `limit_bytes` 与 `observed_bytes` 两个数值）；
- **未知体积仍然受限**：`size_bytes: None` 只是跳过"提前预检"，该文件的
  `max_bytes` 依然是剩余额度，因此仍被流式上限保护。

测试：`limits::tests::the_download_budget_starts_at_the_single_file_cap_and_decreases_per_file`
（默认预算 1024 MiB；记 593,915,961 字节后 `per_file_cap == remaining`；一次超限的
`charge` **不消耗**预算）、`model_store::tests::a_download_budget_never_overspends_and_a_failed_charge_costs_nothing`、
`a_set_download_charges_every_file_against_one_running_budget`（3 个文件共用 100,000 预算，
请求 3 次、`spent == 6000`、`remaining == 94_000`）、
`a_set_download_refuses_a_file_whose_size_exceeds_the_remaining_budget_without_requesting_it`
（第 2 个文件声明 40 KiB > 剩余 10 KiB → **请求数 1**，第二个文件不存在）、
`a_set_download_bounds_an_unknown_size_file_by_the_remaining_streaming_cap`
（`None` 体积 + 1 KiB 预算 → `TooLarge{limit:1024, observed:1025}` 且临时文件已删）、
`a_set_download_skips_files_that_are_already_valid`（缓存命中不计预算、不发请求）。

`download_model_set` 还额外把 §5 的"文件名 = URL 末段"不变量变成**显式错误**
（`the two names must agree`），因为下载器按 URL 末段落盘，两者漂移会让逐文件状态校验
指向不存在的路径（M0a 已记录该规则）：
`a_set_download_refuses_unverifiable_unsourced_and_misnamed_files`。

---

### 交付物 4：本机 fixture 服务器的验证（§12「测试不得依赖公网」）

`src/test_support.rs` 新增 `HttpFixture`：`TcpListener::bind("127.0.0.1:0")` + 手写响应
（零依赖），每个响应带 `Connection: close`，因此 `request_count()` =
**网络请求次数**（单飞的证据）。可构造的响应：正常体 / 任意状态行 / 302+Location /
`Transfer-Encoding: chunked`（无 `Content-Length`）/ 关闭定界（无长度、不 chunked）/
发头后停住（读超时）/ 接受后完全静默。测试通过 `DownloadPolicy` 的
`#[cfg(test)]` 口子把这个服务器接进下载器：**生产构建里没有这个字段**
（`DownloadPolicy::production` 是生产路径的唯一构造点，白名单只认编译期常量、scheme 只认
`https`），因此"仅 HTTPS + 编译期白名单"是编译期保证，而传输/落盘/预算/单飞逻辑
在生产代码路径上被真实执行。

| 覆盖项 | 结果 |
| --- | --- |
| 正确哈希 → 落盘 + 无 `.part` 残留 + 1 次请求 | ✅ `a_verified_download_writes_the_file_and_leaves_no_temp_file` |
| 哈希不符 → 删除临时文件、目标不存在 | ✅ `a_hash_mismatch_deletes_the_temp_file_and_leaves_no_target` |
| 3xx → `RedirectRejected`（带 Location） | ✅ `a_redirect_is_rejected_and_nothing_is_written`（**M2b 起**：白名单内跟随 / 越界 host 拒 / 超限拒 / 降级拒 / 不带凭据 / 最终体仍受上限） |
| `http://` → `SchemeRejected` | ✅ 公开入口测试（不产生任何 I/O） |
| 白名单外 host → `HostRejected`（含伪装域名） | ✅ `the_public_entry_point_rejects_scheme_and_host_before_any_side_effect`、`a_local_manifest_cannot_widen_the_download_host_allow_list` |
| `Content-Length` 超限 → 未写任何文件 | ✅ `a_declared_length_above_the_cap_is_rejected_before_anything_is_written` |
| chunked / 无长度超限 → `TooLarge` + 删临时文件 | ✅ 两条（`observed_bytes == 1025`） |
| 目标已存在且**损坏** → 原子替换成功（§6.7 回归） | ✅ `an_existing_corrupt_target_is_replaced` |
| 目标已存在且正确 → 0 次网络请求 | ✅ `an_existing_valid_target_is_returned_without_any_request` |
| 替换失败 → 原文件存活、临时文件删除 | ✅ `a_failed_replace_keeps_the_original_file_and_drops_the_temp_file`（用 `share_mode(FILE_SHARE_READ)` 精确制造共享冲突失败；断言错误含 Win32 错误码与 `left untouched`） |
| 两个并发下载同一目标 → **恰好 1 次**请求 | ✅ `two_concurrent_downloads_of_one_target_fetch_exactly_once` |
| 磁盘空间分支（注入的空间来源） | ✅ 两条（已知长度 / 未知长度按 `max_bytes` 计） |
| 读取超时（发头后停住 / 完全不响应） | ✅ 两条端到端 |
| 非 2xx（404）→ `Network` | ✅ `a_non_success_status_is_a_network_error` |
| 不安全文件名（URL 末段 `..`） | ✅ 且断言 **0 次**请求 |

**fixture 自身的一个真实缺陷（已修 + 已加回归测试）**：Windows 上 `accept()` 返回的
套接字**继承监听套接字的非阻塞属性**（监听套接字为了可关闭被设成非阻塞）。第一版
fixture 没有把连接改回阻塞模式，于是"请求稍晚到达"时 `read` 立刻返回 `WouldBlock`，
服务器在没有响应的情况下关闭连接，客户端看到的是 `WSAECONNABORTED (10053)` ——
一个只在时序巧合下出现的假失败（实测在 150 次连续连接的压测里命中 4 次，分别在第
14 / 28 / 68 / 73 次，因此 `cargo test --all-targets` 会偶发失败）。修复是
`serve_connection` 里的 `stream.set_nonblocking(false)`，并把这条要求固化成 fixture
自己的回归测试 `test_support::tests::an_accepted_connection_waits_for_a_late_request`
（先连上、等 150 ms、再发请求）。**该测试做过变异验证**：把
`set_nonblocking(false)` 注释掉后它必然失败（已实测），因此它不是"永远为真"的测试。

---

### 交付物 5：`docs/05` §13 参考命令的媒体类型修正

`--data-binary` 会让 curl 发送 `Content-Type: application/x-www-form-urlencoded`，
而 §4.4 的准入顺序用**媒体类型白名单**（只接受 `application/octet-stream`），
该示例必然被拒。修正方式是**给示例补头**（并加注说明为什么必须显式声明），
**没有**放宽 `admit.rs` 的白名单：

```powershell
curl.exe -s -X POST --data-binary "@…\01基础多位置文本.png" `
  "http://127.0.0.1:8760/api/ocr?max_side=2000" `
  -H "Content-Type: application/octet-stream" `
  -H "X-RapidOCR-Token: <token>" -H "Origin: http://127.0.0.1:8760"
```

---

### 验证命令与结果（`docs/05` §12 要求）

在本 crate（`crates/rapid-ocr-rs`）执行；日志：`target/m0b-verify.log`（1–3 号）、
`target/m0b-clippy-default.log` / `m0b-clippy-serve.log`、`target/m0b-test-default.log` /
`m0b-test-serve.log`、`target/m0b-build-release.log`、`target/m0b-formula-integration.log`、
`target/m0b-gate/`、`target/m0b-tree-default.txt` / `m0b-tree-serve.txt`。

| # | 命令 | 结果 | 退出码 |
| --- | --- | --- | --- |
| 1 | `cargo fmt --all` + `cargo fmt --all -- --check` | 无输出 | 0 |
| 2 | `cargo clippy --all-targets -- -D warnings` | `Finished dev profile`，无 warning | 0 |
| 3 | `cargo clippy --features serve --all-targets -- -D warnings` | `Finished dev profile`，无 warning | 0 |
| 4 | `cargo test --all-targets` | 369 + 2 + 4 + 14 + 0 = **389 passed, 0 failed** | 0 |
| 5 | `cargo test --features serve --all-targets` | 369 + 2 + 4 + 14 + **106** = **495 passed, 0 failed** | 0 |
| 6 | `cargo build --release --bins` | `Finished release profile in 17.46s`；`rapidocr.exe` 33,420,800 B、`bench_warm_e2e.exe` 33,269,760 B、`formula_eval.exe` 28,419,072 B、`formula_bench.exe` 27,727,360 B | 0 |
| 7 | `cargo test --lib formula_integration_tests -- --test-threads=1`（`RAPID_OCR_MODEL_ROOT` / `RAPID_OCR_FORMULA_TEST_ROOT` 已设置） | **11 passed, 0 failed**（61.62 s；日志里 `skipping test` 出现 **0** 次 → 真的加载了真实模型与真实页面） | 0 |
| 8 | `cargo tree -e normal -p rapid-ocr-rs`（默认 / `--features serve`） | 各 606 行，SHA-256 **相同**（`BD2AB5E4…C3F6FC`，与 M0c 记录的哈希**逐位相同**）；`tiny_http` 出现 0 次 | 0 |

**基线对比（AGENTS.md §6：修改前建立基线 → 实施 → 验证）**

| 项目 | 修改前（`1144ddb`） | 修改后 |
| --- | --- | --- |
| `cargo test --all-targets` | 354 + 0 = **354 passed** | **389 passed**（+35：`model_store` +34、`test_support` +1） |
| `cargo test --features serve --all-targets` | **459 passed** | **495 passed**（+36：再加 `serve::limits` +1） |
| 删除/跳过/弱化的测试 | — | **0**。修改的既有测试只有 `serve::error` 的两张表（`DownloadError` 的变体名随库类型改名，**断言逐项不变**，并新增了一条 `ReadTimeout → 504 download_timeout` 的映射断言），以及 `transform` 前就存在的 `sha256_file` 测试（原样保留） |
| 默认构建依赖图 | 606 行 / `BD2AB5E4…C3F6FC` | **逐位相同**（`MoveFileExW` / `GetDiskFreeSpaceExW` 是 raw Win32 绑定，**未新增任何依赖**） |

**测试数量与文件规模**：`src/model_store.rs` 35 个测试、`src/test_support.rs` 1 个测试、
`src/bin/serve/limits.rs` 7 个测试、`src/bin/serve/error.rs` 7 个测试。
修复 fixture 的非阻塞缺陷之后，`cargo test --all-targets` 又连续跑了 **5** 次
（含一次 3 连跑），全部 0 failed。

---

### 12 图硬门槛

本阶段**重跑了两个门槛**，输出写在 `target/m0b-gate/`（**没有覆盖** `tests/baseline/`，
`git status --porcelain -- tests/baseline` 为空），比较方式是原始 JSON 里的**数字字面量**
精确字符串比较（不是浮点近似）：

| 门槛 | 文档要求 | 本次实测（release） | 已提交基线 | 结论 |
| --- | --- | --- | --- | --- |
| 12 图 mean CER | `0.44765135645866394` | **`0.44765135645866394`**（12 例） | `0.44765135645866394` | 字面量逐位相同 |
| 12 图区域数均值 | `34.833333333333336` | **`34.833333333333336`**（12 图 × 3 轮 = 36 样本） | `34.833333333333336` | 字面量逐位相同 |

命令（**与 §13 的 CLI 对照一致**）：

```powershell
target\release\bench_warm_e2e.exe --config <OCR-Model>\test-config-small.yaml `
  --images-dir <OCR-test-image> --warmup-rounds 1 --rounds 3 --max-side-len 2000 `
  --intra-threads 16 --output target\m0b-gate\bench-cpu-2000.json
target\release\rapidocr.exe evaluate --manifest <OCR-test-image>\golden-manifest.json `
  --config <OCR-Model>\test-config-small.yaml --output target\m0b-gate\evaluation-cpu.json
```

为什么**必须**重跑：本阶段改了 `resolve_character_path`（`recognizer.rs`）与
`resolve_model_path`，虽然 12 图基准与公式集成测试都通过配置显式给出
`model_path` / `rec_keys_path`（不进下载分支），但"改了识别路径"这一点必须用**实测**
而不是推理来排除影响。结果与硬门槛逐位相同。

---

### 关键行为对比（AGENTS.md §9）

| 项目 | 修改前（`ensure_downloaded`） | 修改后（`download_verified`） | 预期结果 |
| --- | --- | --- | --- |
| 哈希 | `Option<&str>`，`None` 时**下载但不校验** | `&str` 必填；空串在 I/O 前拒绝 | §6.4 的硬要求：不允许无校验下载 |
| 重定向 | 默认 client **自动跟随**（可被跳到任意 host） | `Policy::none()`；3xx → `RedirectRejected`（**M2b**：手工逐跳，白名单内最多 5 跳） | §6.1 第 2 条 |
| host | 无白名单概念 | 编译期常量白名单，清单/URL 不能扩大 | §6.1 第 3 条（OWASP） |
| 体积 | 无上限、无长度预检 | `Content-Length` 预检 + `take(max+1)` 流式上限 | §6.1 第 4、5 条 |
| 临时文件 | `target.with_extension("part")`（**固定名**）；失败路径上残留 | `.part-<pid>-<seq>`（唯一）+ `PartFile` 的 `Drop` 保证删除 | 并发/崩溃残留不再互撞，且"可疑文件"不会留下 |
| 覆盖 | `remove_file(target)` 后 `fs::rename`（崩溃窗口） | `MoveFileExW(MOVEFILE_REPLACE_EXISTING \| MOVEFILE_WRITE_THROUGH)`；失败保留原文件 | §6.7 |
| 并发 | 无锁 | 目标路径单飞 | §6.1 第 8 条 |
| 磁盘空间 | 无检查（写满才报 io 错误） | `GetDiskFreeSpaceExW` 预检 → `InsufficientSpace`（可注入，可单测） | §6.1 第 10 条 |
| 超时 | 单一 60 s 总超时 | 连接 / 读取分项（10 s / 30 s），错误分类区分 | §6.1 第 11 条 |
| 错误表示 | `RapidOcrError::Download(String)`（serve 侧另有一套 `DownloadError`） | 库内唯一的 `DownloadError` 十二类；`RapidOcrError::Download(DownloadError)`；serve 只做 HTTP 映射 | 同一失败原因不可能有两种表示 |
| `RapidOcrError::Download` 在 OCR 路径上的映射 | 一律 502 `download_failed` / kind `"download"` | 原样沿用下载分类（超时 → 504 `download_timeout`） | 更精确，且不新增 `code` |
| 性能 | — | 无热路径改动；单文件下载多了一次"连接预算/读取预算"的构造与一次磁盘查询（微秒级）；12 图门槛逐位相同 | 无退化 |

---

### 未覆盖风险与**做不到的事**（如实记录）

1. **真实的连接阶段超时无法在环回上端到端复现**。实测：环回 TCP 连接约 0.4 ms 完成，
   而 reqwest/tokio 的定时器粒度是 1 ms 量级——把 `connect_timeout` 压到 1 ns，
   连接仍然先完成，最终失败的是"等待响应"的读取预算（`is_timeout()==true`、
   `is_connect()==false`、耗时 ≈ 读取预算）。真实连接错误（拒绝连接）则是
   `is_connect()==true`、`is_timeout()==false`。因此 `ConnectTimeout` 这一分支是通过
   **判定函数** `classify_send_timeout` 验证的（喂入**真实的** reqwest 超时错误 +
   连接阶段应有的耗时），而 `ReadTimeout` 有两条端到端测试；我用的是一个绑定了端口但
   从不 accept 的监听者做过对照实验（结果同上：报的是读取预算）。**"真实网络下的
   连接超时"没有被端到端验证**，这是本阶段明确未覆盖的一点。
2. **TLS/HTTPS 本身没有被 fixture 覆盖**：fixture 是明文 HTTP（无证书依赖），因此
   "真实 TLS 握手 + 证书校验"这条路径只在生产构建里存在、没有被自动化测试执行。
   生产入口的作用域（仅 https + 编译期白名单）由公开入口测试覆盖；测试策略里
   `insecure_http` 字段是 `#[cfg(test)]`，**生产构建里不存在**。
3. **磁盘空间预检在"长度未知"时是保守的**：按 §6.5 用 `max_bytes` 计入需求。默认
   1024 MiB 的上限下，若磁盘剩余空间介于"实际文件大小"与"1024 MiB"之间，一个长度
   未知的小文件会被提前拒绝（`InsufficientSpace`）。这是有意选择（宁可提前拒绝也不写
   半个文件），但**能构造出误拒**；M2 侧可以用已知的 `size_bytes` 或更小的剩余额度
   收紧它。
4. **`read_timeout` 同时约束"等待响应头"**：reqwest 的 blocking `timeout()` 是"每次
   阻塞等待"的超时（不是整批下载的总时长，因此 600 MB 的持续推进不会超时），但它也
   覆盖等待响应头的阶段。这一点已在代码注释与本文档写明，未做进一步拆分（那需要
   自建 HTTP 栈）。
5. **`DownloadError::Cancelled` 只有类型，没有生产者**：§6.6 的"文件边界取消"属于
   M2 的下载 worker；本阶段只固定它的分类与 HTTP 映射（409 `download_cancelled`）。
6. **serve 的下载 worker / `--allow-download-host` 接线未做**（M2）：`--allow-download-host`
   是"用户显式扩大白名单"的入口，本阶段的库函数**只认编译期常量**，因此即使传了该参数，
   库也不会放宽（这正是 §6.1 第 3 条要的默认行为）；M2 需要把 `allowed_hosts` 作为
   **显式参数**接进来（并与启动警告绑定），那是新增的 API，不在本阶段。
7. **`http://` 明文 fixture 与 `DownloadPolicy` 的 `#[cfg(test)]` 口子**：单测覆盖的是
   生产代码的传输/落盘/预算/单飞路径，但**不是**生产策略的 scheme/host 判定分支
   （那两个分支由公开入口测试覆盖）。两者合起来覆盖全部要求，但没有一条测试同时
   经过"https + 编译期白名单 + 真实传输"（需要真实证书/真实站点）。
8. **未提交、未在干净 clone 上验证**：按要求不 commit；证据来自当前工作树。
   `git diff` 只包含上表列出的改动（`git diff --numstat`：#文件行数改动量与
   "注释中提到的旧函数名"一致，没有整文件行尾翻转）。
9. **行尾**：本机 `core.autocrlf=true` 且工作树本来就混用 LF/CRLF（84 LF / 47 CRLF）。
   本阶段新写/改写的若干文件在工作树里是 LF，git 因此在 `git diff` 时提示
   "LF will be replaced by CRLF"；仓库里存的是 LF，且改动行数与预期一致（例如
   `src/error.rs` 只有 6 增 2 删 / 58 行），不存在整文件行尾改写。
10. **M2 的端到端"点击下载 → 进度 → 模型齐备"未验证**：本阶段没有任何 HTTP 端点，
    因此 §12 里"下载"那一行（重定向拒绝、非 https 拒绝、白名单拒绝、长度超限、
    流式超限、哈希失败、单飞、磁盘不足 507、损坏重下）在本阶段是**库级**验证，
    507/502/504 这些**状态码**仍只有 M0c 的映射单元测试，没有经过真实响应。

---

### 与 `docs/05` §11「M0」验收清单的对照（M0b 范围内的条目）

| §11 M0 条目 | 本阶段 | 证据 |
| --- | --- | --- |
| 加固下载器 + `MoveFileExW` 原子替换（§6） | ✅ 完成 | 交付物 1：12 条要求逐条有测试；**36 个新测试**（`model_store` +34、`test_support` +1、`limits` +1） |
| 迁移 CLI 调用方、删除可传 `None` 的入口（§6.4） | ✅ 完成 | 交付物 2：4 个调用点迁移；`ensure_downloaded` 与 serve 侧重复的 `DownloadError` 均已删除且无兼容层 |
| `--max-download-mb`（§6.2） | ✅ 完成 | 交付物 3：默认 1024（单一来源）、0 值启动期拒绝、单文件 = 总量额度、剩余额度递减、未知体积仍受限 |
| `DownloadError` 十二类 + serve 复用同一类型（§6.1 第 12 条） | ✅ 完成 | `every_download_error_class_has_a_stable_kind`、`every_download_error_maps_to_a_documented_status_and_code`、`every_rapid_ocr_error_variant_is_mapped` |
| 「测试不得依赖公网」 | ✅ 完成 | 交付物 4：fixture 只在 `127.0.0.1:0`；联网测试 0 条（`a_refused_connection_is_a_network_error` 用的是本机已释放端口） |
| **M0 验收**：以上每项都有单元测试；`cargo test` 全绿；文档与实现一致 | ✅ 本阶段范围内成立 | 389 / 495 passed，0 failed；两个硬门槛逐位相同；本文件 + `docs/05` §13 已同步 |
| §6.6 下载取消、M1/M2 的 HTTP 与 worker | ⛔ 不在 M0b | 见"未覆盖风险"第 5、6、10 条 |

---

## M1：最小闭环（HTTP 层 + `serve` 子命令 + 内联页面 + 12 图经 HTTP 与 CLI 逐张一致）

**阶段**：M1 —— `docs/05` §11「M1」的全部条目 + §3/§4/§7/§8/§9/§10 里与本里程碑相关的冻结契约。
**开工基线**：`dbab12e`（M0a `1144ddb` + M0b `dbab12e` 已提交：模型清单/单一来源、加固下载器、
serve 纯逻辑 105 测试、**无 HTTP**）。
**日期**：2026-10-03（同一天，接在 M0c 记录之后）
**提交**：`（未提交：按要求不 commit）`

### 环境

| 项目 | 值 |
| --- | --- |
| target | Windows x64 + MSVC ABI：`x86_64-pc-windows-msvc` |
| 工具链 | rustc 1.98.1 / cargo 1.98.1 |
| crate | `rapid-ocr-rs` 0.7.0（独立 git 仓库，本阶段未提交） |
| 真实资产 | `OCR-Model/small/`（PP-OCRv6 det/rec small + dict）、`OCR-test-image/`（12 图 + `golden-manifest.json`）、`OCR-Model/test-config-small.yaml` |
| HTTP 依赖 | `tiny_http = { version = "0.12", optional = true }`（`serve = ["dep:tiny_http"]`，**不进 default**） |

---

### 交付物 1：HTTP 层与 `serve` 子命令

**新增/改写的文件**（全部在二进制侧，库零改动）：

```text
src/bin/serve/model_plan.rs   459 行   6 tests  模型集单一来源解析 + 逐文件状态 + 引擎路径绑定（§5.3/§5.4/§7.6）
src/bin/serve/engine.rs        95 行   1 test   OcrBackend + 引擎工厂（生产路径 = RapidOcrEngine）
src/bin/serve/results.rs      203 行   3 tests  有界结果存储 + 有界序列化（§4.5/§4.6）
src/bin/serve/download.rs      65 行   0 test   独立下载 worker（M2 的处理体；M1 如实判失败）
src/bin/serve/server.rs       951 行   4 tests  运行期核心：共享状态/双队列/worker/TTL 清理/端点语义
src/bin/serve/http.rs         652 行   5 tests  tiny_http 接线：路由/准入/响应头/端点分发
src/bin/serve/run.rs          433 行   6 tests  启动编排（绑定→校验→模型→页面）、注入契约、启动日志
src/bin/serve/tests.rs       1299 行  24 tests  真实绑定端口的端到端测试（原始 TCP 客户端）
src/bin/web/index.html       2024 行   （数据）  Temp/demo3-v2.html 的副本 + 1 处最小改动
                             ────────────────
                             4157 行  51 tests（含 2 个依赖边界测试）
```

**改动既有文件（最小必要面）**：

| 文件 | 改动 | 理由 |
| --- | --- | --- |
| `Cargo.toml` | `tiny_http` optional + `serve = ["dep:tiny_http"]` | §2.1：HTTP 只在 `serve` feature 里，且不在 `default` |
| `src/bin/rapidocr.rs` | `Command::Serve`（启用 feature 时用真实 `ServeArgs`，未启用时给出"重建提示"的可定位错误） | §3 |
| `src/bin/serve/mod.rs` | 挂上 7 个新模块；**删除 M0c 的 `#![allow(dead_code)]`**；`scope_tests` 替换为 `dependency_boundary`（2 个测试） | 交付物 4 |
| `src/bin/serve/{admit,error,jobs,limits,queue,state}.rs` | 逐项接上真实调用点；删除/接线的具体项见"交付物 4" | 交付物 4 |

**线程模型（§8.1，与文档逐条对应）**：

```text
serve-accept（1 个，具名线程；主线程 join 它，退出用 Server::unblock()）
  ├── recv_timeout(200 ms) + 关闭标志
  ├── 静态/校验类请求就地处理（路由/准入/状态/模型/任务查询）
  ├── POST /api/ocr：准入 → 有界读取 → 建任务 → 双队列（满 → 503，不阻塞）
  └── POST /api/models/download：校验 → 有界 channel（容量 4）→ 满则 503 并回收任务
serve-ocr（**恰好 1 个**：引擎 &mut self，§8.2）
serve-download（1 个：M2 的处理体；M1 收到命令后 Queued→Running→Failed 并写明"未实现"）
serve-sweeper（1 个：JobStore::tick + 结果/原图与任务存储同步清理，500 ms 一轮）
```

推理**绝不**在 accept 线程上：`POST /api/ocr` 只做准入、读体与入队；两把锁
（`jobs` 与 `engine`）从不同时持有，`/api/status` 只读状态机，不会因一次推理而阻塞。

**端点（M1 子集，逐条对照 §4.2）**：

| 端点 | 行为 | 关键证据 |
| --- | --- | --- |
| `GET /` | 内联页面（启动时注入 nonce/token）+ nonce CSP | `the_page_carries_the_nonce_csp_and_the_frozen_security_headers` |
| `GET /api/status` | 服务/引擎/provider 三字段/ORT 指纹/队列/保留/TTL/上限，模型目录脱敏 | `status_reports_the_frozen_three_provider_fields_and_redacts_paths` |
| `GET /api/models` | 每个集合的逐文件状态（扁平 `state` 字符串）、`missing`/`corrupt`/`blocked` | `models_reports_every_file_state_and_matches_the_ocr_409` |
| `POST /api/ocr` | **202** `{job_id, kind, queue, position, state}`；`?max_side=` / `?queue=` | `the_job_goes_from_202_to_succeeded_and_its_result_round_trips` |
| `GET /api/jobs/{id}` | `JobView` 全字段（position 由调度器实时计算） | 同上 + `cancelling_a_queued_job_succeeds_and_a_running_job_is_409` |
| `GET /api/jobs/{id}/result` | 成功 → 已序列化结果；失败 → **重放原始状态码/错误体**；未完成 → 409 | `a_failed_job_replays_its_original_status_and_code_on_result` |
| `POST /api/jobs/{id}/cancel` | 排队 → 200 `cancelled`；运行中/终态 → 409 `not_cancellable` | `cancelling_a_queued_job_succeeds_and_a_running_job_is_409` |
| `POST /api/models/download` | 未开 `--allow-download` → 403 `downloads_disabled`；开启 → 建真实任务并由 M2 接缝如实判失败 | `the_download_endpoint_degrades_visibly` |

**结果序列化（§4.6）**：`to_output_json` + 有界写入器（累计字节超限即中止，**不**先建大
`String`），超限 → 413 `result_too_large`；`/result` 只把已保存的字节原样写出。
响应里额外给出 `plain_text`（= 库的 `text`，`plain_text(TextOrder::Reading)`），
因为内联页面的"复制全文"（§9.2）读的就是这个名字。

### 交付物 2：安全与响应头（按冻结契约，无一处放宽）

| 要求 | 实现 | 证据 |
| --- | --- | --- |
| 监听地址硬编码 | `security::bind_address(port)`（签名里没有地址参数）+ `assert_loopback` 启动断言；日志打印实际地址与允许集合 | M0c 测试 + `run.rs` 的端口一致性检查 |
| `Host` → 421 | `LocalOrigin::check_host` | `the_host_header_is_validated_for_dns_rebinding`（含 `[::1]`、端口不符、无 Host） |
| `Origin` → 403（仅状态改变方法） | `check_origin` | `state_changing_requests_require_a_matching_origin`（缺失 / `null` / 不匹配）+ `read_requests_do_not_require_an_origin` |
| token（所有 `/api/*`） | `X-RapidOCR-Token` + 常量时间比较；`GET /` 是令牌下发者，**不**要求令牌 | `the_token_is_required_on_every_api_route`、`the_page_carries_...` |
| 不发送 `Access-Control-Allow-Origin` | 没有任何 CORS 头 | 页面测试逐项断言 |
| 三个安全头 | `SECURITY_HEADERS` 在**每个**响应上（含 401/403/404/405/409/413/421/503） | 同上 + 各状态码测试 |
| nonce CSP（无 `unsafe-inline`） | `GET /` 的 `Content-Security-Policy`，nonce 与页面里的 3 个 `nonce=` 属性**逐字节相同** | `the_main_page_csp_is_nonce_based_and_never_allows_inline`、`the_real_page_injects_cleanly_and_matches_the_csp_nonce` |
| 路径脱敏（§7.4/§10.9） | `/api/status`、`/api/models` 的 `model_dir` 一律 `<redacted>`；ORT 指纹只给文件名+体积+SHA-256（剥掉 `LoadedModule.path`） | `status_reports_..._and_redacts_paths`（断言绝对路径一次都不出现） |

**准入顺序（§4.4）**：路由 → token → Host/Origin → **队列容量（未读 body）** → `Content-Length`
→ 有界读取（字节上限 + 截止时间）→ 建任务。`an_over_long_content_length_is_rejected_before_the_body_is_read`
用一个"声明 4 MiB、一个字节都不发"的请求证明 413 发生在读取之前（否则该请求会一直等到超时）。

### 交付物 3：内联页面（原型零改写，1 处最小改动）

- `Temp/demo3-v2.html` → `src/bin/web/index.html`（SHA-256 一致，`Temp/` 原型**未被触碰**：
  `git status --porcelain -- Temp/demo3-v2.html` 为空）；
- 占位符契约严格照做：`__CSP_NONCE__` ×4、`__SRV_TOKEN__` ×3（页面自己的契约注释说明了
  这两个数字的算法）；注入后三项断言，任一失败即**拒绝启动**：
  1. `assert_no_placeholders_left`（M0c 的严格残留检查）；
  2. 每个 `nonce="…"` 属性与 CSP 头的 nonce 逐字节相同（HTML 注释里的同形文字不算属性——
     页面顶部注释逐字写了 `nonce="…"`，这是它自己在描述这条规则）；
  3. token 真的出现在页面里（否则页面会静默退化成"离线预览模式"，永远不访问服务）。

**对页面做的唯一改动**（2 行 + 5 行注释，逻辑一行未动）：

```diff
 const ENG_MAP = {
   ...
   waiting_models:['cloud-off','等待模型就绪','warn'],
+  /* 服务端（src/bin/serve/state.rs 的 EngineState）在模型不齐备时报的是
+     blocked_models_missing（docs/05 §7.6 的枚举名）；离线预览模式报 waiting_models。
+     两者是同一件事，展示必须一致，否则真实模式下会退化成"状态未知"。 */
+  blocked_models_missing:['cloud-off','等待模型就绪','warn'],
   unknown:       ['info','状态未知','dim']
 };
-  const head = (tq.ready && s.state === 'waiting_models')
+  const waiting = s.state === 'waiting_models' || s.state === 'blocked_models_missing';
+  const head = (tq.ready && waiting)
```

原因：服务端的 `EngineState`（§7.6 冻结的枚举名）是 `blocked_models_missing`，而页面只认识
演示模式用的 `waiting_models`；不改的话真实模式下会显示"状态未知"。**服务端没有改名去迁就
页面**（§7.6 的枚举名是文档冻结的），改的是页面这一处映射。

**页面对"尚未实现的端点"的降级（如实记录）**：

| 页面调用 | M1 的实现 | 页面表现 |
| --- | --- | --- |
| `POST /api/models/download` | 403 `downloads_disabled`（未开 `--allow-download`）；开启时创建真实任务后失败并写明"未实现、无网络 I/O" | toast 显示 `downloads_disabled` 文案或失败原因；横幅提示"服务未以 --allow-download 启动" |
| `GET /api/jobs/{id}/annotated.png` | 404 `not_found`（M3） | `apiBlob` 拒绝 → toast"请求失败（HTTP 404）"，页面不崩 |
| `GET /api/jobs/{id}/export?format=` | 404 `not_found`（M3） | 同上 |
| 公式路由开关 | M1 服务端固定文本路由（§10.8）；页面本来就不发送这个开关 | 公式队列显示"不可用（不影响普通 OCR）"，文本队列正常 |
| `plain_text` | 已提供（见交付物 1） | 复制全文走服务端的阅读顺序文本 |

### 交付物 4：删除 M0 脚手架，接上 M0c 的每一处接缝

- **`#![allow(dead_code)]` 已删除**。逐项处置：
  - 接上真实调用点（不再是"只被测试用"）：`HttpMethod::parse`、`AdmissionError::Route` 的三个字段、
    `AdmissionError::rejected`、`BodyBudget::{max_body,received}`（超限时打印账本口径）、
    `JobStoreLimits::new`（`from_limits` 改为调用它，校验只有一份实现）、
    `SchedulerConfig::new`（`from_limits` 同上）、`JobKind::name` / `JobState::name`
    （202/任务视图里的 `kind`/`state` 不再写字符串字面量）、
    `DualQueueScheduler::{round_len,served_in_round,is_empty}`（进 `/api/status` 的队列诊断）、
    `JobStore::is_empty`（清理线程的早退）、`ServeConfigError::{field,reason}`（`Display` 改为调用它们）、
    `LocalOrigin::port`（允许集合与实际绑定端口的一致性检查）、`ServeLimits::download_budget`
    （下载端点的 `--max-download-mb` 预检，§6.2）、`JobStore::set_position`（由调度器计算 position）；
  - 删除（M1 不再需要）：`ModelPlan::{model_dir,sets}`、`ModelSnapshot::source`、
    `ServeShared::{model_dir,queue_wait_bound}`、`serve::run::resolved_model_dir`、
    M0c 的 `scope_tests`（由更强的 `dependency_boundary` 取代）；
  - **保留但逐项 `#[allow(dead_code)]`（带理由）**：`ServeError::{ExportTooLarge, InsufficientDiskSpace, UnsupportedInput}`
    （§11.1 冻结的协议变体，生产者分别在 M2/M3）、`EngineState::Rebuilding` +
    `EngineStateMachine::{begin_loading,begin_rebuild,models_still_missing}`（M3 的
    `POST /api/engine/reload`）。这些项都有 M0c 的转换测试，删掉会让 §7.6/§11.1 的冻结契约
    失去覆盖；`allow` 是**逐项**的，与原来覆盖整个子树的 `#![allow(dead_code)]` 不是一回事。
- **`ModelReadiness` 由 `ModelSetStatus` 填充**（M0c 接缝 1）：`ModelPlan::resolve` 用库的
  `ModelSource`（单一来源规则）+ `ModelRequest::text_only`（与 `--config` 一致的管线选择），
  启动快照的 `blocked` 清单直接来自共享的 `validate_model_files`。
- **§7.6 的 OCR 409 体形状（M0c 接缝 4）已决定并统一**：
  `code` = `models_missing`，若有损坏文件则 `models_corrupt`（§5.2/§11.1 要求可区分）；
  清单进 `detail`，字段名与值**复用** `/api/models` 的同一份计算：
  `detail.{missing,corrupt,blocked,source,model_dir}` 与 `/api/models` 的同名字段**逐字节相同**
  （`blocked` = 缺失 ∪ 损坏，也等于 `EngineState::BlockedModelsMissing.missing`）。
- **路径脱敏（M0c 接缝 5）**：`/api/status` 不再直接序列化库的 ORT 指纹（它带绝对路径），
  改为只给文件名 + 体积 + SHA-256 + provider DLL 名单。
- **读取超时的实体（M0c 接缝 6）**：用 `Instant` 截止时间在**每次尝试读取前**判定；已知边界见"未覆盖风险"。

### 交付物 5：验证

#### 5.1 静态检查与 feature 矩阵（最终树，日志在 `target/m1-verify/`）

| # | 命令 | 结果 | 退出码 |
| --- | --- | --- | --- |
| 1 | `cargo fmt --all -- --check` | 无输出 | 0 |
| 2 | `cargo clippy --all-targets -- -D warnings` | 无 warning | 0 |
| 3 | `cargo clippy --features serve --all-targets -- -D warnings` | 无 warning（**含 M0c 的 105 个测试与新代码**） | 0 |
| 4 | `cargo test --all-targets` | 369 + 2 + 4 + 14 = **389 passed, 0 failed** | 0 |
| 5 | `cargo test --features serve --all-targets` | 389 + **156** = **545 passed, 0 failed** | 0 |
| 6 | `cargo build --release --bins` | `Finished release profile [optimized] in 7.98s`；`rapidocr.exe` 34,446,336 B | 0 |
| 7 | `cargo build --release --bins --features serve` | `Finished release profile [optimized] in 8.31s`（同一二进制即可跑 serve） | 0 |

**默认依赖图逐字节未变**（§2.1 的硬约束）：

| 证据 | 值 |
| --- | --- |
| `cargo tree -e normal -p rapid-ocr-rs`（M1 开工前） | 606 行，SHA-256 `EB00BBA0C242E09DE4E7631662634596F3D5325778D32B32213E70981A8EB7B1` |
| 同上（M1 完成后） | 606 行，SHA-256 **完全相同** |
| 默认树里的 `tiny_http` 出现次数 | **0** |
| `cargo tree -e normal --no-default-features` | 605 行，`tiny_http` **0** 次 |
| `cargo tree … --features serve` | 611 行，`tiny_http v0.12.0`（+ `ascii`/`chunked_transfer`/`httpdate`） |
| 源码级边界 | `dependency_boundary` 两个测试：库（`src/` 除 `src/bin`）不得引用 HTTP 库；`serve/` 子树里只有 `http.rs` 可以；`Cargo.toml` 必须 `optional = true`、不在 `default`、且只由 `serve` 用 `dep:` 引入 |

`Cargo.lock` 有改动（记录所有 feature 的依赖并集），这是 lock 文件的职责；**默认 feature 的依赖图**才是
§2.1 约束的对象，它是逐字节相同的。

#### 5.2 真实绑定端口的端到端测试（`src/bin/serve/tests.rs`：24 个测试，原始 TCP 客户端）

| 验收项 | 测试 | 结果 |
| --- | --- | --- |
| `/`、`/api/status`、`/api/models`、`202 → queued → running → succeeded → /result` | `the_job_goes_from_202_to_succeeded_and_its_result_round_trips`、`the_page_carries_...`、`status_reports_...`、`models_reports_...` | ✅ |
| 401（缺 token / 错 token） | `the_token_is_required_on_every_api_route` | ✅ |
| 403（缺 / `null` / 不匹配 `Origin`，仅 POST） | `state_changing_requests_require_a_matching_origin`、`read_requests_do_not_require_an_origin` | ✅ |
| 421（错 Host / 无 Host / `[::1]`） | `the_host_header_is_validated_for_dns_rebinding` | ✅ |
| 503（队列满，且**未读 body**） | `a_full_queue_is_503_without_reading_the_body` | ✅ |
| 404 / 410（淘汰后可区分；TTL 后回到 404） | `evicted_jobs_are_410_and_after_the_ttl_they_become_404` | ✅ |
| 409（取消运行中的任务，状态不变） | `cancelling_a_queued_job_succeeds_and_a_running_job_is_409` | ✅ |
| 413（`Content-Length` 预检，声明后不发） | `an_over_long_content_length_is_rejected_before_the_body_is_read` | ✅ |
| 413 `result_too_large`（有界序列化中止） | `a_result_over_the_limit_is_a_413_result_too_large` | ✅ |
| 404/405（M3/M4 端点不存在；`Allow` 头） | `unknown_paths_are_404_and_wrong_methods_are_405` | ✅ |
| 媒体类型（非 octet-stream）与 `?max_side=` 校验 | `the_ocr_media_type_is_validated`、`the_max_side_query_parameter_is_validated` | ✅ |
| 安全头齐备 / 无 CORS / nonce CSP 无 `unsafe-inline` | `the_page_carries_the_nonce_csp_and_the_frozen_security_headers` | ✅ |
| 占位符残留 → **拒绝启动** | `run::tests::a_residual_placeholder_refuses_to_start`（病理输入：token 值里含另一个占位符字面量）、`a_foreign_nonce_refuses_to_start`、`a_page_without_a_nonce_or_a_token_refuses_to_start` | ✅ |
| 引擎 `Failed` → `/api/status` 带 reason，OCR 503 `engine_unavailable` | `an_engine_that_fails_to_load_is_failed_with_a_reason_and_503` | ✅ |
| 空模型目录：服务 Ready + `BlockedModelsMissing` + 409 字段与 `/api/models` 一致 | `models_reports_every_file_state_and_matches_the_ocr_409` | ✅ |

#### 5.3 端到端：12 张真实图片经 HTTP 与 CLI 逐张对照（`target/m1-e2e/`）

```powershell
# 服务（release 二进制，真实模型目录 + 真实配置）
target\release\rapidocr.exe serve --port 8791 `
  --model-dir D:\100_Projects\110_Daily\SnapClip\OCR-Model\small `
  --config D:\100_Projects\110_Daily\SnapClip\OCR-Model\test-config-small.yaml
# 每张图：POST /api/ocr（application/octet-stream，带 X-RapidOCR-Token 与 Origin）→
#         轮询 GET /api/jobs/{id} → GET /api/jobs/{id}/result
# 对照：target\release\rapidocr.exe run --img-path <同一张图> --config <同一配置> --json
```

| # | 图片 | HTTP `regions` | CLI `regions` | 区域数相同 | `text` 逐字节相同 | 逐区域文本序列相同 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 01基础多位置文本.png | 42 | 42 | ✅ | ✅ | ✅ |
| 2 | 02多语言与RTL混排.png | 21 | 21 | ✅ | ✅ | ✅ |
| 3 | 03旋转与倾斜.png | 13 | 13 | ✅ | ✅ | ✅ |
| 4 | 04表格与键值对.png | 61 | 61 | ✅ | ✅ | ✅ |
| 5 | 05代码与等宽字体.png | 38 | 38 | ✅ | ✅ | ✅ |
| 6 | 06低对比度与深色背景.png | 21 | 21 | ✅ | ✅ | ✅ |
| 7 | 07小字号与密集排版.png | 37 | 37 | ✅ | ✅ | ✅ |
| 8 | 08数字公式与符号.png | 51 | 51 | ✅ | ✅ | ✅ |
| 9 | 09竖排文本.png | 14 | 14 | ✅ | ✅ | ✅ |
| 10 | 10长段落与分栏.png | 37 | 37 | ✅ | ✅ | ✅ |
| 11 | 11文字样式与特效.png | 22 | 22 | ✅ | ✅ | ✅ |
| 12 | 12综合压力测试.png | 61 | 61 | ✅ | ✅ | ✅ |
| — | **合计** | **418** | **418** | **12/12** | **12/12** | **12/12** |

`ALL_12_MATCH=True`（脚本判据：逐张 regions 数、`text`、逐区域文本序列三者全等）。
每张图的完整响应与 CLI 输出分别保存在 `target/m1-e2e/serve-<name>.json` 与 `cli-<name>.json`。

#### 5.4 空模型目录

```text
service.state=ready            engine.state=blocked_models_missing
model_dir=<redacted>           source=default_table
models.missing=[PP-OCRv6_det_small.onnx, PP-OCRv6_rec_small.onnx, ppocrv6_dict.txt]
POST /api/ocr → http=409 code=models_missing
  detail.missing=[PP-OCRv6_det_small.onnx, PP-OCRv6_rec_small.onnx, ppocrv6_dict.txt]
  detail.corrupt=[]  detail.source=default_table  detail.model_dir=<redacted>
FIELDS_MATCH_API_MODELS=True   （missing/blocked/source/model_dir 与 /api/models 逐字节相同）
```

#### 5.5 双向公平性（真实 HTTP 边界）

M1 的**生产**路由固定为文本队列（§10.8：公式路由默认关闭，页面也不发送这个开关），
因此"公式洪水"不能靠真实引擎在 HTTP 上产生。做法（**测试专用慢速路径，如实说明**）：

1. 用 `ServeContext::engine_factory` 注入一个**脚本化后端**（`Scripted`：可设 `delay` /
   区域数 / 失败 / 是否记录调用），它替代真实引擎，但**队列、准入、调度、任务生命周期全是真实代码**；
2. 公式队列通过 `OcrRouting { formula: true }`（M4 才会接上真实来源）+ `POST /api/ocr?queue=formula`
   进入——生产路径下这个取值会被 400 `bad_request` 拒绝（`the_formula_queue_is_refused_when_formula_routing_is_off`）；
3. 每个方向的上界取自 **`/api/status` 的公开字段** `queues.<class>.wait_bound`
   （= `capacity × 对方配额`，§8.3 的可证明上界），再乘单任务耗时并留余量；
4. 测试先**证明目标队列真的被灌满**（`used >= capacity`），再提交另一队列的任务并计时，
   最后断言计时窗口内洪水队列确实被服务过（否则"没被饿死"没有意义）。

| 方向 | 参数 | 上界 | 结果 |
| --- | --- | --- | --- |
| 公式洪水（容量 2）下普通任务 | `delay=25 ms`，text 容量 4 / formula 容量 2，连续配额 4/1 | `wait_bound(text)=4` → `(4+3)×25 ms + 750 ms = 925 ms` | ✅ 未被饿死 |
| 普通洪水（容量 4）下公式任务 | 同上 | `wait_bound(formula)=8` → `(8+3)×25 ms + 750 ms = 1025 ms` | ✅ 未被饿死 |

#### 5.6 12 图硬门槛（`target/m1-gate/`，**没有**覆盖 `tests/baseline/`）

```powershell
target\release\bench_warm_e2e.exe --config <OCR-Model>\test-config-small.yaml `
  --images-dir <OCR-test-image> --warmup-rounds 1 --rounds 3 --max-side-len 2000 `
  --intra-threads 16 --output target\m1-gate\bench-cpu-2000.json
target\release\rapidocr.exe evaluate --manifest <OCR-test-image>\golden-manifest.json `
  --config <OCR-Model>\test-config-small.yaml --output target\m1-gate\evaluation-cpu.json
```

| 门槛 | 文档要求 | 本次实测（release） | 比较方式 | 结论 |
| --- | --- | --- | --- | --- |
| 12 图区域数均值 | `34.833333333333336` | **`34.833333333333336`**（36 样本） | 原始 JSON 的**数字字面量**字符串精确比较 | 逐位相同 |
| 12 图 mean CER | `0.44765135645866394` | **`0.44765135645866394`**（12 例） | 同上 | 逐位相同 |

`git status --porcelain -- tests/baseline` 为空；`Temp/demo3-v2.html` 也未被改动。

---

### 关键行为对比（AGENTS.md §9）

| 项目 | 修改前（M0 基线） | 修改后 | 预期结果 |
| --- | --- | --- | --- |
| `serve` feature | 空 feature（`serve = []`），无 HTTP | `serve = ["dep:tiny_http"]`（optional，不进 default）；默认依赖图逐字节不变 | §2.1 |
| `rapidocr serve` | 子命令**不存在**（无法解析） | 子命令可解析；未启用 feature 时给出"重建提示"的可定位错误 | §3 |
| HTTP 端到端 | 完全没有（M0c 记录明说"一条都没跑"） | 24 个真实端口测试 + 12 张真实图片经 HTTP 与 CLI 逐张一致 | §12 |
| OCR 409 体形状 | 保留为 M0c 接缝（只有 `code`，无清单） | `code`（`models_missing`/`models_corrupt`）+ `detail` 与 `/api/models` 同源同值 | §7.6 |
| `ModelReadiness` | 本地接缝类型，没有生产者 | 由 `ModelSource`/`ModelSetStatus`（库的唯一实现）填充 | §5.2/§7.6 |
| 引擎的模型路径 | 由 `--config` 的 `model_path` 决定（与 `/api/models` 不是一个来源） | 一律由 `--model-dir` 的模型集钉住（`pin_engine_paths`），三个 `allow_download=false` | §5.3/§0.2 |
| 结果序列化 | 无 | 有界写入器（超限即中止）→ 413 `result_too_large` | §4.6 |
| 下载 | 无端点 | `POST /api/models/download`：未开开关 → 403；开启 → 真实任务 + 独立的 M2 worker（M1 如实失败，**无网络 I/O**） | §4.2/§8.1 |
| 性能表现 | — | 推理链路一行未动；12 图两个硬门槛逐位相同；新增的只有 serve 侧的线程与 HTTP | 无退化 |

---

### 未覆盖风险与**做不到的事**（如实记录）

1. **未提交、未在干净 clone 上验证**：按要求不 commit；证据来自当前工作树。
2. **`chunked` 请求体的 413 在 HTTP 层没有专门测试**：HTTP 层测的是 §4.4 第 5 步的
   `Content-Length` 预检（"声明 4 MiB 一个字节都不发 → 413"），流式上限（第 6 步）由 M0c 的
   `BodyBudget`/`read_body` 单元测试覆盖（用脚本化 `BodySource`）。二者合起来覆盖该要求，
   但**没有一条测试**同时经过"真实 chunked 传输 + 上限中止"。
3. **读取超时不能中断已经发起的一次 `read`**：`tiny_http` 不暴露 socket 读超时，因此
   "客户端发完请求头就不再发数据"会让 accept 线程阻塞在那一次 `read` 上（`Instant` 截止时间
   只能在**每次尝试读取之前**生效）。这是 M0c 接缝 6 的实体化结论，不是回归。
   要彻底解决只能给 `tiny_http` 上游提读超时或换服务器实现。
4. **accept 是单线程、就地读 body**：一个慢速上传会占住 accept 线程（其它静态请求等待）。
   这是 §8.1 的既定线程模型（"静态/校验类请求就地处理"），M1 未加并发 accept。
5. **`--allow-download` 打开时，下载任务必然失败**（原因文本写明"未实现、无网络 I/O"）。
   选择这个形状而不是"静默 403"是为了让页面**可见地降级**并让 `JobKind::ModelDownload`
   与 `/api/jobs/{id}` 的生命周期在 M1 就是真的；M2 只需替换 worker 的处理体。
6. **公式队列在 M1 生产路径上不可达**（§10.8），因此公平性验收用的是测试专用慢速后端 +
   `?queue=`（见 5.5）；真实引擎 + 公式路由的公平性要等 M4。
7. **内联页面的区域列表按响应的 `regions` 顺序渲染**，而 `to_output_json` 保留的是**检测顺序**；
   §9.2 希望列表是"阅读顺序"。M1 没有改库的输出形状（§1.1 指定复用 `to_output_json`），
   `复制全文` 用的 `plain_text` 是阅读顺序的。这是 M3/M4 需要决定的一个协议问题。
8. **`/api/models` 每次都重新读盘并重新哈希**（10–30 MB 文件，约 10–100 ms/次）：为了让 M2
   下载完成后页面立刻看到 `present`；令牌保护下本机页面轮询低频，因此没有加缓存。
9. **`--open` 只能打开系统默认浏览器**（`cmd /C start`），无法验证"确实打开了"。
10. **未验证真实浏览器里的手工闭环**（粘贴、拖放、进度条、键盘/焦点、View Transitions）：
    §12 的"手工"一行需要人工操作浏览器，本次只做了 HTTP 层与页面注入的自动验证。

---

### 与 `docs/05` §11「M1」验收清单的对照

| §11 M1 条目 | 本阶段 | 证据 |
| --- | --- | --- |
| `serve` 子命令 + feature 隔离 + 未启用 feature 的可定位错误 | ✅ 完成 | `cli.rs` 测试 + `rapidocr.rs` 的两个 `Command::Serve` 分支 |
| provider 启动期解析；`GET /`、`/api/status`、`POST /api/ocr`、`/api/jobs/{id}`、`/result` | ✅ 完成 | 5.2 的 HTTP 测试；`ServeShared::status_json` 三字段 |
| 单图上传（XHR + 进度 + 取消上传）、异步任务与轮询 | ✅ 服务端完成 | `202 → queued → running → succeeded`；页面侧的 XHR 是原型既有代码（未改） |
| 预览与 polygon 叠框、区域列表、复制全文、JSON 导出 | ✅ 数据面完成 | `regions[].polygon.points`（恰好 4 点、原图坐标）、`plain_text`；页面的渲染逻辑未改 |
| 模型缺失提示（消费 `ModelSetStatus`；**不含下载动作**） | ✅ 完成 | `/api/models` + 409 `detail`；下载按 M2 处理并可见降级 |
| 测试：状态机、tombstone（容量+TTL）、双队列 503、公平调度、准入顺序、Host/Origin/token、安全头与 nonce CSP | ✅ 完成 | 105（M0c 保留）+ 51（M1 新增）= 156 个 serve 测试，0 failed |
| **M1 验收**：12 图经 HTTP 的 `regions` 与文本与 `rapidocr run --json` 逐张一致 | ✅ 完成 | 5.3：12/12 一致（418 = 418） |
| **M1 验收**：空 `--model-dir` 下 `/api/models` 与 `/api/ocr` 的缺失字段一致 | ✅ 完成 | 5.4：`FIELDS_MATCH_API_MODELS=True` |
| **M1 验收**：公式洪水下普通 OCR 不被饿死、普通洪水下公式也不被饿死 | ✅ 完成（测试专用慢速后端） | 5.5 两个方向 |
| **M1 验收**：空模型目录下服务仍为 `Ready` + `EngineState::BlockedModelsMissing` | ✅ 完成 | 5.4 |
| **M1 验收**：CLI 中不存在任何可改变监听地址的选项，也不存在 `--ocr-workers` | ✅ 完成 | M0c 的 `cli` 枚举测试（21 个选项，无 `--host`/`--ocr-workers`） |
| M2/M3/M4 的条目（下载进度、`annotated.png`、`export`、`engine/reload`、公式模型） | ⛔ 不在 M1 | 见下表接缝 |

---

### 接缝（留给 M2 / M3 / M4）

**M2（模型下载）**

1. `download::worker` 的处理体：把 `NOT_IMPLEMENTED_REASON` 那一行换成真实的逐文件
   `download_model_set`（库侧已经就绪）+ 进度上报（`JobRecord` 目前没有进度字段，需要扩展）；
2. `ServeLimits::download_budget` 已用于"已知体积之和 > `--max-download-mb` → 413"的预检，
   M2 的逐文件递减直接用同一个 `DownloadBudget`；
3. `--allow-download-host` 目前只是被解析（M0b 的库函数只认编译期白名单）：M2 需要把它作为
   显式参数接进 `DownloadRequest.allowed_hosts` 并打印高风险警告；
4. 下载完成后**不自动**重建引擎（§7.6）：M2/M3 需要在 `POST /api/ocr` 或
   `POST /api/engine/reload` 时惰性创建——`EngineStateMachine::begin_loading` 正为此保留；
5. 下载任务的 `queue` 字段目前借用 `QueueClass::Text`（`JobStore` 要求一个类别）且 `position`
   恒为 `null`；M2 若要在 UI 上区分，应引入真正的中性类别而不是继续借用。

**M3（诊断与导出）**

1. `GET /api/jobs/{id}/annotated.png`：需要保留原图编码字节（M1 读完 body 就丢弃，
   字节账本仍按 `--max-body-mb` 记），并用 `output::visualize::draw_output` 生成 PNG；
2. `?format=json|md|html` 导出：`ReportMode::Static` + 独立的导出 CSP +
   `Content-Disposition: attachment` + `data:` 内嵌图片 + `--max-export-mb` → 413 `export_too_large`
   （`ServeError::ExportTooLarge` 已按 §11.1 冻结，等生产者）；
3. `POST /api/engine/reload`：`begin_loading` / `models_still_missing` / `begin_rebuild` 三个
   已测试的转换是它的全部状态迁移；
4. 诊断面板需要的逐阶段耗时已经在 `/result` 的 `timings`/`stages` 里（未重新测量）。

**M4（公式与评估）**

1. `OcrRouting { formula: true }` 的真实来源（CLI 选项或端点）：M1 的 `?queue=formula`
   只在测试路径下被接受，生产路径明确 400；
2. 公式模型集：`ModelPlan::resolve` 目前固定 `ModelRequest::text_only`，M4 需要
   `text_and_formula` 并处理"两个集合声明同一 role"的歧义（`ModelPlan::file_for` 已经会
   报 `AmbiguousRole` 而不是静默取第一个）；
3. 页面的公式开关目前只影响展示（原型不发送该开关）：M4 需要给 `POST /api/ocr` 一个
   明确的选中方式（查询参数或 JSON 字段），这会是一个**协议新增**，需要先改 `docs/05`；
4. 评估（CER/精确匹配）复用库的 `evaluation`，不另写指标。

---

## M2：模型管理（真实下载 + 进度 + 文件边界取消 + host opt-in + 惰性建引擎）

**阶段**：M2 —— `docs/05` §11「M2」的全部条目 + §4.2/§4.3/§4.6/§5.4/§6/§7.4–§7.6 里与本
里程碑相关的冻结契约。M1 记录里"留给 M2"的 5 条接缝（下载 worker 处理体、进度、
`--allow-download-host` 接线、惰性建引擎、中性队列类别）逐条收口。
**开工基线**：`06bd0af`（M0 `1144ddb` + M0b `dbab12e` + M1 `06bd0af` 已提交，工作树干净）。
**日期**：2026-10-03（接在 M1 记录之后）
**提交**：`（未提交：按要求不 commit）`

### 环境

| 项目 | 值 |
| --- | --- |
| target | Windows x64 + MSVC ABI：`x86_64-pc-windows-msvc` |
| 工具链 | rustc 1.98.1 / cargo 1.98.1 |
| crate | `rapid-ocr-rs` 0.7.0（独立 git 仓库，本阶段未提交） |
| 真实资产 | `OCR-Model/small/`、`OCR-Model/test-config-small.yaml`、`OCR-test-image/`（12 图 + golden） |
| 真实网络（opt-in） | ModelScope（`www.modelscope.cn`）：字典直连 200；ONNX 权重 **302 → CDN** |

**变更规模**（`git diff --stat`，15 个跟踪文件：+3739 / −198，其中本记录的追加 `docs/06` 为 +590、`docs/05` 为 +38）：

| 文件 | 行数 | 改动 |
| --- | --- | --- |
| `src/model_store.rs` | 2251 → 2749 (+498) | 显式 host 参数、观察者（进度/取消）、`available_disk_bytes` |
| `src/bin/serve/download.rs` | 72 → 405 (+333) | 真实下载 worker + 可注入执行体 + host 校验 |
| `src/bin/serve/jobs.rs` | 956 → 1322 (+366) | 中性队列、进度、结构化失败、`CancelOutcome`、`finish_cancelled` |
| `src/bin/serve/server.rs` | 1025 → 1413 (+388) | 真实下载端点、惰性建引擎、reload、两次同步拒绝、worker 收尾 |
| `src/bin/serve/tests.rs` | 1409 → 2533 (+1124) | 17 个新 HTTP 测试 + 脚本化下载器 + 闸门 |
| `src/bin/serve/error.rs` | 925 → 1005 (+80) | `ModelSetNotFound`(404)、`InsufficientDiskSpace` 带两个数值 |
| `src/bin/serve/model_plan.rs` | 504 → 558 (+54) | `set_by_id`/`set_ids`/`model_dir`、`pending_download` |
| `src/bin/serve/run.rs` | 469 → 509 (+40) | `--allow-download-host` 校验、启动警告、注入运行期 |
| `src/bin/serve/http.rs` | 706 → 714 (+8) | `POST /api/engine/reload` 路由与分发 |
| `src/bin/serve/{state,mod,limits}.rs`、`src/exports.rs` | +1/+8/+0/+1 | 生产者已到（去掉两处 `allow(dead_code)`）、文档、导出 |
| `src/bin/web/index.html` | 2100 → 2118 (+18) | 两处：错误文案与**真实下载进度**（见交付物 7） |
| `docs/05-local-web-demo-implementation.md` | +38 | §4.2 作业形状、§4.3 下载取消语义、§13 参考命令 |

**没有新增依赖**：`Cargo.toml` 未改（§2.1 的硬约束）。

---

### 交付物 1：把真实下载接到 HTTP 上（§4.2、§6）

**库侧**（`src/model_store.rs`）：

```rust
pub const DEFAULT_ALLOWED_HOSTS: &[&str] = &ALLOWED_DOWNLOAD_HOSTS;

pub struct DownloadRequest<'a> {
    pub url: &'a str, pub expected_sha256: &'a str, pub save_dir: &'a Path,
    pub max_bytes: u64, pub connect_timeout: Duration, pub read_timeout: Duration,
    /// §6.1 第 3 条的 opt-in：**显式参数**，库常量一个字都不改。
    pub allowed_hosts: &'a [&'a str],
}
impl<'a> DownloadRequest<'a> {
    pub fn new(url: &'a str, expected_sha256: &'a str, save_dir: &'a Path) -> Self; // allowed_hosts = 编译期白名单
}

/// 进度与取消（§6.6）：`file_started` 是**唯一**的取消检查点。
pub trait DownloadObserver {
    fn file_started(&mut self, file: &ModelFileSpec, index: usize, total: usize,
                    declared_bytes: Option<u64>) -> bool;
    fn bytes_written(&mut self, written_bytes: u64);
    fn file_finished(&mut self, file: &ModelFileSpec, index: usize, bytes: u64);
}
pub struct NoObserver;                                    // 库内调用方的默认：不取消、不报进度
pub fn download_model_set_observed(
    set: &ModelSet, root: &Path, budget: &mut DownloadBudget,
    connect_timeout: Duration, read_timeout: Duration,
    allowed_hosts: &[&str], observer: &mut dyn DownloadObserver) -> Result<Vec<PathBuf>>;
pub fn download_model_set(/* 未变 */) -> Result<Vec<PathBuf>>;   // = _observed(DEFAULT_ALLOWED_HOSTS, &mut NoObserver)
```

- **双权威被消除**：`DownloadPolicy` 里原来的 `allowed_hosts` 字段**删除**了。可信 host 列表
  现在只属于 `DownloadRequest`（"请求的契约"），策略只剩"可注入的运行环境"（磁盘空间探测 +
  测试用的明文口子）。`allowed_hosts` 参数经 `download_model_set_observed` → 每个文件的
  `DownloadRequest` 单向流入，构成唯一来源。
- 集合下载的进度口径：`index`（1 基）/`total`/`declared_bytes` 只统计**需要下载**的文件
  （缺失 ∪ 损坏；`Present` 的文件不计数、不发请求、也不触发取消检查点）。

**serve 侧**（`src/bin/serve/download.rs` 重写，405 行）：

```rust
pub(super) struct DownloadCommand { pub job_id: String, pub set_id: String }   // 形状未变
pub(super) struct DownloadJob<'a> {
    pub set: &'a ModelSet, pub root: &'a Path, pub budget_bytes: u64,
    pub connect_timeout: Duration, pub read_timeout: Duration,
    pub allowed_hosts: Vec<String>,                     // 编译期白名单 ∪ --allow-download-host
}
pub(super) trait DownloadSink { /* file_started / bytes_written / file_finished */ }
pub(super) trait ModelDownloader: Send {                // 镜像 EngineFactory 的注入缝
    fn download(&mut self, job: &DownloadJob<'_>, sink: &mut dyn DownloadSink)
        -> Result<Vec<PathBuf>, RapidOcrError>;
}
pub(super) type DownloaderFactory = Arc<dyn Fn() -> Box<dyn ModelDownloader> + Send + Sync>;
pub(super) fn real_downloader_factory() -> DownloaderFactory;   // 生产路径
pub(super) fn worker(runtime: Arc<ServeShared>, inbox: Receiver<DownloadCommand>,
                     factory: DownloaderFactory);
```

`POST /api/models/download`（`server.rs::submit_download`）按顺序做四件事：

1. `--allow-download` 未开 → **403 `downloads_disabled`**（M1 已有，未改动）；
2. `set_id` 严格解析（`ModelPlan::set_by_id`）：未知 → **404 `model_set_not_found`**，
   `detail` 给出请求的 id 与**已知集合**；**全仓库没有 `sets[0]` 回落**
   （`grep -n 'sets\[0\]' src/bin/serve/*.rs` 只命中测试里的 JSON 断言）；
3. 预算预检（§6.2）与磁盘预检（§6.5）→ 见交付物 6；
4. 建任务（`JobQueue::Download` + 初始进度）→ 有界 channel（满 → 503 `busy`，并回收任务）。

**修改前后行为对比（交付物 1）**

| 项目 | 修改前（M1） | 修改后 |
| --- | --- | --- |
| `POST /api/models/download`（开 `--allow-download`） | 建真实任务，worker 立即判失败并写"未实现、无网络 I/O" | **真实下载**：按集合逐文件走库的加固下载器（HTTPS/重定向逐跳校验/host 白名单/体积上限/唯一临时名/`MoveFileExW`/单飞/空间预检） |
| 未知 `set_id` | 400 `bad_request`（不说明已知集合） | 404 `model_set_not_found` + `detail.{set_id, known_sets}` |
| 集合解析 | 在响应里现查 status，但 worker 只拿到 id 不解析 | 提交与执行**都用** `set_by_id`；执行期集合消失 → 同一个 404（可定位），不换集合 |
| 下载完成后的集合状态 | 交付物不存在 | `/api/models` 立刻看到 `present`（每次重新读盘+哈希，M1 已记录该设计） |

---

### 交付物 2：进度与文件边界取消（§4.3、§6.6）

**任务层**（`src/bin/serve/jobs.rs`）：

```rust
pub struct DownloadProgress {           // 序列化进 JobView.download（仅 model_download 非 null）
    pub files_done: usize, pub files_total: usize,
    pub bytes_done: u64, pub bytes_total: Option<u64>,
    pub current_file: Option<String>,
}
impl DownloadProgress { pub fn planned(files_total: usize, bytes_total: Option<u64>) -> Self }

pub struct JobFailure {                      // 失败分类：与 ServeError 的 HTTP 映射同源
    pub status: u16, pub code: &'static str, pub message: String,
    pub detail: serde_json::Value,
}
impl JobFailure { pub fn new(..) -> Self; pub fn from_body(status: u16, body: &ErrorBody) -> Self }
impl From<&ServeError> for JobFailure { .. }

pub enum CancelOutcome { Cancelled, CancelRequested }

impl JobStore {
    pub fn set_download_progress(&mut self, id: &str, progress: DownloadProgress) -> Result<(), ServeError>;
    pub fn is_cancel_requested(&self, id: &str) -> bool;
    pub fn fail_classified(&mut self, id: &str, failure: JobFailure, now: Millis) -> Result<(), ServeError>;
    pub fn cancel(&mut self, id: &str, now: Millis) -> Result<CancelOutcome, ServeError>;
    pub fn finish_cancelled(&mut self, id: &str, now: Millis) -> Result<(), ServeError>;  // worker 专用
}
```

- `JobView` 追加 `failure` / `download` / `cancel_requested`（`error` 仍是同一份文本，旧客户端不受影响）；
- **取消语义按任务类型区分**（这是 M1 接缝第 5 条的根因修法，而不是给下载"特批"）：
  - `Queued`（任何类型）→ 立即 `Cancelled`，可靠（§4.3 原文）；
  - `Running` 且是**下载** → 登记 `cancel_requested`，**状态不变**，返回 200 + 视图
    （客户端看到的是事实：当前文件还在下）；
  - `Running` 的 **OCR** 与全部终态 → 409 `not_cancellable`（M1 语义一字未改）；
- **`Running → Cancelled` 的生产者**：库只在观察者于文件边界返回 `false` 时产生
  `DownloadError::Cancelled`，worker 用它调用 `finish_cancelled`；若出现"取消结论先到、请求登记
  后到"的竞态，`ServeShared::finish_download_cancelled` 会把请求补登记——任务**绝不**停在 `running`；
- 取消后：当前文件**下载完并原子替换**（已校验的文件保留），后续文件在 `file_started`
  处被拒（不开始、不发请求），临时文件由库的 `PartFile::Drop` 保证不残留。

**如实声明做不到的事**：取消**不能中断正在进行的那个文件**。`reqwest` 的阻塞读取一旦发起就没有
安全的中断语义，因此取消的延迟上界是"当前文件的剩余下载时间"；HTTP 响应与此一致
（200 + `state:"running"` + `cancel_requested:true`，而不是假装已停止）。库里的
`DownloadObserver` 文档逐字写了这一条。

**修改前后行为对比（交付物 2）**

| 项目 | 修改前（M1） | 修改后 |
| --- | --- | --- |
| `/api/jobs/{id}` 的进度 | 无（只有 `elapsed_ms`） | `download{files_done, files_total, bytes_done, bytes_total, current_file}` |
| 失败的机器可读分类 | 只有 `error` 文本（客户端只能字符串匹配） | `failure{status, code, message, detail}`，与 `/result` 上重放的错误体同源 |
| 运行中的下载取消 | 409 `not_cancellable`（与 OCR 一样） | 200 + `cancel_requested`，文件边界兑现后 `state=cancelled` |
| 排队中的下载取消 | 200 `cancelled`（可靠） | 同（未变），并新增"取消后 worker 的 `start` 必须失败"的断言 |
| 关闭时未开始的下载任务 | `fail()` 被存储拒绝（`Queued → Failed` 非法）→ 任务永远停在 `queued` | `abandon_download`：按"排队中取消"收尾为 `cancelled` |

---

### 交付物 3：`--allow-download-host` 的显式 opt-in（§6.1 第 3 条）

```rust
// src/bin/serve/download.rs
pub(super) fn validate_extra_hosts(hosts: &[String]) -> Result<Vec<String>, ServeConfigError>;
```

- 只接受**裸主机名**：带 scheme/端口/路径/userinfo/通配符/空白/非 ASCII 一律拒绝，错误里带
  开关名与原值（可定位），并按大小写不敏感去重；
- `run.rs` 启动期调用它，失败即**拒绝启动**（实测：`invalid --allow-download-host=https://evil.example:
  the host must be a bare host name (no scheme, port, path, userinfo or wildcard)`，exit 1）；
- 启动时打印**生效列表**并 stderr 打印**高风险警告**（实测输出，逐字）：

```text
serve: trusted download hosts www.modelscope.cn, evil.example, mirror.example.com
serve: WARNING --allow-download-host extends the trusted download allow-list beyond the compiled-in
set [www.modelscope.cn]: [evil.example, mirror.example.com]. The compiled-in constant is what makes a
local manifest.json unable to point downloads at arbitrary hosts: a manifest is a *resource
description*, and docs/05 §6.1 item 3 (OWASP SSRF prevention) requires the allow-list to come from
trusted configuration instead. Every host you add here is accepted for model downloads; add only
hosts you control or trust, and remove the flag when you no longer need it.
```

- **库常量没有被改**：`ALLOWED_DOWNLOAD_HOSTS` 仍是 `["www.modelscope.cn"]`，扩展只经
  `DownloadRequest::allowed_hosts` / `download_model_set_observed(.., allowed_hosts, ..)` 传入；
- `/api/status` 新增 `download_hosts`（只读诊断，值 = 编译期白名单 ∪ opt-in）。

**"本地 manifest 不能自己扩大白名单"的两层证据**：

1. **库级**（`model_store::tests::a_local_manifest_cannot_widen_the_download_host_allow_list`，M0b 起就在）：
   `manifest.json` 声明 `https://evil.example/...` → 走**公开**入口（默认 = 编译期白名单）得到
   `HostRejected{host:"evil.example"}`；
2. **库级（新增）** `an_explicit_host_allow_list_extends_the_compiled_in_one`：同一请求，
   `allowed_hosts` 传 `DEFAULT_ALLOWED_HOSTS` → 拒绝且**零请求**；传 `["127.0.0.1"]`（fixture）
   → 放行并落盘；同时断言 `ALLOWED_DOWNLOAD_HOSTS == ["www.modelscope.cn"]`、`DEFAULT_ALLOWED_HOSTS == ALLOWED_DOWNLOAD_HOSTS`；
3. **HTTP 级（新增）** `the_download_host_opt_in_is_passed_as_an_explicit_parameter`：脚本化下载器
   记录每次任务收到的列表——不带开关是 `["www.modelscope.cn"]`，带 `--allow-download-host evil.example`
   是 `["www.modelscope.cn","evil.example"]`。

---

### 交付物 4：`POST /api/engine/reload` 与**惰性**建引擎（§7.6）

**HTTP**：新增路由 `Route::EngineReload`（`http.rs`），无请求体，响应 200：

```json
{ "outcome": "ready|blocked_models_missing|failed",
  "engine": { …与 /api/status 的 engine 同一形状（冻结的 EngineState）… },
  "missing": [...], "corrupt": [...], "source": "default_table", "model_dir": "<redacted>",
  "load_ms": 42 }
```

`server.rs::ensure_engine_loaded(force: bool) -> EngineLoad{Ready,BlockedModelsMissing,Failed}` 是
`EngineStateMachine::{begin_loading, models_still_missing}` 的**唯一生产者**：

1. 模型齐备 → `begin_loading()`（`Blocked`/`Ready`/`Failed` 都合法）→ **释放状态锁**建立会话
   → `load_succeeded(requested, selected_ep, fallback_to_cpu)` 或 `load_failed(reason)`；
2. 模型仍缺失且状态是 `BlockedModelsMissing` → `models_still_missing(missing)`（刷新清单，状态不变）；
3. 模型仍缺失但状态是 `Ready`/`Failed` → **不进入** `Loading`（§7.6 的转换表里
   `Ready|Failed → BlockedModelsMissing` 不存在），改为 `Loading → Failed` 并在 reason 里**点名**
   缺哪些文件（实测响应 `outcome:"failed"`、`engine.reason` 含文件名）；
4. `force=false`（`POST /api/ocr` 的惰性路径）在"已 Ready 且引擎在场"时立即返回，不重建；
   `force=true`（reload）即使已 Ready 也重建会话（用户的显式意图是"按磁盘当前文件重新加载"）；
5. 加载全程**不持有** `engine_state` 锁（否则 `/api/status` 在加载期间被阻塞、也看不到
   `loading`），只用一把独立的 `engine_load` 互斥串行化两个加载者；
6. 会话创建耗时进 `/api/status.engine_load_ms`（`null` = 还没建立过）与 reload 的 `load_ms`
   （本次调用的墙钟耗时）。

**惰性创建**（M1 接缝第 4 条）：`submit_ocr` 的准入在 `BlockedModelsMissing` 时**先看磁盘**：
模型已经齐备（典型场景是刚下载完）→ `begin_loading()` 并把会话创建留给 worker（accept 线程
绝不建会话，§8.2 的同一理由）；仍然缺失 → 409 + 与 `/api/models` 同源同值的清单。
`ocr_worker` 取出任务后调用 `ensure_engine_loaded(false)`，失败时任务以**同一份**错误映射失败
（409 `models_missing` 带清单 / 503 `engine_unavailable` 带 reason）。

**实测证据**（HTTP 测试）：
`the_engine_is_created_lazily_on_the_next_ocr_request_and_loading_is_visible`：
启动时不齐备 → `engine.state == "blocked_models_missing"`、工厂调用 0 次；把缺失文件写到磁盘
（等价于下载完成）→ **仍然** `blocked_models_missing`、工厂 0 次（证明没有后台建引擎）；
`POST /api/ocr` → 202，闸门停在会话创建处 → `/api/status.engine.state == "loading"`、工厂 1 次；
放行 → 任务 `succeeded`、`engine.state == "ready"`、`engine_load_ms` 非 null。

一条边界（如实记录）：`Failed` 状态下 `POST /api/ocr` 会在**准入**就返回 503（不建任务），因此
worker 的惰性路径不会对着 `Failed` 反复重试；唯一会"重试"的情形是"任务在 `Loading` 期间被准入、
随后这次加载失败"——那时已准入的任务会各自触发一次新的加载尝试，失败即以 `Loading → Failed`
收尾并给该任务 503 `engine_unavailable`（不假装成功）。`an_engine_that_fails_to_load_is_failed_with_a_reason_and_503`
覆盖的是准入即 503 的那条主路径。

---

### 交付物 5：中性队列类别（M1 接缝第 5 条）

```rust
pub enum JobQueue { Text, Formula, Download }
impl JobQueue { pub fn name(self) -> &'static str; pub fn class(self) -> Option<QueueClass>; pub fn from_class(c: QueueClass) -> Self }
```

`JobRecord.queue` / `JobView.queue` 的类型从 `QueueClass` 换成 `JobQueue`。根因是**两个概念被
混用**：`QueueClass` 是"双队列调度器的类别"（有容量/配额/等待上界），下载任务走 §8.1 的
独立有界 channel，从不进调度器、`position` 永远是 `null`。M1 借用 `QueueClass::Text` 会让任何
按 `queue` 聚合的诊断报告"一个从未排队的下载任务属于文本队列"。

- `server.rs::sync_positions` 用 `view.queue.class()` 过滤：下载任务不参与 `position` 计算；
- `/api/status.queues` 的 `text`/`formula` 计数**本来**就不含下载（它们数调度器），未改动；
- `/api/jobs/{id}` 的 `queue` 对下载任务是 `"download"`；OCR 任务仍是 `"text"`（未变）。

---

### 交付物 6：M1 的两处诚实缺口（507/413 的两个数值、`Cancelled` 生产者）

| 缺口 | 修法 | 证据 |
| --- | --- | --- |
| `ServeError::InsufficientDiskSpace` 无生产者（507 只在映射表里） | 变体带两个数值（`required_bytes`/`available_bytes`），`submit_download` 用**可注入**的空间探测（生产 = 库的 `available_disk_bytes`，即下载器内部同一个 `GetDiskFreeSpaceExW`）做任务级预检；`detail` 字段名与库侧 `DownloadError::InsufficientSpace` 逐字相同 | 新增 HTTP 测试 `a_disk_space_refusal_is_a_507_with_both_numbers`（注入 10 B → 507，`required_bytes`/`available_bytes` 两个数值都断言）；`error::tests::download_details_carry_the_numbers` 断言同步预检与任务内预检字段同名 |
| 预算拒绝没有两个数值（M1 只回 413 `payload_too_large`） | 改用库的分类 `ServeError::Download(DownloadError::TooLarge{limit_bytes, observed_bytes})` → 413 `payload_too_large`，`detail` 两个数值 | `a_download_budget_refusal_is_a_413_with_both_numbers`（1 MiB 预算 vs 2×10 MiB 声明 → `limit_bytes=1048576`、`observed_bytes=20971520`，且下载器**零调用**） |
| `DownloadError::Cancelled` 没有生产者 | 库：观察者在文件边界返回 `false` → `Cancelled`（新增库测试）；serve：worker 映射为 `finish_download_cancelled`，并在"脚本化下载器自己报 Cancelled"时不把任务留在 `running` | 库 `cancelling_at_a_file_boundary_keeps_verified_files_and_leaves_no_temp_file`、`a_cancel_requested_before_the_first_file_writes_nothing`；HTTP `a_running_download_is_cancelled_at_a_file_boundary`、`a_download_error_cancelled_lands_on_cancelled_not_running` |

**新增的协议项**：`ServeError::ModelSetNotFound{set_id, known}` → **404 `model_set_not_found`**。
§11.1 的清单里没有能产生它的变体，但 §4.2 的请求体只有 `set_id`，"未知集合"必须可定位、
**绝不**回落 `sets[0]`；它与"请求体畸形"的 400 `bad_request` 是两件事。已在 `docs/05` §4.2 记录。

---

### 交付物 7：页面（`src/bin/web/index.html`，2 处最小改动）

1. `ERR_TEXT.payload_too_large` 的文案：`'图像超出请求体上限（--max-body-mb）'` →
   `'请求体或下载总量超出上限（--max-body-mb / --max-download-mb）'`。
   原因：M2 起 413 `payload_too_large` 也可能是**下载预算**拒绝（§6.2），旧文案会主动误导。
2. 横幅里的真实下载进度：`state.dlJob.download` 的
   `bytes_done/bytes_total`（未知时退回 `files_done/files_total`）算出百分比，写上
   `当前 <file> · n/m 个文件`，并把进度条从"不确定态"（`bar ind`）换成真实宽度。
   原因：§9.2 要求横幅给出**下载进度**，而 M1 的实模式只有一个不确定的滚动条；进度字段是
   M2 才有的（交付物 2）。改动只在 `renderBanner` 的 `state.dlJob` 分支内（新元素 id
   `dlJobBar`，沿用既有 `.bar` 样式；无内联 `onclick`、无外部资源、nonce/token 字面量未动）。

`Temp/demo3-v2.html` **未被触碰**（见"证据：未触碰的文件"）。脚本语法用 `node --check` 复核
（真实 IIFE 块 67,143 字符 → exit 0；模板里 `nonce="` 仍 4 处、`__SRV_TOKEN__` 仍 3 处，
与 M1 的注入契约一致）。

---

### 验证 1：静态检查、feature 矩阵与依赖隔离

日志：`target/m2-verify/m2-gates.log`（1–7 号单次连续执行）、`target/m2-verify/tree-*.txt`。

| # | 命令 | 结果 | 退出码 |
| --- | --- | --- | --- |
| 1 | `cargo fmt --all -- --check` | 无输出 | 0 |
| 2 | `cargo clippy --all-targets -- -D warnings` | 无 warning | 0 |
| 3 | `cargo clippy --features serve --all-targets -- -D warnings` | 无 warning | 0 |
| 4 | `cargo test --all-targets` | 374 + 2 + 4 + 14 = **394 passed, 0 failed** | 0 |
| 5 | `cargo test --features serve --all-targets` | 394 + **176** = **570 passed, 0 failed** | 0 |
| 6 | `cargo build --release --bins` | `rapidocr.exe` **34,522,112 B**（M1: 34,446,336 B） | 0 |
| 7 | `cargo build --release --bins --features serve` | 同一二进制即可跑 serve | 0 |

**默认依赖图逐字节未变**（§2.1 的硬约束）：

| 证据 | 值 |
| --- | --- |
| `cargo tree -e normal -p rapid-ocr-rs` | 606 行，SHA-256 `BD2AB5E41B1A6D649E2F80B0D3D3E55327B96EB7C6F861E55DFC7C8501C3F6FC` |
| 与 M0c（`tiny_http` 引入**之前**）的快照 `target/m0c-tree-default.txt` 逐行比较 | **0 处差异** |
| 默认树 / `--no-default-features` 里的 `tiny_http` | 0 / 0 次 |
| `cargo tree -e normal -p rapid-ocr-rs --features serve` | 611 行，`tiny_http` 1 次 |
| `Cargo.toml` | **未改**（无新依赖） |

**测试增量与基线对比（AGENTS.md §6）**

| 项目 | 修改前（M1 完成） | 修改后 | 说明 |
| --- | --- | --- | --- |
| `cargo test --all-targets` | 369+2+4+14 = 389 | **374+2+4+14 = 394** | +5（`model_store` 35 → 40） |
| `cargo test --features serve --all-targets` | 389+156 = 545 | **394+176 = 570** | +25 净增（库 +5、serve 二进制 +20） |
| serve 各文件测试数 | `download` 0 / `jobs` 20 / `tests` 24 | **1 / 22 / 41** | +1 / +2 / +17 |
| 删除/跳过/弱化的测试 | — | **0** | 2 个 M1 测试按**新行为**改写（见下），其余为纯新增 |
| 默认构建依赖图 | 606 行 | 606 行，与 M0c 快照 0 差异 | 未变 |

**两个按新行为改写的 M1 测试（不是弱化，是期望变更）**：

1. `the_download_endpoint_degrades_visibly` → 更名并改写为
   `the_download_endpoint_refuses_disabled_unknown_and_url_bearing_requests`：
   M1 断言"worker 判失败并写明未实现"，M2 的处理体是真的，所以现在断言
   **403 / 404 + 已知集合 / 400（带 URL 的请求体）**，以及"集合已齐备时任务成功且 0 个文件要下"；
2. `unknown_paths_are_404_and_wrong_methods_are_405`：`/api/engine/reload` 从"不存在的 M3 端点"
   变成真实路由，因此从 404 列表移到 405 断言（并断言 `Allow: POST`）。

---

### 验证 2：12 图硬门槛（`target/m2-gate/`，**没有**覆盖 `tests/baseline/`）

```powershell
target\release\bench_warm_e2e.exe --config <OCR-Model>\test-config-small.yaml --images-dir <OCR-test-image> `
  --warmup-rounds 1 --rounds 3 --max-side-len 2000 --intra-threads 16 --output target\m2-gate\bench-cpu-2000.json
target\release\rapidocr.exe evaluate --manifest <OCR-test-image>\golden-manifest.json `
  --config <OCR-Model>\test-config-small.yaml --output target\m2-gate\evaluation-cpu.json
```

| 门槛 | 文档要求 | 本次实测（release） | 比较方式 | 结论 |
| --- | --- | --- | --- | --- |
| 12 图区域数均值 | `34.833333333333336` | **`34.833333333333336`** | 原始 JSON 的数字**字面量**字符串精确比较 | 逐位相同 |
| 12 图 mean CER | `0.44765135645866394` | **`0.44765135645866394`** | 同上 | 逐位相同 |

**是否重跑、为什么**：**重跑了**。M2 **没有**改动推理链路（`ImageInput → OcrRequest → OcrOutput`
一行未动；库侧改动全部在 `model_store` 的下载/空间探测与 `exports.rs` 的导出面），因此按
M0a/M1 记录的口径这属于"明知不会变"的一类；但仍然重跑，因为 M2 在**二进制侧**动过共享状态
（`ServeRuntime` 的线程与共享状态、任务存储）且 12 图门槛是发布前的唯一数值关卡——
"没跑"与"跑了且逐位相同"是两种证据强度。`git status --porcelain -- tests/baseline` 为空。

---

### 验证 3：12 图 HTTP-vs-CLI 逐张对照（重跑，`target/m2-e2e/`）

**为什么重跑**：M2 改了页面消费的响应形状（`/api/jobs/{id}` 新增
`failure`/`download`/`cancel_requested`；下载任务的 `queue` 值改为 `download`），因此按要求
重跑判据（脚本是 M1 的 `run-e2e.ps1` 原样复制到 `target/m2-e2e/`，只改输出目录）。

```text
ALL_12_MATCH=True
region counts: serve=418 cli=418
```

| # | 图片 | serve `regions` | CLI `regions` | 区域数相同 | `text` 逐字节相同 | 逐区域文本序列相同 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 01基础多位置文本.png | 42 | 42 | ✅ | ✅ | ✅ |
| 2 | 02多语言与RTL混排.png | 21 | 21 | ✅ | ✅ | ✅ |
| 3 | 03旋转与倾斜.png | 13 | 13 | ✅ | ✅ | ✅ |
| 4 | 04表格与键值对.png | 61 | 61 | ✅ | ✅ | ✅ |
| 5 | 05代码与等宽字体.png | 38 | 38 | ✅ | ✅ | ✅ |
| 6 | 06低对比度与深色背景.png | 21 | 21 | ✅ | ✅ | ✅ |
| 7 | 07小字号与密集排版.png | 37 | 37 | ✅ | ✅ | ✅ |
| 8 | 08数字公式与符号.png | 51 | 51 | ✅ | ✅ | ✅ |
| 9 | 09竖排文本.png | 14 | 14 | ✅ | ✅ | ✅ |
| 10 | 10长段落与分栏.png | 37 | 37 | ✅ | ✅ | ✅ |
| 11 | 11文字样式与特效.png | 22 | 22 | ✅ | ✅ | ✅ |
| 12 | 12综合压力测试.png | 61 | 61 | ✅ | ✅ | ✅ |
| — | **合计** | **418** | **418** | **12/12** | **12/12** | **12/12** |

空模型目录（§7.6 的回归，M1 判据原样重跑）：

```text
service.state=ready engine.state=blocked_models_missing model_dir=<redacted> source=default_table
models.missing=[PP-OCRv6_det_small.onnx, PP-OCRv6_rec_small.onnx, ppocrv6_dict.txt]
ocr http=409 code=models_missing
ocr detail missing=[PP-OCRv6_det_small.onnx, PP-OCRv6_rec_small.onnx, ppocrv6_dict.txt] corrupt=[] source=default_table model_dir=<redacted>
FIELDS_MATCH_API_MODELS=True     engine.state equals blocked_models_missing: True     service ready: True
```

---

### 验证 4：真实网络（opt-in，`RAPID_OCR_ALLOW_NETWORK=1`）与一条**未解决**的发现

> **【M2b 已解决】** 本节 (b) 记录的阻塞项（默认表 ONNX 权重 302 → CDN 被拒绝）已由 M2b
> 修复：§6.1 第 2 条改写为"禁止盲从 + 手工逐跳校验"，`ALLOWED_DOWNLOAD_HOSTS` 增加
> `cdn-lfs-cn-1.modelscope.cn`，`a_real_network_weight_download_is_rejected_because_the_host_redirects`
> 被替换为成功路径用例
> `a_real_network_weight_download_through_the_cdn_redirect_lands_and_verifies`。
> 证据、命令与实测跳转链见文末 **【M2b】**（本节其余内容按当时事实**原样保留**，不覆盖）。
> 本节表格里的"重定向（真实主机）"一行因此只在"当时"成立，现状见 M2b 记录。

日志：`target/m2-verify/network-test.log`（命令
`$env:RAPID_OCR_ALLOW_NETWORK='1'; cargo test --features serve --bin rapidocr a_real_network -- --nocapture --test-threads=1`）。

**(a) 成功路径**（`a_real_network_download_of_the_default_table_dictionary_lands_and_verifies`）：
临时模型目录 + 本地 manifest（一个待下载的真实字典 + 两个已就位的夹具文件以满足三个 role），
经 `POST /api/models/download` 走**生产**下载器：

```text
real network download: files_done=1 files_total=1 bytes_done=74947 bytes_total=74947 elapsed_ms=614
real network download: ppocrv6_dict.txt = 74947 bytes, sha256 b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d
test serve::tests::a_real_network_download_of_the_default_table_dictionary_lands_and_verifies ... ok
```

断言全过：任务 `succeeded`；`ppocrv6_dict.txt` 落盘，**SHA-256 与默认表声明的哈希逐位相同**；
`/api/models` 从 `complete:false` 变为 `complete:true`、`missing:[]`。

**(b) 一条真实发现：默认表的 ONNX 权重在 ModelScope 上是 302，而 §6.1 第 2 条要求拒绝重定向。**

第一次尝试直接下载默认表的 v6-tiny 文本集合（6,346,587 B）时，任务在**第一个文件**就失败：

```text
state=failed  failure.code=download_failed  failure.detail.kind=redirect  status=502
error: the model host answered with a redirect to
  `https://cdn-lfs-cn-1.modelscope.cn/prod/lfs-objects/f4/2c/0fbd…?filename=PP-OCRv6_det_tiny.onnx…`;
  automatic redirects are disabled
```

独立复核（`curl.exe`，不带 `-L`）：

| URL | 结果 |
| --- | --- |
| `…/onnx/PP-OCRv6/det/PP-OCRv6_det_tiny.onnx` | **302**（345 B 的跳转体）→ `cdn-lfs-cn-1.modelscope.cn` |
| `…/paddle/PP-OCRv6/rec/PP-OCRv6_rec_tiny/ppocrv6_tiny_dict.txt` | 200，27,156 B，`num_redirects=0` |
| `…/paddle/PP-OCRv6/rec/PP-OCRv6_rec_small/ppocrv6_dict.txt` | 200，74,947 B，`num_redirects=0` |
| `…/paddle/PP-OCRv4/rec/arabic_PP-OCRv4_rec_infer/arabic_dict.txt` | 200，405 B，`num_redirects=0` |

即：**字典/词表类来源是直连的，ONNX 权重走 LFS 的 302 跳转**。加固下载器按当时冻结的契约拒绝它
（`a_real_network_weight_download_is_rejected_because_the_host_redirects` 就是这条结论的
opt-in 回归测试：真实主机 302 → 502 `download_failed`/`redirect`，目录里一个字节都没落；
**M2b 已用成功路径用例取代它**）。
后果必须如实写清：**当时 `POST /api/models/download` 对"含权重的默认表集合"必然失败**，
用户看到的是一条可定位的 `redirect` 错误；这不是 M2 的接线缺陷，而是"默认表来源 + 拒绝重定向"
两条冻结决策的合成结果。两条出路（都需要先改 `docs/05` §6.1 第 2 条）：
① **【M2b 已采纳】** 逐跳校验 host/path 后放行重定向（§6.1 已经写明这是"未来若要支持"的方式，
新增 `cdn-lfs-cn-1.modelscope.cn` 这一项**逐跳**校验的固定 host）；② 把 CDN 直链写进默认表
（auth_key 会过期，不可行）。**M2 不做这个决定**，把它作为 M3/发布前的阻塞项上报。

---

### 覆盖分工（哪一层证了什么）

**只在库层**（`model_store.rs` 单测，本机 fixture，零公网）：仅 HTTPS/拒绝 302/编译期白名单与
显式参数/`Content-Length` 预检/流式上限/唯一 `.part` 名与 `Drop` 清理/**单飞**（2 线程 → 1 次请求）/
磁盘空间预检（注入探针）/**`MoveFileExW` 原子替换**（含"目标已损坏 → 覆盖"与"替换失败保留原文件"）/
缓存命中不发请求/预算记账/观察者的下标·字节·顺序/**文件边界取消**（已校验文件保留、无 `.part`）/
"第一个文件前取消则零请求零写入"/`available_disk_bytes`。

**只在 HTTP 层**（`serve::tests`，脚本化下载器，零公网）：403/404/400 的拒绝形状/
**413 与 507 的同步拒绝（两个数值）**/202→queued→running→succeeded·failed·cancelled 的完整生命周期/
**运行中可观测的进度**（闸门保证确定性）/逐文件失败后已校验文件保留/**运行中下载的文件边界取消**
（200 + `cancel_requested` → `cancelled`，第二个文件从未开始）/排队中取消立即生效/
`failure`+`download`+`cancel_requested` 的 JSON 形状/`--allow-download-host` 作为显式参数传入/
`POST /api/engine/reload` 的三种结论/**惰性建引擎**与 `loading` 可见/集合齐备后任务成功且 `/api/models` 变 `complete`。

**两层都覆盖**：真实网络（opt-in）里，生产下载器经 HTTP 端点下载真实字典（成功 + 哈希 + complete），
**以及真实权重经 302 → CDN 的逐跳跟随**（M2b：成功 + 哈希 + complete，取代了原先"真实主机的
302 → 结构化 `redirect` 失败"这条拒绝路径证据）。

**未覆盖（如实）**：

1. **单飞与原子替换只在库层验证**：HTTP 层没有"两个并发请求下载同一文件"的测试（下载 worker 只有
   一个、有界 channel 也是串行的，因此这条在 serve 里不是主要风险，但没有断言）；
2. **§6.3 的"目标已损坏时重新下载"在 HTTP 层未覆盖**：脚本化下载器只写新文件、不做替换；
   库层的 `an_existing_corrupt_target_is_replaced` 覆盖了它；
3. **逐 64 KiB 的进度粒度未在 HTTP 层断言**：脚本化下载器每文件报两个块；真实下载器的分块
   回调只在真实网络运行中被隐式走过；
4. **真实网络的多文件成功路径未跑通**：第一次尝试在第二个文件遇到 10 s `connect_timeout`
   （504），因此成功路径的夹具改成"一次真实往返"（其余两个 role 由已就位夹具满足）。
   多文件真实下载的网络稳定性由使用者承担，本阶段不假装它已验证；
5. **未验证真实浏览器里的手工闭环**（下载按钮、进度条、取消按钮的视觉行为）：§12 的"手工"一行
   仍需人工操作浏览器；本阶段只做了 HTTP 层与页面注入的自动验证 + `node --check`；
6. **`--allow-download-host` 只在启动期校验**（`validate_extra_hosts` 单测 + 启动实测），
   没有"运行期不接受任何 host 变更"的测试（本来也没有这种入口）。

---

### 关键行为对比（AGENTS.md §9）

| 项目 | 修改前（M1） | 修改后 | 预期结果 |
| --- | --- | --- | --- |
| 下载端点（原有功能） | 占位处理体，必失败并写明未实现 | 真实下载（库的加固路径），失败给结构化分类 | §4.2/§6 |
| 下载进度（新增） | 不存在（`elapsed_ms` 而已） | `download{files_done,files_total,bytes_done,bytes_total,current_file}` | §4.3 的作业形状 |
| 下载取消（修改功能） | 运行中 409（与 OCR 一样） | 运行中 200 + `cancel_requested`，**文件边界**兑现；排队中仍立即取消 | §6.6，且不假装能中断当前文件 |
| 失败分类（修改功能） | 只有 `error` 文本 | `failure{status,code,message,detail}`（与 `/result` 的错误体同源） | 客户端不必字符串匹配 |
| host opt-in（新增） | `--allow-download-host` 只被解析、无作用 | 显式参数接入 `download_model_set_observed` + 启动打印高风险警告 + 启动期校验 | §6.1 第 3 条 |
| 库常量 | 编译期白名单 | **未改**（`ALLOWED_DOWNLOAD_HOSTS` 仍是 1 项，单测锁死） | 清单永远不能自己扩大白名单 |
| 引擎状态（修改功能） | 启动期一次性判定，`begin_loading`/`models_still_missing` 无生产者 | `POST /api/engine/reload` + `POST /api/ocr` 的惰性路径是它们的生产者；`loading` 可见、`Failed` 带 reason | §7.6 |
| 下载完成后的引擎 | 不存在该路径 | **不**在后台创建；下一次 `POST /api/ocr` 或显式 reload 才创建 | §7.6 原文 |
| 队列诊断（修改功能） | 下载任务借用 `QueueClass::Text`，`position:null` | 中性 `JobQueue::Download`；`position` 仍为 `null` 但不再是"文本队列" | 诊断不再说谎 |
| 磁盘/预算拒绝（修改功能） | 507 无生产者；413 无两个数值 | 同步 507/413，`detail` 里两个数值；任务内同一组字段名 | §6.2/§6.5/§11.1 |
| 页面（修改功能） | 实模式下载进度是不确定条；413 文案只提图片 | 真实百分比 + 当前文件名；413 文案覆盖两种预算 | §9.2 |
| 性能表现 | — | 推理链路一行未动；12 图两个硬门槛逐位相同；新增的只有下载线程与 HTTP 路径 | 无退化 |
| 依赖 | — | `Cargo.toml` 未改；默认依赖图与 M0c 快照 0 差异 | §2.1 |

**证据：未触碰的文件**（在 `crates/rapid-ocr-rs` 与父仓库分别检查）：

- `Temp/demo3-v2.html`：父仓库 `git status --porcelain -- Temp/demo3-v2.html` 为空；
- `docs/03-windows-only-optimization-tasks.md`：`git status --porcelain` 为空；
- `tests/baseline/`：`git status --porcelain -- tests/baseline` 为空（门槛输出写在 `target/m2-gate/`）。

---

### 与 `docs/05` §11「M2」验收清单的对照

| §11 M2 条目 | 本阶段 | 证据 |
| --- | --- | --- |
| `GET /api/models`、`POST /api/models/download`、下载任务进度 | ✅ 完成 | 交付物 1/2；`/api/models` 未改形状（M1 已有），下载端点真实化 + `download` 进度字段 |
| 单飞、空间检查、强制 SHA-256、失败清理、`MoveFileExW` 原子替换、重定向逐跳校验 | ✅ 完成（库层为唯一实现，M0b 起；M2 把前四条经 HTTP 真实驱动；M2b 把重定向改为逐跳校验并经真实 CDN 驱动） | 库层 48 个 `model_store` 测试（其中 7 条是重定向用例）+ HTTP 层 413/507/哈希失败/真实权重 302（M2b） |
| 下载取消（文件边界）与"目标已损坏时重新下载"（§6.3） | ✅ 取消（库 + HTTP 两层）；⚠️ §6.3 的替换只在库层 | 交付物 2；库 `an_existing_corrupt_target_is_replaced` |
| 模型齐备后惰性创建 engine 并显示耗时 | ✅ 完成 | 交付物 4；`/api/status.engine_load_ms` + reload 的 `load_ms` |
| **M2 验收**：无网络可验证的接缝 + 一个 opt-in 真实网络测试 | ✅ 完成 | 20 个 serve 测试（脚本化下载器 + 注入空间探测 + 闸门）与 2 个 opt-in 网络测试 |
| M3/M4 的条目（`annotated.png`、`export`、provider 运行期切换、公式模型） | ⛔ 不在 M2 | 见下表接缝 |

---

### 接缝（留给 M3 / M4 / 发布前）

1. ~~**【发布前阻塞项】默认表的 ONNX 权重不可下载**（本阶段的真实发现，见验证 4b）：ModelScope
   对 `onnx/**` 返回 302 到 `cdn-lfs-cn-*.modelscope.cn`，而 §6.1 第 2 条禁止自动重定向。要打通
   "点下载 → 模型齐备 → 识别"这条主线，必须先改 `docs/05` §6.1 第 2 条并实现**逐跳** host/path
   校验（或给出另一个可信的直连来源）。M2 不擅自放宽这条冻结契约。~~
   **【M2b 已解决，2026-10-04】** §6.1 第 2 条已改写为"禁止盲从 + 手工逐跳校验"，
   `ALLOWED_DOWNLOAD_HOSTS` 增加 `cdn-lfs-cn-1.modelscope.cn`，真实链路已跑通（见文末 M2b 记录）。
   保留的残余风险：CDN host 名变更时需要显式加一项（这是"白名单必须被审查"的设计后果，
   不是缺陷）。
2. **M3 的 provider 运行期切换**：`EngineState::Rebuilding` 与 `begin_rebuild` 仍无生产者；
   M2 的 `POST /api/engine/reload` **故意不经过它**（显式 reload = "按磁盘当前文件重建会话"，
   与"切换 provider"是两件事）。M3 需要"暂停新任务 → 排空 → 销毁旧 engine → 创建新 engine"，
   并把 `Rebuilding` 接到 `/api/status`。
3. **reload 会阻塞 accept 线程直到会话建好**（实测 `load_ms` 在毫秒级；真实模型几百毫秒到数秒）。
   M3 若要让 reload 异步化，应给出 `Loading` 期间的可轮询语义（现在是同步返回最终状态）；
   当前实现已保证 `/api/status` 在加载期间仍可读（不持有状态锁）。
4. **`GET /api/jobs/{id}/annotated.png` / `export`**（M3）与它们对原图编码字节的保留需求。
5. **公式模型集（M4）**：`ModelPlan` 固定 `ModelRequest::text_only`；下载层对公式集合已经可用
   （库的 `download_model_set_observed` 与集合无关），但 566 MB 的进度/体积提示、`?queue=formula`
   的生产来源仍未接线。
6. **下载 channel 容量是编译期常量**（`DOWNLOAD_QUEUE_CAPACITY = 4`）。若将来要暴露
   `--max-queue-download`，按 §3 的方式加参数与校验（当前不提供设了不生效的选项）。
7. **`/api/status` 的 `download_hosts` / `engine_load_ms`** 是本阶段新增的**诊断字段**（页面不读）；
   若 M3 的导出/诊断面板要用它们，需在 `docs/05` §4.2 的 `/api/status` 一栏登记。
8. **`JobView.failure` 与 `/result` 的错误体是两份序列化**（内容同源、形状不同：前者多了 `status`）。
   M4 若要统一（例如给 `/result` 也加 `status`），需先改 §4.2。

---

### 未覆盖风险（如实）

1. **未提交、未在干净 clone 上验证**：按要求不 commit；`crates/` 在父仓库被 `.gitignore` 忽略，
   证据来自当前工作树（`06bd0af` + 本阶段改动）。
2. **真实网络测试是 opt-in 且依赖上游内容**：`a_real_network_*` 只有设置
   `RAPID_OCR_ALLOW_NETWORK=1` 才联网；不设置时打印 `skipping`。上游若替换同名文件，
   哈希校验会**失败**（这正是期望行为），届时该测试会红——这是网络测试的正常语义，不是 flaky。
3. **多文件真实下载未跑通**：见"覆盖分工"第 4 条（第一次尝试在第二个文件遇到 10 s
   `connect_timeout`）。因此"逐文件递减预算在真实多文件场景下"只有本机 fixture（库层）与
   脚本化（HTTP 层）两层证据。
4. **页面改动只有 `node --check` 与注入测试**：真实浏览器里的进度条/取消按钮行为未人工复核。
5. **`--allow-download-host` 的高风险警告是 stderr 文本**（不是交互确认）：加了它就等于放行，
   没有"二次确认"这种机制（本机单用户工具，且 §6.1 只要求"打印高风险警告"）。
6. **`ServeError::InsufficientDiskSpace` 的"需求"是估算**：`download_bytes_total` 只统计
   `Missing` 且任一未知体积即 `None`，因此 serve 侧的任务级预检用
   `pending_download`（缺失 ∪ 损坏）并在有未知体积时按 `--max-download-mb` 计。这是保守估计，
   真正的逐文件判定仍在库侧（§6.5 的原文）。
7. **并发工作流**：父仓库工作树里还有其它工作流的未提交改动（`src-tauri/**` 等），与本阶段
   无关；本阶段的验证全部在 `crates/rapid-ocr-rs` 内取得。

---

# 【M2b】逐跳校验的重定向：解决 M2 的发布前阻塞项

- **时间**：2026-10-04（本机）
- **工作范围**：`crates/rapid-ocr-rs`（父仓库 `.gitignore` 的既有改动与本阶段无关）
- **未提交**：按要求不 commit；证据全部来自当前工作树
- **被解决的条目**：M2 的"【发布前阻塞项】默认表的 ONNX 权重不可下载"（见本文"验证 4b"与
  "接缝"第 1 条，两处已就地标注"已解决"）
- **改动文件**：`src/model_store.rs`、`src/exports.rs`、`src/bin/serve/error.rs`、
  `src/bin/serve/tests.rs`、`docs/05-local-web-demo-implementation.md`、本文件
- **`Cargo.toml` / 依赖**：未改（默认依赖图与 M2 快照 606 行逐字节相同）

## 1. 根因

不是"少了一个开关"，而是**契约与真实来源的合成结果**：

- `docs/05` §6.1 第 2 条冻结了 `redirect(Policy::none())`，实现里 3xx 一律
  `DownloadError::RedirectRejected`；
- 而 ModelScope 对**所有权重**（`onnx/**`）应答 **302 → `cdn-lfs-cn-1.modelscope.cn`**
  （LFS 对象存储），字典/词表才是直连 200。

因此 `POST /api/models/download` 对任何"含权重的默认表集合"必然在第一个文件失败
（502 `download_failed` / `detail.kind=redirect`）。§6.1 第 2 条本身已经写明未来要做的方式：
"必须逐跳校验 host/path"，M2b 就是把这句括号里的话实现出来，并把它写回契约。

## 2. 实现（`src/model_store.rs`）

### 2.1 重定向语义（逐条可定位）

| 规则 | 行为 |
| --- | --- |
| 不盲从 | 客户端仍是 `redirect(Policy::none())`；每个 3xx 都由 `follow_redirect` 自己读、自己判定 |
| 跳数上界 | `MAX_REDIRECT_HOPS = 5`（公开常量）。已跟随 5 跳后再收到 3xx → `RedirectRejected{location}`；`location` 是那次的 `Location` 原文 |
| 每跳 scheme | 必须是 `https`；`https → http` 降级 → `SchemeRejected{scheme:"http"}`（**测试策略放行明文的口子不顺延到跳转目标**：跳转判定用生产语义，即只认 `https`） |
| 每跳 host | 必须在**生效白名单**（编译期常量 ∪ 显式 `allowed_hosts`）内；否则 `HostRejected{host}`，错误里是**那个** host |
| 相对 Location | `current.join(location)`（RFC 9110 允许相对引用；绝对 URL / 绝对路径 / 相对路径三种形式都有用例） |
| 缺 Location | 3xx 没有可用的 `Location` → `RedirectRejected{location:None}` |
| 非 3xx | 立即返回响应：2xx 进入长度预检/流式上限/哈希校验；4xx/5xx 由调用方报 `Network` |
| 凭据 | 每个 hop 都走同一个 `send_request`（自己的 `User-Agent` + `Referer`），**没有**任何 `Authorization`/cookie/token 头的来源；有用例断言跳转目标收到的头里没有它们 |
| 落盘名字 | 来自**初始 URL** 的末段（不变）；跳转目标只决定"从哪里取字节" |
| 其余保证 | 全部作用在**最终**响应体上：`Content-Length` 预检、`take(max_bytes + 1)` 流式上限、唯一临时名、RAII 清理、强制 SHA-256、`MoveFileExW` 原子替换、单飞、磁盘预检、分项超时（超时按跳计） |

`DownloadError::RedirectRejected { location }` 保留（serve 侧仍是 502 `download_failed` /
`detail.kind=redirect`），只是语义从"任何 3xx"收窄为"超限或没有可用 `Location`"。

### 2.2 白名单：新增一项，而不是放宽规则

`ALLOWED_DOWNLOAD_HOSTS` 从 1 项变为 2 项：

```rust
pub const ALLOWED_DOWNLOAD_HOSTS: [&str; 2] = ["www.modelscope.cn", "cdn-lfs-cn-1.modelscope.cn"];
```

- 这是**逐串精确**比较（大小写不敏感），**没有**后缀/子域通配：
  `cdn-lfs-cn-2.modelscope.cn`、`cdn-lfs-cn-1.modelscope.cn.evil.example` 都不通过（有用例）；
- 它只作为 `Location` 目标出现，内容仍然必须匹配默认表声明的 SHA-256；
- "扩大白名单必须改常量 + 改测试"这条机制没有变：
  `the_allowed_download_hosts_are_exactly_the_declared_set` 现在逐项锁死这 2 项；
- 本地 `manifest.json` 仍然只能提供 URL，不能扩大白名单（原用例保持通过）。

### 2.3 一个**被实测否掉的**设计（如实记录）

最初实现把 §6.1 第 2 条的 "host/path 校验" 理解成"跳转目标的 URL 末段必须等于初始 URL 的末段"。
真实运行立刻否掉了它：真实 CDN 是 LFS 对象布局

```text
https://cdn-lfs-cn-1.modelscope.cn/prod/lfs-objects/f4/2c/0fbd…?filename=PP-OCRv6_det_tiny.onnx
```

末段是**对象哈希**，文件名只在 `?filename=` 查询参数里。该规则会让所有权重都下载失败
（真实网络测试观测到的错误是 `redirect`，`location` 正是上面这条 URL）。它被删除，理由不是
"为了通过测试"，而是**它本来就不承担安全责任**：写盘路径由初始 URL 决定、内容由声明的
SHA-256 决定，跳转改变不了其中任何一个。落盘名字的正确性由
`a_cdn_style_redirect_with_a_hash_path_still_lands_under_the_original_name` 锁住
（跳转到哈希路径 → 仍然写成 `model.onnx`）。

### 2.4 一个**被否掉的**测试观测方案（如实记录）

曾尝试加一个 `redirect_observation`（跳数 + 最终 host）供真实网络测试断言"确实跟随了 302"。
它在 `serve` 测试里恒为 0：`src/bin/serve/tests.rs` 属于 **bin** 目标，链接的是**未开 `cfg(test)`**
的库，因此库里所有 `#[cfg(test)]` 记录点都不存在。要让它在 bin 测试里生效，只能在**生产路径**上
无条件插入观测代码——为测试方便改生产路径在这个项目里不可接受。因此该方案被整体删除，
"跟随了 302" 改用**库外部**的 `curl.exe` 观测 + 内容哈希这两件互相独立的事实来证明（见第 4 节）。

## 3. 契约文档（`docs/05`）

- §6.1 第 1 条：明确"初始 URL 与**每一跳**都适用仅 HTTPS"；
- §6.1 第 2 条：由"禁止自动重定向"改写为"**禁止盲从** + 手工逐跳跟随"，并逐条冻结
  每跳校验、`N = 5`、相对 `Location` 解析、不携带凭据、不跟随非 3xx、跟随不改变其它保证；
  保留 OWASP SSRF 理由（"盲从等于把下载哪个地址的决定权交给上游"）；
- §6.1 第 3 条：白名单数值更新为 2 项并说明 CDN 只作为跳转目标；补"整串精确比较、不做后缀放宽"；
- §1.2 表格、§11 M2 清单、§12 验证计划的"下载"一行同步（重定向逐跳校验）；
- §7.2 安全模型：新增一行"下载的跳转（出站 SSRF）"，把规则写进安全表。

## 4. 测试

### 4.1 本机 fixture（库层，零公网；新增 8 条 `model_store` 用例）

| # | 用例 | 断言 |
| --- | --- | --- |
| 1 | `a_redirect_to_an_allowed_host_is_followed_and_verified` | 三台 fixture 服务器（入口 → 中间站 → 终点）；第一跳是绝对 URL、第二跳是**相对** `Location`；成功落盘且哈希匹配；入口 1 次 / 中间站 2 次 / 一台从未被访问的服务器 0 次 |
| 2 | `a_redirect_to_a_host_outside_the_allow_list_is_rejected` | 跳向 `localhost:{port}`（**可解析**，因此"盲从"会真的取到内容）→ `HostRejected{host:"localhost"}`；目录为空；越界 host **一次都没被连**（`request_count()==0`） |
| 3 | `a_redirect_chain_longer_than_the_hop_limit_is_rejected` | 服务器把 `/model.onnx` 永远指回自己 → `RedirectRejected{location}`；请求数恰好 `MAX_REDIRECT_HOPS + 1` |
| 4 | `a_redirect_without_a_location_header_is_rejected` | `302` 无 `Location` → `RedirectRejected{location:None}` |
| 5 | `a_redirect_that_downgrades_to_http_is_rejected` | `https → http`（明文端口上**确实有**服务器在监听）→ `SchemeRejected{scheme:"http"}`，且明文主机 `request_count()==0` |
| 6 | `a_redirect_target_receives_no_credential_header` | 记录跳转目标收到的**全部**头名：`authorization` / `proxy-authorization` / `cookie` / `x-rapidocr-token` / `x-auth-token` 一个都不在；`user-agent` 仍在 |
| 7 | `a_redirected_body_above_the_cap_is_still_rejected` | 跳转目标回 **chunked**（无 `Content-Length`，预检不可能顺手挡住）64 KiB，上限 4096 → `TooLarge{4096, Some(4097)}`，目录为空 |
| 8 | `a_cdn_style_redirect_with_a_hash_path_still_lands_under_the_original_name` | 真实 CDN 形状（哈希目录段 + `?filename=`）→ 接受，落盘名仍是 `model.onnx`，哈希匹配 |
| — | `the_redirect_hop_limit_is_small_and_fixed` | 静态断言 `MAX_REDIRECT_HOPS == 5` 且在 `[1,8]` 内（改它必须同时改 §6.1 第 2 条） |

被替换/改写的 1 条：`a_redirect_is_rejected_and_nothing_is_written`（旧的"任何 3xx 都拒绝"）
按**新行为**删除，由上面 8 条覆盖（不是弱化：旧断言是"拒绝一切跳转"，新断言是"白名单内放行、
越界/超限/降级/无目标拒绝、最终体仍受上限约束"）。

### 4.2 真实网络（opt-in）

命令（日志：`target/m2b-verify/network-test.log`）：

```powershell
$env:RAPID_OCR_ALLOW_NETWORK='1'
cargo test --features serve --bin rapidocr a_real_network -- --nocapture --test-threads=1
```

**(a) 权重：302 → CDN（M2b 打通的那条路径）**
`a_real_network_weight_download_through_the_cdn_redirect_lands_and_verifies` 走**默认表**
（不是夹具）的 v6-tiny 集合（det + rec + dict，三个真实文件，6,346,587 B）：

```text
real network download (default-table v6 tiny): {"cancel_requested":false,"download":{"bytes_done":6346587,"bytes_total":6346587,"current_file":null,"files_done":3,"files_total":3},"elapsed_ms":1006,"error":null,"failure":null,"id":"job-0000000000000000","kind":"model_download","position":null,"queue":"download","queued_ms":655,"started_ms":655,"state":"succeeded"}
real network download (weights): files_total=3 bytes_total=6346587 elapsed_ms=1006 max_hops=5 expected_weight_host=cdn-lfs-cn-1.modelscope.cn
real network download (weights): PP-OCRv6_det_tiny.onnx = 1829618 bytes, sha256 f42c0fbd294d95eac1a550e131b277dac97462c8025fa4b6c3cec1b7894bd3d5
real network download (weights): PP-OCRv6_rec_tiny.onnx = 4489813 bytes, sha256 e16e242de5937ad92609223f19bc2aff3727ee40b095f996907c24749bad251b
real network download (weights): ppocrv6_tiny_dict.txt = 27156 bytes, sha256 c5cbe34ef40c29c4df07ed012bf96569cb69a2d2a01a07027e9f13cb832bd9cd
test serve::tests::a_real_network_weight_download_through_the_cdn_redirect_lands_and_verifies ... ok
```

断言：任务 `succeeded`、3 个文件全部落盘、每个文件的 SHA-256 与默认表声明**逐位一致**、
`PP-OCRv6_det_tiny.onnx` 的内容等于默认表声明值、`/api/models` 变 `complete` 且 `missing:[]`、
无 `.part-*` 残留。**裸跳数**由库外部的 curl 独立观测（下节），测试只断言"内容来自那条 URL 的
302 之后"（字节哈希对上）。

**(b) 字典：直连 200（对照组）**
`a_real_network_download_of_the_default_table_dictionary_lands_and_verifies` 保持原样（本地
manifest + 一个真实字典 + 两个已就位夹具），仍成功：`bytes_done=74947 elapsed_ms=435`，
SHA-256 `b5f2bfe2…401c5d` 与默认表一致。

**(c) 独立观测（`curl.exe` 8.19.0，不带 `-L`；日志：`target/m2b-verify/curl-redirect-chain.log`）**

| URL | 结果 |
| --- | --- |
| `…/v3.9.1/onnx/PP-OCRv6/det/PP-OCRv6_det_tiny.onnx` | **302**，跳转体 345 B → `https://cdn-lfs-cn-1.modelscope.cn/prod/lfs-objects/f4/2c/0fbd…?filename=PP-OCRv6_det_tiny.onnx&…&auth_key=…` |
| `…/v3.9.1/onnx/PP-OCRv6/rec/PP-OCRv6_rec_tiny.onnx` | **302**，跳转体 345 B → `https://cdn-lfs-cn-1.modelscope.cn/prod/lfs-objects/e1/6e/242d…?filename=PP-OCRv6_rec_tiny.onnx&…` |
| `…/v3.9.1/paddle/PP-OCRv6/rec/PP-OCRv6_rec_tiny/ppocrv6_tiny_dict.txt` | **200**，27,156 B（`num_redirects=0`） |

结论（观测到的跳转链）：**来源 `www.modelscope.cn` ——1 跳→ 终点
`cdn-lfs-cn-1.modelscope.cn`**，跳数 1，远小于上界 5；字典 0 跳。与下载器"把 302 跟随到同一
host"的行为一致。

## 5. 门禁（`target/m2b-verify/m2b-gates.log`）

| # | 命令 | 结果 | 退出码 |
| --- | --- | --- | --- |
| 1 | `cargo fmt --all -- --check` | 无输出 | 0 |
| 2 | `cargo clippy --all-targets -- -D warnings` | 无 warning | 0 |
| 3 | `cargo clippy --features serve --all-targets -- -D warnings` | 无 warning | 0 |
| 4 | `cargo test --all-targets` | 382 + 2 + 4 + 14 + 0 = **402 passed, 0 failed** | 0 |
| 5 | `cargo test --features serve --all-targets` | 402 + **176** = **578 passed, 0 failed** | 0 |
| 6 | `cargo build --release --bins` | 0 | 0 |
| 7 | `cargo build --release --bins --features serve` | 0 | 0 |

- 第一次跑 3 号时我并行执行了 `cargo tree`，clippy 因等待包缓存锁而中断（该次日志里
  "exit=101 且无输出"）；随后**串行**重跑得到上面结果。另外 3 号第一次真实失败是
  `clippy::print_literal`（我在 `eprintln!` 尾部直接写了字面量）——已按 lint 修好，
  不是用 allow 掩盖。
- **依赖图未变**（`target/m2b-verify/tree-*.txt`）：

| 证据 | 值 |
| --- | --- |
| `cargo tree -e normal -p rapid-ocr-rs` | 606 行，SHA-256 `BD2AB5E41B1A6D649E2F80B0D3D3E55327B96EB7C6F861E55DFC7C8501C3F6FC`（与 M2 快照**逐行 0 差异**） |
| 与 M0c 快照 `target/m0c-tree-default.txt` 比较 | 0 差异 |
| `--no-default-features` | 605 行，与 `target/m0c-tree-no-default.txt` 0 差异 |
| `--features serve` | 611 行，与 M2 快照 0 差异；`tiny_http` 出现 1 次 |
| 默认树里的 `tiny_http` | 0 次 |
| `Cargo.toml` | 未改 |

## 6. 12 图硬门槛（输出写在 `target/m2b-gate/`，**没有**覆盖 `tests/baseline/`）

```powershell
target\release\bench_warm_e2e.exe --config ..\..\OCR-Model\test-config-small.yaml --images-dir ..\..\OCR-test-image `
  --warmup-rounds 1 --rounds 3 --max-side-len 2000 --intra-threads 16 --output target\m2b-gate\bench-cpu-2000.json
target\release\rapidocr.exe evaluate --manifest ..\..\OCR-test-image\golden-manifest.json `
  --config ..\..\OCR-Model\test-config-small.yaml --output target\m2b-gate\evaluation-cpu.json
```

| 门槛 | 文档要求 | 本次实测（release） | 比较方式 | 结论 |
| --- | --- | --- | --- | --- |
| 12 图区域数均值 | `34.833333333333336` | **`34.833333333333336`**（36 样本） | 原始 JSON 的数字**字面量**字符串精确比较 | 逐位相同 |
| 12 图 mean CER | `0.44765135645866394` | **`0.44765135645866394`**（12 例） | 同上 | 逐位相同 |

两条命令退出码均为 0；`git status --porcelain -- tests/baseline` 为空。
**为什么重跑**：下载路径不是推理路径（本次改动一行都没碰 `src/ocr/**`、`src/runtime/**`），
所以"没跑"在原理上也说得通；但这两个数字是发布前的唯一数值关卡，"跑了且逐位相同"与"合理推断
应该相同"是两种证据强度，而代价只有约 100 s，因此按要求实跑。

## 7. 关键行为对比（AGENTS.md §9）

| 项目 | 修改前（M2） | 修改后（M2b） | 预期结果 |
| --- | --- | --- | --- |
| 收到 3xx（修改功能） | 一律 `RedirectRejected` / 502 `redirect`；**默认表权重全部下载失败** | 白名单内、最多 5 跳、逐跳校验后跟随；越界/超限/降级/无目标仍是可定位拒绝 | 默认表权重可下载；SSRF 面不因跟随而扩大 |
| `ALLOWED_DOWNLOAD_HOSTS`（修改数据） | 1 项 `["www.modelscope.cn"]`，测试逐项锁死 | 2 项（+`cdn-lfs-cn-1.modelscope.cn`），测试仍逐项锁死 | 新增 host 必须显式审查；仍然精确匹配、无通配 |
| 落盘名字来源（原有功能） | 初始 URL 末段 | 初始 URL 末段（**不变**；跳转目标不参与命名） | 模型表声明名 = 磁盘名 |
| 大小上限（原有功能） | `Content-Length` 预检 + `take(max+1)` | 同上，作用在**最终**响应体（有针对跳转的 chunked 超限用例） | 跳转不成为绕过上限的口子 |
| host 白名单拒绝（原有功能） | 只可能发生在初始 URL | 初始 URL **与每一跳** 都发生 | 越界 host 一次都不被连 |
| 凭据（原有功能） | 无凭据头 | 无凭据头，且**跨跳不新增**（有断言） | 跳转目标收不到任何凭据 |
| 真实网络（新增证据） | 302 → 502 `redirect`（拒绝路径） | 302 → 3 个文件全部落盘 + 哈希一致 + `complete`（成功路径） | 主线"点下载 → 模型齐备"打通 |
| 推理链路 / 12 图门槛（原有功能） | 逐位相同 | **逐位相同** | 无退化 |
| 依赖（原有功能） | 606 行默认树 | 606 行，0 差异 | 未引入任何依赖 |

## 8. 未覆盖风险（如实）

1. **CDN host 名是硬编码的一项**：若 ModelScope 改到 `cdn-lfs-cn-2` 或其他域名，跳转会以
   `HostRejected`（502 `download_failed` / `detail.kind=host`）变红——这是"白名单必须显式审查"
   的设计后果，不是缺陷；修法是改常量 + 改锁死它的测试，或者由用户
   `--allow-download-host` 显式放行。本次沿 `det`/`rec`/`cls`/`rec_small` 四个权重 URL实测都落在
   `cdn-lfs-cn-1`，但没有"CDN 不会换名"的证据。
2. **真实网络测试是 opt-in 且依赖上游内容 + `--test-threads=1`**：`RAPID_OCR_ALLOW_NETWORK=1`
   才联网；上游替换同名文件会让哈希校验失败（这正是期望行为）。测试里没有"跳数"的内部断言
   （见 §2.4 的原因），跳数是 curl 的外部观测。
3. **跳转过程中的超时口径**：每跳各自受 `connect_timeout` / `read_timeout` 约束（与库原有
   "按阻塞等待计时"一致），因此 5 跳的**最坏**耗时是 5 个读取预算，而不是"整次下载一个总预算"。
   没有为整条链引入总时限（那会改变 §6.1 第 11 条的既有语义），如实记录。
4. **多文件真实下载的成功路径只在这一次跑通**：v6-tiny 集合 6.3 MB 用了约 1.0 s；M2 记录过的
   "第二个文件 10 s connect_timeout"是当时的偶发网络问题，本次没有复现，但也没有对它做加固
   （超时值未改）。
5. **未提交、未在干净 clone 上验证**：按要求不 commit；证据来自当前工作树。
6. **`--allow-download-host` 的高风险警告仍是 stderr 文本**（M2 的既有结论，本阶段未改）。

---

## M3：诊断与导出（`annotated.png` / 三格式导出 / 时间账本 / provider 运行期切换）

**阶段**：M3 —— `docs/05` §11「M3」的全部条目 + §4.2（两个新端点）、§4.5（原图保留与
`original_evicted`）、§4.6（有界序列化）、§7.5/§7.6（provider 运行期切换与 `Rebuilding`）、
§9.5（静态 HTML 导出与导出 CSP）、§10.6（诊断数据复用、不重新测量）。
M1/M2 记录里"留给 M3"的接缝（`annotated.png` 的原图字节、导出与 `ReportMode::Static`、
`POST /api/engine/reload` 的 `begin_rebuild`、诊断面板的数据来源）逐条收口。
**开工基线**：`dcf8583`（M0 `1144ddb`/`dbab12e` + M1 `06bd0af` + M2 `8532fd8` + M2b `dcf8583`
已提交，工作树干净）。
**日期**：2026-10-04（接在 M2b 记录之后）
**提交**：`（未提交：按要求不 commit）`

### 环境

| 项目 | 值 |
| --- | --- |
| target | Windows x64 + MSVC ABI：`x86_64-pc-windows-msvc` |
| 工具链 | rustc 1.98.1 / cargo 1.98.1 |
| crate | `rapid-ocr-rs` 0.7.0（独立 git 仓库，本阶段未提交） |
| 页面语法检查 | node v24.12.0（`node --check`） |
| 真实资产 | `OCR-Model/small/`、`OCR-Model/test-config-small.yaml`、`OCR-test-image/`（12 图 + golden） |

**变更规模**（`git diff --numstat`，15 个跟踪文件 +3112 / −256；其中本记录的追加 +420，
其余 14 个跟踪文件 +2692 / −256；另有新增文件 `src/bin/serve/export.rs` 433 行）：

| 文件 | 行数 | 改动 |
| --- | --- | --- |
| `src/output/html.rs` | 328 → 375 | `ReportMode::{Full,Static}`；脚本段抽成常量；两个渲染入口都收 `mode` |
| `src/exports.rs` | +8/−1 | 导出 `ReportMode` 与 `LoadImage`/`OcrInput`（复用库的唯一解码实现） |
| `src/bin/rapidocr.rs` | +3/−1 | `report` 显式传 `ReportMode::Full`（CLI 输出不变） |
| `src/bin/serve/export.rs` | **新增 433** | 标注 PNG、base64/data URL、三种导出文档、时间账本 JSON、`ExportFormat` |
| `src/bin/serve/error.rs` | 1008 → 1083 | `ExportTooLarge{limit,observed,annotated}`、`OriginalEvicted`(410)、`ProviderRejected`(400) |
| `src/bin/serve/jobs.rs` | 1322 → 1414 | `original_retained`、字节压力下**先释放原图**、`release_original`/`take_released_originals` |
| `src/bin/serve/results.rs` | 226 → 272 | 成功载荷改为 `Arc<OcrOutput>` + 实测序列化长度；`text_bounded` |
| `src/bin/serve/server.rs` | 1413 → 1781 | 原图保留区、`plan` 可切换、`annotated_png`/`export`/`job_snapshot`、`apply_provider`、worker 先建引擎再取任务 |
| `src/bin/serve/http.rs` | 714 → 938 | 两个新路由、导出头与导出 CSP、reload 的可选 provider 体、切换线程 |
| `src/bin/serve/state.rs` | +71 | `ServeConfigPlan::with_provider`（复用启动期规则） |
| `src/bin/serve/{mod,run}.rs` | +4 | 新模块与 `allow_provider_fallback` 接线 |
| `src/bin/serve/tests.rs` | 2716 → 3432 | 11 个新 HTTP 测试 + `SessionPlan`/`ReleaseOnDrop` 接缝 + 2 个按新行为改写的既有测试 |
| `src/bin/web/index.html` | +40/−4 | 诊断面板的时间账本/运行时段；两条错误文案 |
| `docs/05-local-web-demo-implementation.md` | +23/−1 | 三处**实现证明文档不完整**的更正（见文末） |

**没有新增依赖**：`Cargo.toml` 未改（base64 是自己写的 20 行 + RFC 4648 向量测试）。

---

### 交付物 1：`GET /api/jobs/{id}/annotated.png`（§4.2、§4.5）

**根因**：M1 在识别开始时就把编码原图**丢掉了**，而 §4.5 的字节账本仍然按
`--max-body-mb` 记着它——账目与事实分叉，`annotated.png` 因此不可能实现。
本阶段的修法是让"保留"名副其实（而不是把字节再读一次或长期保留解码结果）：

```rust
// server.rs
/// 保留的**原图编码字节**（只保留编码字节，绝不保留解码结果）。
struct RetainedOcr { bytes: Arc<[u8]>, max_side: Option<u32> }
// JobState.originals: HashMap<String, RetainedOcr>   ← 与 JobStore 同一把锁
// worker 取用时只克隆 Arc（一份图片都不复制）
fn retained_ocr(&self, id: &str) -> Option<(Arc<[u8]>, Option<u32>)>;
// 端点
pub fn annotated_png(&self, id: &str) -> Result<Body, ServeError>;

// export.rs（唯一实现）
pub(super) fn decode_original(bytes: &[u8]) -> Result<RecImage, ExportError>;   // 库的 LoadImage
pub(super) fn annotated_png(original: Arc<[u8]>, output: &OcrOutput) -> Result<Vec<u8>, ExportError>;
```

`decode_original` 走库里**同一个** [`LoadImage`]/[`OcrInput`]（编码字节上限、header 像素
探测、EXIF 方向、解码错误语义只有一份），叠加用 §1.1 指定的
`output::visualize::draw_output`，PNG 编码用 crate 既有的 `image`。

**状态码（逐条可定位）**：

| 情形 | 结果 | 依据 |
| --- | --- | --- |
| 任务不存在 | 404 `job_not_found` | §4.5 |
| 任务已淘汰 | 410 `job_evicted` | §4.5 |
| 排队/运行/失败/取消（没有区域可叠） | **409 `job_not_finished`** | 与 `/result` 同语义：不能凭空造一张图 |
| 结果在、原图被保留预算释放 | **410 `original_evicted`** | §4.2 |
| 正常 | 200 `image/png` | §4.2 |

**`original_evicted` 的生产者（新增的保留顺序）**：§4.2 冻结了这个 `code`，但 §4.5 原来的
"超限按最旧终态优先**淘汰整个任务**"会让它**永远不可达**（任务被淘汰时客户端拿到的是
`job_evicted`）。因此字节上限分两步：

1. **先释放最旧终态任务的原图**（`original_retained = false`，`retained_bytes` 同步减少）：
   任务记录与结果都留下，`/result` 与 `/api/jobs/{id}` 照常可用，只有 `/annotated.png`
   变成 410 `original_evicted`；
2. 原图都释放完仍超限 → 才淘汰整个终态任务（M0c 起的原有行为，仍是第二步）。

另外两条"原图再也用不上"的路径也立即释放（记账同步）：**失败**与**取消**的任务
（它们永远不会有注释图），以及被 TTL/数量上限淘汰的任务（随任务一起消失）。
活跃任务（`queued`/`running`）的原图**永不**释放：worker 还要用它解码。

**修改前后行为对比（交付物 1）**

| 项目 | 修改前（M1/M2） | 修改后 | 预期 |
| --- | --- | --- | --- |
| 编码原图 | 读完 body → 识别开始即丢弃（`pending.remove`）；`retained_bytes` 仍按它记账 | 识别后仍在保留区（`Arc<[u8]>`，一份），记账与事实一致 | §4.5 |
| `/annotated.png` | 路由不存在 → 404 `not_found` | 真 PNG（尺寸 = 解码后的原图；检测框由 `draw_output` 画出） | §4.2 |
| 原图被淘汰 | 无此状态（要么任务在、要么任务没了） | 任务在、原图没了 → 410 `original_evicted`（结果仍可读） | §4.2 |
| 字节上限超限 | 淘汰最旧终态任务 | **先**释放它的原图，**再**按需淘汰任务 | 让 §4.2 的 `code` 可达（docs/05 §4.5 已记） |

---

### 交付物 2：`GET /api/jobs/{id}/export?format=json|md|html`（§4.2、§4.6、§9.5）

**库侧**：`output/html.rs` 新增 `ReportMode::{Full, Static}`，两个渲染入口都收它：

```rust
pub enum ReportMode { Full, Static }
pub fn render_report(title, image_href, width, height, items, timing_summary, mode) -> Result<String>;
pub fn render_output_report(title, image_href, output, timing_summary, mode) -> Result<String>;
```

两个模式的**唯一**差异是常量 `REPORT_SCRIPT`（1570 字符，Full 才拼进去）：`Static` 的正文
不含任何 `<script`，而样式、SVG、列表与公式段一字不少。`Static` 的文档由导出响应的独立 CSP
（`script-src 'none'`）背书，因此不需要 nonce。

**serve 侧**：三种格式全部由**库的渲染器**产生，且都经有界写入器：

| 格式 | 来源 | 上限 | 附件名 / Content-Type |
| --- | --- | --- | --- |
| `json` | `export::result_json`（= `/result` 的**同一份**值：库 `to_output_json` + `plain_text` + `timing_ledger`） | `--max-result-mb`（§4.6，任务侧已强制）且 ≤ `--max-export-mb` | `ocr-<id>.json` / `application/json; charset=utf-8` |
| `md` | 库 `to_output_markdown` | `--max-export-mb` | `ocr-<id>.md` / `text/markdown; charset=utf-8` |
| `html` | 库 `render_output_report(.., ReportMode::Static)` + `data:image/png;base64,…` 内嵌 | `--max-export-mb`（含内嵌图片） | `ocr-<id>.html` / `text/html; charset=utf-8` + **导出 CSP** |

**HTML 的体积判定分两步，两步都不截断**：

1. **投影**：先用空 `image_href` 渲染一遍量出正文大小（data URL 只含 base64 字母与前缀，
   `escape_attr` 不改动其中任何字符，因此 `正文 + data_url` 就是最终长度）。超限时**根本不
   构造**那份大文档——`3200×2000` 的标注 PNG 远超 `--max-result-mb` 的那条风险在这里被挡住；
2. **有界写入**：最终文档仍经 `results::text_bounded`，任何意外都会变成 413 而不是一条内容
   缺失的文档。

超限 → **413 `export_too_large`**，`detail` 三个字段：`limit_bytes`、`observed_bytes`（**完整**
文档长度：json 用 worker 已测的 `serialized_bytes`，md/html 用渲染结果长度）、
`annotated`（`/api/jobs/{id}/annotated.png`，**不受导出预算约束**，直接可用）。
JSON 与 Markdown 不需要原图；HTML 需要——原图已被释放时是 410 `original_evicted`
（宁可不给，也不给一条图片链接失效的文档，§9.5 第 5 条）。

**导出 CSP（§9.5 第 2 条，逐字冻结，只加在 HTML 上）**：

```text
default-src 'none'; style-src 'unsafe-inline'; img-src data:; script-src 'none'; sandbox
```

主页面 CSP 仍是 nonce 且**不含** `unsafe-inline`（既有测试继续断言这一点）。

**页面改动（`src/bin/web/index.html`，`__CSP_NONCE__`/`__SRV_TOKEN__` 字面量未动）**：
`ERR_TEXT` 增加 `export_too_large`（文案明确指向"下载标注图"）与 `original_evicted`
（说明结果仍可看、重新上传即可再标注）。两个导出/下载按钮**本来就是**调这两个端点，
现在端点存在即可用，因此没有其它改动。

---

### 交付物 3：诊断面板的数据（§10.6、§11 M3）

**不重新测量任何库已经报告的东西**：

| 面板需要的 | 来源 | 状态 |
| --- | --- | --- |
| 逐阶段耗时 | `/result.timings`（库 `OcrTimings`）+ `/result.stages` | M1 已有 |
| **时间账本 + 口径残差 + 自解释文案** | `/result.timing_ledger`（库 `TimingLedger`） | **M3 新增** |
| ORT 指纹（已脱敏） | `/api/status.ort.version` + `.ort.fingerprint`（只给文件名/体积/SHA-256/DLL 名单） | M1 已有 |
| 峰值工作集 | `/api/status.memory.{peak_working_set_bytes, source}`（库 `runtime::memory`） | M1 已有 |
| provider 三字段 | `/api/status.provider.{requested, selected_ep, fallback_to_cpu}` | M1/M0c 已有 |
| 原图保留概况 | `/api/status.retention.retained_originals`（新增的诊断计数字段） | **M3 新增** |

`timing_ledger` 的字段全部由库计算，serve 不做任何算术：

```jsonc
"timing_ledger": {
  "total_ms": …, "input_preprocess_ms": …, … "unattributed_ms": …,   // TimingLedger 的全部字段
  "attributed_ms": …, "input_ms": …, "inference_ms": …, "rust_ms": …, // 库的同名方法
  "shares": { … } | null,                                            // LedgerShares
  "conservation": { "residual_ms": …, "excess_ms": …, "tolerance_ms": …,
                    "conserved": false, "interpretation": "…" }        // 库写的自解释文案
}
```

**面板必须显示"不是严格划分"这句话**：页面把 `conservation.interpretation` **原文**渲染出来
（`ledgerHtml()`，在折叠的诊断面板里），并且把 `residual_ms` / `excess_ms` / `tolerance_ms`
单列成表，因此"占比"不会被读成严格划分。这句文案是库写的（`conservation_interpretation`），
前端**不**自己改写成"仅供参考"——那样会丢掉可核对的口径（两个窗口串行、不是重复计时）。

---

### 交付物 4：provider 运行期切换（§7.5、§7.6、§11 M3）

**入口**：`POST /api/engine/reload` 的**可选**请求体 `{"provider":"cpu|directml|cuda"}`
（白名单式校验：恰好一个键；`allow_provider_fallback` **不能**从 API 改，它是启动期开关）。

**顺序**（`server.rs::apply_provider`，逐条对应 §7.6 的 M3 段）：

1. **先校验**：`ServeConfigPlan::with_provider(requested, allow_provider_fallback)` 就是
   启动期的**同一个** `validate`（名称/feature 判定 + §7.5 的 `fail_if_provider_unavailable`
   语义），非法 → **400 `bad_request`** + `detail.{provider, reason}`（`reason` 是库侧原文，
   例如 "DirectML provider support is not compiled in; rebuild with `--features directml-provider`"），
   **状态一字未动**；
2. **暂停新任务**：`Ready → Rebuilding`（§7.6 的合法边）；`POST /api/ocr` 此刻是
   `OcrAdmission::Queue` → **202 `queued`**（不是 409/503）；`/api/status` 报告**新**的
   `requested`，而 `selected_ep`/`fallback_to_cpu` 是 `null`；
3. **排空**：`engine_load` 已被本线程持有，worker 只有在拿到它之后才会去拿 `engine`，
   因此随后获取 `engine` 锁**恰好等到正在进行的那次推理结束**（不中断推理，§4.3）；
4. **销毁旧 engine → 创建新 engine**（顺序固定，避免失败时留下与 `/api/status` 不一致的引擎）；
5. `Rebuilding → Ready`（成功）或 `Rebuilding → Failed`；
6. **失败恢复旧 engine**（用旧配置重建会话）：成功 → 配置回到旧值、`outcome = "rolled_back"`
   且带 `error`/`rollback_ms`；连旧引擎也起不来 → 明确 `Failed`，`reason` 里两个原因都写。

**执行方式（M3 的设计决定）**：切换序列在**独立线程** `serve-provider-switch` 里跑、由那个
线程写响应；accept 线程立刻回到循环。理由是可验证性本身就是需求的一部分：如果在 accept 线程
上同步执行，`/api/status` 与 `/api/ocr` 会在整个序列（排空 + 两次建会话）期间停摆，那么
"`Rebuilding` 可见"与"新任务入队而不是被拒绝"这两条**根本不可能被观测**。客户端语义不变：
**请求只在序列结束或失败后才返回**。同时只允许一个切换在跑（`AtomicBool` + RAII 凭据），
第二个请求得到 **503 `busy`**（不排队）。

**worker 的一个必要改动**：会话创建从"取任务之后"移到"取任务**之前**"
（`ocr_worker`：`has_queued_work` → `ensure_engine_loaded(false)` → 才 `next_scheduled`）。
否则 worker 会把队列里的任务标成 `running`、然后阻塞在 `engine_load` 上干等切换结束——
排空判据（"没有推理在跑"）就不再干净，任务视图也会撒谎。改完之后：切换期间任务一律留在
`queued`，切换结束后它们用**新**引擎执行（测试用 `served` 记录逐次证明）。

---

### 验证 1：静态检查、feature 矩阵与依赖隔离

日志：`target/m3-verify/m3-gates.log`（1–7 号单次连续执行）、`target/m3-verify/tree-*.txt`。

| # | 命令 | 结果 | 退出码 |
| --- | --- | --- | --- |
| 1 | `cargo fmt --all -- --check` | 无输出 | 0 |
| 2 | `cargo clippy --all-targets -- -D warnings` | 无 warning | 0 |
| 3 | `cargo clippy --features serve --all-targets -- -D warnings` | 无 warning | 0 |
| 4 | `cargo test --all-targets` | 384 + 2 + 4 + 14 + 0 = **404 passed, 0 failed** | 0 |
| 5 | `cargo test --features serve --all-targets` | 404 + **203** = **607 passed, 0 failed** | 0 |
| 6 | `cargo build --release --bins` | 0 | 0 |
| 7 | `cargo build --release --bins --features serve` | 0 | 0 |

**依赖隔离（默认构建的依赖图逐字节未变）**：

| 证据 | 值 |
| --- | --- |
| `cargo tree -e normal -p rapid-ocr-rs` | 606 行，SHA-256 `BD2AB5E41B1A6D649E2F80B0D3D3E55327B96EB7C6F861E55DFC7C8501C3F6FC` |
| 与 M2b 快照 `target/m2b-verify/tree-default.txt` 比较 | **0 处差异**；与 M0c 快照同样 0 差异 |
| 与 `--no-default-features` / `--features serve` 快照比较 | 各 **0 处差异**（605 / 611 行） |
| 默认树里的 `tiny_http` | **0** 次（serve 树 1 次） |
| `Cargo.toml` | **未改**（本阶段没有新增任何依赖） |

**测试增量与基线对比（AGENTS.md §6）**

| 项目 | 修改前（M2b） | 修改后 | 说明 |
| --- | --- | --- | --- |
| `cargo test --all-targets` | 382+2+4+14 = 402 | **384+2+4+14 = 404** | +2：`output/html.rs` 的 `Static` 无脚本 + `Full` 脚本段钉住 |
| `cargo test --features serve --all-targets` | 402+176 = 578 | **404+203 = 607** | +29 净增（库 +2、serve 二进制 +27） |
| serve 各文件测试数 | `export` 0 / `http` 5 / `jobs` 22 / `results` 3 / `state` 21 / `tests` 41 | **6 / 8 / 26 / 5 / 22 / 52** | 新增模块与用例 |
| 删除/跳过/弱化的测试 | — | **0**（3 个既有测试按**新行为**改写，见下） |  |

**按新行为改写的 3 个既有测试（不是弱化）**：

1. `http::tests::the_router_matches_the_m1_subset_and_nothing_else` →
   `the_router_matches_the_documented_endpoint_set_and_nothing_else`：两个 M3 端点从"必须 404"
   变成真实路由（404 列表换成 `annotated`（无 `.png`）/`export/data` 等**仍然**不存在的路径，
   并新增"这两个端点只接受 GET"的 405 + `Allow: GET` 断言）；
2. `tests::unknown_paths_are_404_and_wrong_methods_are_405`：同样把两个端点移出 404 列表，
   改断言 **404 `job_not_found`**（路由存在、任务不存在）与 POST → 405 `Allow: GET`；
3. `jobs::tests::eviction_by_bytes_uses_the_original_plus_result_budget`：字节超限的**预期行为**
   变了（先释放原图而不是淘汰任务），断言随之改写为"a 的原图被释放、a 的结果仍在、账本减少"；
   整任务淘汰那条路径由新增的 `when_only_results_remain_the_oldest_terminal_job_is_evicted` 覆盖。

---

### 验证 2：12 图硬门槛（`target/m3-gate/`，**没有**覆盖 `tests/baseline/`）

```powershell
target\release\bench_warm_e2e.exe --config <OCR-Model>\test-config-small.yaml --images-dir <OCR-test-image> `
  --warmup-rounds 1 --rounds 3 --max-side-len 2000 --intra-threads 16 --output target\m3-gate\bench-cpu-2000.json
target\release\rapidocr.exe evaluate --manifest <OCR-test-image>\golden-manifest.json `
  --config <OCR-Model>\test-config-small.yaml --output target\m3-gate\evaluation-cpu.json
```

| 门槛 | 文档要求 | 本次实测（release） | 比较方式 | 结论 |
| --- | --- | --- | --- | --- |
| 12 图区域数均值 | `34.833333333333336` | **`34.833333333333336`**（36 样本） | 原始 JSON 的数字**字面量**字符串精确比较 | 逐位相同 |
| 12 图 mean CER | `0.44765135645866394` | **`0.44765135645866394`**（12 例） | 同上 | 逐位相同 |

`git status --porcelain -- tests/baseline` 为空。

---

### 验证 3：CLI 的 `report` / `run` 输出未变（**本阶段改了报告渲染器**）

`ReportMode` 是新增参数，因此必须证明 `Full` 的产物与本阶段之前相同。做法是**真实前后对比**
（`target/m3-cli-pin/`）：改动前用当时的 release 二进制把两张真图跑一遍 `rapidocr report` 与
一次 `rapidocr run --json`（`before/`），改动后（同一个 `report` 命令、同一个 `--config`、
同样的两张图）再跑一遍（`after/`）。

报告里嵌的是**实测耗时**（`total N ms`），它每次运行都会变，因此逐字节相同的比较对象是
"去掉那一处计时摘要之后的文档"：

```text
01基础多位置文本.html : html-without-timing identical=True | script block identical=True | script chars=1570
04表格与键值对.html    : html-without-timing identical=True | script block identical=True | script chars=1570
run --json regions: before=42 after=42
run --json texts identical=True
run --json first-point x identical=True
```

即：除 `total N ms` 这一处计时摘要外，`Full` 模式的报告**逐字节相同**（含 1570 字符的内联
脚本段）；`run --json` 的 42 个区域、逐区域文本与多边形坐标完全相同。
另有两条不依赖计时的单元断言钉住同一件事：
`output::html::tests::{full_mode_still_renders_the_exact_cli_script_block, static_mode_emits_no_script_at_all}`
（后者断言 `Full == Static + 脚本段` 这一等式）。

---

### 验证 4：M3 的 HTTP 端到端断言（`src/bin/serve/tests.rs`，真实绑定端口）

| 验收项 | 测试 | 关键断言（实测通过） |
| --- | --- | --- |
| 标注图成功 | `the_annotated_png_is_a_real_png_with_the_original_dimensions_and_drawn_boxes` | 200 `image/png`；`\x89PNG\r\n\x1a\n` 签名；解码后 100×50 = `image.original_size`；像素 (1,1) = 调色板第 0 色 `[255,0,0]`（证明 `draw_output` 真跑了）；`original_retained == true` |
| 原图淘汰 | `the_annotated_png_is_410_original_evicted_once_the_budget_releases_the_original` | 1 MiB 保留预算；`original_retained == false`；**410 `original_evicted`**；`/api/jobs/{id}` 仍 200、`/result` 仍 200 且字节数与释放前一致；`retention.retained_originals == 0`、`retained_bytes == 结果长度` |
| 三格式导出 | `every_export_format_is_served_as_an_attachment_and_the_html_is_static_and_offline` | JSON 与 `/result` **逐字节相同**；三种格式都有 `Content-Disposition: attachment; filename="ocr-<id>.<ext>"`；HTML 无 `<script`（大小写不敏感）、恰好一个 `src="` 且是 `data:image/png;base64,`、无 `/api/` 链接、含 `<style>`、带**导出 CSP**；主页面 CSP 不含 `unsafe-inline`；未知/缺失 `format` → 400 |
| 超限导出 | `an_export_over_the_limit_is_a_413_pointing_at_the_annotated_png` | 三种格式都 413 `export_too_large`；`detail.limit_bytes == 1 MiB`、`detail.annotated == /api/jobs/{id}/annotated.png`、`message` 含 `annotated.png`；json 的 `observed_bytes` = `/result` 长度（worker 已测）；**同一任务的 `annotated.png` 仍是 200**（"指向"必须真的可用） |
| 限额内导出 | `an_export_under_the_limit_is_served_intact` | 三种格式都 200 且非空 |
| 导出前置状态 | `exports_require_a_successful_job` | 失败任务：`annotated.png` 与 `export?format=md` 都是 409 `job_not_finished` |
| 诊断字段 | `the_diagnostics_payload_reports_the_library_ledger_fingerprint_and_memory` | `/result.timings` 含 `total_ms/preprocess_ms/detector_infer_ms/recognizer_infer_ms/postprocess_ms`；`timing_ledger` 含命名分量与 `attributed_ms/input_ms/inference_ms/rust_ms`；`conservation.residual_ms == -1.0`、`conserved == false`、`excess_ms == 1.0`；`interpretation` **含 `NOT a strict partition`**、`residual_ms = -1.000000`、`SCOPE difference`；`/api/status` 的 `ort.fingerprint{file,complete}`、`memory{peak_working_set_bytes,source}`、provider 三字段、`queues.text.wait_bound` 都在 |
| 切换成功 | `switching_the_provider_rebuilds_the_session_and_queues_requests_meanwhile` | 切换中 `/api/status.engine.state == "rebuilding"`、`provider.requested == "cpu"` 而 `selected_ep/fallback_to_cpu == null`；`POST /api/ocr` → **202 `queued`**；排空后的第一个任务已 `succeeded`；放行后响应 `outcome=ready`、`selected_ep=scripted-2`、`error=null`、工厂调用数 **2**；队列里的任务随后 `succeeded`，`/api/status` 的 `selected_ep` 也是 `scripted-2` |
| 切换失败回滚 | `a_failed_switch_rolls_back_to_the_previous_engine` | 响应 `outcome="rolled_back"`、`error` 含 "scripted session 2"、`rollback_ms` 是数字；`engine.state=ready`、`selected_ep=scripted-3`（恢复出来的旧会话）；工厂调用数 **3**；之后一次识别仍成功 |
| 回滚也失败 | `a_switch_that_cannot_restore_the_old_engine_lands_in_failed` | `outcome="failed"`、`engine.state="failed"`；`reason` 同时含两个原因（session 2 与 "restoring the previous engine" 与 session 3）；`/api/status` 的 `engine.reason` 与响应**同一句**；`POST /api/ocr` → **503 `engine_unavailable`** |
| provider 非法 | `requesting_a_provider_that_is_not_compiled_in_is_a_400_and_changes_nothing` | 400 `bad_request`；`detail.reason` 含 `directml-provider`（库侧原文）；工厂调用数仍为 **1**、`/api/status` 仍是 `scripted-1`/`ready`；服务照常可用（构建里真的编译了 DirectML 时该用例转为断言"切换确实发生"，两个分支都断言真实行为） |

---

### 关键行为对比（AGENTS.md §9）

| 项目 | 修改前（M2b） | 修改后（M3） | 预期结果 |
| --- | --- | --- | --- |
| 原图编码字节（修改） | 识别开始即丢弃（记账仍算它） | 留在保留区（只编码字节、`Arc` 共享；失败/取消/淘汰时释放） | §4.5，`annotated.png` 才有可能 |
| 字节预留顺序（修改） | 超限 → 淘汰最旧终态任务 | 先释放其原图（任务与结果保留），再按需淘汰任务 | §4.2 的 410 `original_evicted` 可达（docs/05 §4.5 已记） |
| `/annotated.png`（新增） | 路由不存在（404 `not_found`） | 200 `image/png`（库的唯一解码 + `draw_output`）；原图没了是 410 `original_evicted` | §4.2 |
| 结果载荷（修改） | 只留序列化 JSON 字节 | `Arc<OcrOutput>` + worker 实测长度（导出用库渲染器的前提） | §4.6，且内存不重复保留两份 |
| 报告渲染（修改） | 只有一种内联脚本的输出 | `ReportMode::{Full,Static}`；CLI 仍 `Full`（**逐字节相同**，见验证 3），导出用 `Static`（**无 `<script`**） | §9.5 第 1 条 |
| 导出（新增） | 路由不存在 | json/md/html 三格式；附件头；HTML 带**导出 CSP**；图片 `data:` 内嵌（真离线）；超 `--max-export-mb` → 413 `export_too_large`（`detail` 指向 `annotated.png`） | §4.6、§9.5 |
| 诊断面板（修改） | 只有逐阶段耗时 + ORT 指纹（无账本、无内存） | 增加库的时间账本（分量 + 残差 + `interpretation` 原文）与运行时/内存行 | §10.6、§11 M3 |
| provider 运行期切换（新增） | `Rebuilding`/`begin_rebuild` 无生产者 | 显式设置应用：校验 → `Rebuilding`（OCR 202 入队）→ 排空 → 销毁 → 重建 → `Ready`；失败回滚或 `failed`；同时只允许一个 | §7.5、§7.6 |
| OCR worker（修改） | 取任务 → 建引擎 | 建引擎 → 取任务（切换期间任务留在 `queued`，不假装 `running`） | 排空判据干净 |
| 页面（修改） | 无账本展示；未知错误码走通用文案 | 账本段（含 `interpretation` 原文）+ 运行时/内存段；`export_too_large`/`original_evicted` 文案 | §9.2 |
| 依赖 / 推理链路 | — | `Cargo.toml` 未改；三份依赖树与 M2b 快照 **0 差异**；推理链路一行未动，两个硬门槛逐位相同 | §2.1 |

**证据：未触碰的文件**

- `Temp/demo3-v2.html`：`git status --porcelain -- Temp/demo3-v2.html` 为空
  （SHA-256 `14871FED101D11451F9B799FD199144D6CEC7874C5682D0D630DED1F5E3D46EE`）；
- `docs/03-windows-only-optimization-tasks.md`：`git status --porcelain` 为空；
- `tests/baseline/`：`git status --porcelain -- tests/baseline` 为空（门槛输出写在 `target/m3-gate/`）；
- 页面模板的注入契约未动：`__CSP_NONCE__` ×4、`__SRV_TOKEN__` ×3、真实 `nonce="…"` 属性 ×3；
  真实 IIFE 块 69,168 字符，`node --check` exit 0。

---

### 与 `docs/05` §11「M3」验收清单的对照

| §11 M3 条目 | 本阶段 | 证据 |
| --- | --- | --- |
| 时间账本、ORT/provider 指纹、内存信息进入诊断面板（含口径说明） | ✅ 完成 | 交付物 3；`/result.timing_ledger`（库 `TimingLedger`）+ `/api/status.{ort,memory,provider}`；面板渲染 `interpretation` 原文 |
| `annotated.png`、Markdown/HTML 导出（HTML 走 `ReportMode::Static` + 独立 CSP，§9.5） | ✅ 完成 | 交付物 1/2；11 个 HTTP 测试 + 6 个 `export` 单测 |
| provider 运行期切换：暂停新任务 → 排空 → 销毁旧 engine → 创建新 engine → `rebuilding`；失败恢复旧 engine 或明确 `failed` | ✅ 完成 | 交付物 4；4 个 HTTP 测试（成功 / 回滚 / 回滚也失败 / provider 非法） |
| 测试：导出 HTML 可用且不含 `<script>`、CSP 头正确 | ✅ 完成 | `every_export_format_is_served_...`（含 `data:` 内嵌、附件头、导出 CSP）；库侧 `static_mode_emits_no_script_at_all` |
| M4 的条目（公式模型下载、公式队列、公式区域展示、CER 评估） | ⛔ 不在 M3 | 见下表接缝 |

---

### 接缝（留给 M4）

1. **公式模型与路由**：`ModelPlan::resolve` 仍固定 `ModelRequest::text_only`；
   `OcrRouting { formula: true }` 的生产来源（端点参数或 CLI）仍未接线，
   因此 `?queue=formula` 在生产路径仍是 400（M1 起的语义未变）；
2. **公式区域进入导出/诊断**：库的渲染器与账本已经支持 `formula_ms` / `formula` 区域
   （`to_output_markdown`、`render_output_report`、`TimingLedger.formula_ms`），但生产路径
   跑不出公式区域，因此这一层只有单元/夹具证据；M4 接上公式管线即可复用，无需新协议；
3. **评估（CER/精确匹配）**：复用库的 `evaluation`，不另写指标（M4 条目）；
4. **`--max-export-mb` 的图片部分**：当前把标注 PNG 与文档一起计入同一预算（§9.5 的原文），
   若 M4 要给"只导结果、不嵌图"的选项，需要先改 §9.5；
5. **`annotated.png` 的解码开销**：每次请求都重新解码原图（这是 §4.5"不长期保留 `RecImage`"
   的直接后果），大图上单次约几十 ms；没有做缓存（缓存会重新引入"长期保留解码结果"）。
   如需优化，应先改 §4.5；
6. **`POST /api/engine/reload` 的无 body 形式仍是同步的**（M2 的接缝 3 未变）：它在
   accept 线程上建会话，因此慢机器上会短暂挡住其它请求。M3 只为**provider 切换**做了
   异步化（那条路径长得多）；把无 body 的 reload 也异步化属于同一类改动，留给 M4；
7. **`serve-provider-switch` 线程与关闭**：切换进行中被关闭时，那个线程会继续跑完
   （它持有 `engine_load`）；若会话创建本身永远卡住（例如底层驱动挂死），
   `ServeRuntime::stop()` 的 join 会跟着挂住——这是 M2 起就存在的同类风险
   （accept 线程上的同步加载），M3 没有加剧，但也没有消除。

### 未覆盖风险（如实）

1. **未提交、未在干净 clone 上验证**：按要求不 commit；证据来自当前工作树（`dcf8583` + 本阶段改动）。
2. **provider 切换的"成功路径"没有真实加速器证据**：三份门禁都不带 `directml-provider`，
   因此 `with_provider` 的成功分支用的是脚本化引擎（`selected_ep = scripted-N`）。
   真实 EP 的切换只有"校验被拒"这一条走的是生产代码路径；若要真实的 DirectML/CUDA 切换证据，
   需要 `--features directml-provider`（或 CUDA）的构建与可用设备，本阶段没有。
3. **`original_evicted` 的场景是"单任务自身超过预算"**：测试用一个 1 MiB 保留预算 + 一张
   0.8 MiB 的原图 + 0.25 MiB 的结果触发它；"多任务竞争预算导致最旧任务的原图被释放"这条
   由 `jobs.rs` 的单元测试（`an_over_budget_store_releases_originals_before_evicting_jobs`）
   覆盖，HTTP 层没有第二个场景（不是遗漏，是不想为了覆盖面把测试写成时序敏感的）。
4. **HTML 导出的体积判定是"投影 + 有界写入"两步**：投影用"空 `image_href` 渲染一遍"量正文，
   依据是 data URL 只含 `escape_attr` 不改动的字符。若将来图片 URL 变成需要转义的形状
   （例如带 `&` 的查询串），这条等式不再成立——那时**第二步**有界写入仍会拒绝，不会发出超限文档。
5. **手工浏览器闭环未做**：页面里"下载标注图""导出 JSON/MD/HTML"的点击路径只做了
   HTTP 层与 `node --check` 验证；真实浏览器里的下载与诊断面板展开没有人工复核
   （M1/M2 的同类未覆盖风险仍在）。
6. **诊断面板展示的是英文长文案**：`interpretation` 是库写的英文（报告里的其它 `basis`/`note`
   也是英文），本阶段**没有**把它翻译成中文再展示——翻译本身就是第二套解释，容易与库原文分叉。
7. **`/api/status.retention.retained_originals` 是 M3 新增的诊断字段**，页面不读它
   （只用它做测试断言）；若前端要用，需要像 M2 的 `download_hosts`/`engine_load_ms` 那样
   在 `docs/05` §4.2 的 `/api/status` 一栏登记。

---

# 【M4】公式与评估：真实的第二队列、模型集接线与复用库的评估

- **时间**：2026-10-04（本机）
- **工作范围**：`crates/rapid-ocr-rs`（父仓库 `.gitignore` 的既有改动与本阶段无关）
- **未提交**：按要求不 commit；证据全部来自当前工作树
- **开工基线**：`c2f35d6`（M0 `1144ddb` + M0b `dbab12e` + M1 `06bd0af` + M2 `8532fd8` + M2b `dcf8583` + M3 `c2f35d6`）
- **阶段**：`docs/05` §11「M4」的全部条目 + §3 / §4.1 / §4.2（新增 4.2.1）/§4.4 / §5.3 / §5.4 /
  §8.3 / §9.2 / §9.4 / §10.8 / §11.1 / §12 里与本里程碑相关的冻结契约
- **被收口的接缝**：M1 接缝「M4（公式与评估）」4 条、M2 接缝 5（公式模型集）、M3 接缝 1–3
  （公式模型与路由、公式区域进入导出/诊断、评估复用库）
- **本阶段**：`docs/05` 的三处**文档修正**（§5.4 作用域、§4.2.1 新协议、§4.1 的唯一例外）、
  §3 的两个新选项、§8.3/§10.8/§11.1/§12 的补充。**没有改动任何库文件**（见"证据：未触碰的文件"）

## 环境

| 项目 | 值 |
| --- | --- |
| target | Windows x64 + MSVC ABI：`x86_64-pc-windows-msvc` |
| 工具链 | rustc 1.98.1 / cargo 1.98.1 |
| crate | `rapid-ocr-rs` 0.7.0（独立 git 仓库，本阶段未提交） |
| 真实资产（文本） | `OCR-Model/small/`（det/rec/dict）、`OCR-Model/test-config-small.yaml`、`OCR-test-image/`（12 图 + `golden-manifest.json`） |
| 真实资产（公式） | `OCR-Model/Formula-Recognition-Models/onnx/pp_formulanet_plus_m.onnx`（593,915,961 B）、`OCR-Model/Formula-Detection-Model/pix2text-mfd-1.5.onnx`（80,311,115 B）、`Formula-TestSet/` |
| 模型目录（证据用） | `target/m4-model-dir/`（4 个**硬链接**：三个文本文件 + 公式识别模型）、`target/m4-model-dir-noformula/`（只硬链接文本三个） |

**变更规模**（`git diff --stat`，12 个跟踪文件 + 1 个新文件：+1764 / −179）：

| 文件 | 改动 | 内容 |
| --- | --- | --- |
| `src/bin/serve/model_plan.rs` | +423 | `ModelRequest::text_and_formula`；按 **role 组**（`Pipeline`）分组的就绪判定；`spec_for`/`file_for` 唯一解析（含 `AmbiguousRole`）；公式识别/检测解析；廉价的 `missing_on_disk` |
| `src/bin/serve/server.rs` | +354 | M4 的 `OcrRouting`（带 `disabled_reason`）、`routing_for`、公式策略组装、公式 409（存在性预检）、`/api/models.formula` 块、`error_body` 唯一映射点、`text_request`/`recognize_with` 唯一引擎入口、评估单飞凭据 |
| `src/bin/serve/tests.rs` | +609 | 4 个 fixture 修正 + 15 个新测试（公式队列、公式 409、评估 4 条、页面就绪数据面） |
| `src/bin/web/index.html` | +143 | `QUEUE_ROLES.formula` 的角色修正；`formula` 块进 `normalizeModels`；`modelsReadyFor('formula')` 叠加路由门禁；评估小节（路径 + 表格） |
| `src/bin/serve/http.rs` | +107 | `/api/evaluate` 路由 + `spawn_evaluation`（独立线程、单飞）、公式队列的**读 body 之前**存在性预检、错误体映射收口 |
| `src/bin/serve/run.rs` | +62 | `--formula-detector` 启动期校验与解析（CLI > 模型集 role）、公式路由判据接线、启动日志两行 |
| `src/bin/serve/cli.rs` | +53 | `--formula-detector`、`--max-eval-cases`（选项面 21 → 23，逐项枚举测试同步） |
| `src/bin/serve/limits.rs` | +31 | `--max-eval-cases`（默认 32，≥ 1） |
| `src/bin/rapidocr.rs` | +28 | `evaluation_report_value`（CLI 与 serve 的评估报告**同一份**实现） |
| `src/bin/serve/error.rs` | +22 | `ServeError::Evaluation`（400 `bad_request` + `detail.reason`） |
| `src/bin/serve/evaluate.rs` | 新文件 | `POST /api/evaluate` 的请求体白名单、清单加载/限流/路径解析、复用 `evaluation::ocr` 的逐例评估 + 4 个单测 |
| `src/bin/serve/mod.rs` | +2 | 模块表与 `mod evaluate;` |
| `docs/05-...md` | +109 | 见上文"文档修正" |

**D1/D2/D3/D4 的签名（新增或改动的对外面）**：

```rust
// model_plan.rs —— role 分组的唯一实现
pub(super) enum Pipeline { Text, Formula }
impl Pipeline { pub fn of(role: ModelRole) -> Self }
pub(super) fn blocking_files(statuses: &[ModelSetStatus], pipeline: Pipeline) -> Vec<BlockingFile>
impl ModelPlan {
    pub fn missing_on_disk(&self, pipeline: Pipeline) -> Vec<String>;   // 只 stat，不哈希
    pub fn formula_recognizer(&self) -> Result<(PathBuf, String), ModelPlanError>;
    pub fn formula_detector(&self) -> Result<Option<PathBuf>, ModelPlanError>;
    fn spec_for(&self, role: ModelRole) -> Result<Option<&ModelFileSpec>, ModelPlanError>;
}
impl ModelReport {
    pub fn is_complete(&self) -> bool;              // 文本管线（每个文件 Present 且有哈希）
    pub fn formula_complete(&self) -> bool;
    pub fn formula_missing_names(&self) -> Vec<String>;  // 以及 corrupt / blocking
}

// server.rs
pub(super) struct OcrRouting { pub formula: bool, pub disabled_reason: Option<String> }
pub(super) fn routing_for(detector: Option<&Path>) -> OcrRouting;
impl ServeShared {
    pub fn formula_policy(&self) -> Option<FormulaPolicy>;
    pub fn formula_models_on_disk(&self) -> bool;
    pub fn formula_status_json(&self, report: &ModelReport) -> Value;
    pub fn formula_blocked_body(&self) -> Body;
    pub fn error_body(&self, error: &ServeError) -> Body;
    pub fn begin_evaluation(self: &Arc<Self>) -> Option<EvaluationGuard>;
    pub fn max_eval_cases(&self) -> usize;
}
pub(super) fn text_request(bytes: Arc<[u8]>, max_side: Option<u32>, formula: FormulaPolicy) -> OcrRequest;
pub(super) fn recognize_with(runtime: &ServeShared, request: OcrRequest) -> Result<OcrOutput, ServeError>;

// evaluate.rs / http.rs / cli.rs
pub(super) fn parse_manifest_body(bytes: &[u8]) -> Result<PathBuf, ServeError>;
pub(super) fn load_cases(manifest: &Path, limit: usize) -> Result<Vec<EvaluationCase>, ServeError>;
pub(super) fn resolve_image(manifest: &Path, case: &EvaluationCase) -> PathBuf;
pub(super) fn run(shared: &ServeShared, manifest: &Path) -> Result<Value, ServeError>;
pub(super) const IOU_THRESHOLD: f32 = 0.5;
fn spawn_evaluation(shared: &Arc<ServeShared>, manifest: PathBuf, request: Request) -> Result<(), Box<(ServeError, Request)>>;
// CLI：--formula-detector <ONNX>、--max-eval-cases <N>（默认 32）
// 协议：POST /api/ocr?queue=formula（队列即管线）、POST /api/evaluate {"manifest": "<path>"}
//      /api/models.formula = {complete, missing, corrupt, blocked, routing, disabled_reason,
//                             required_roles, detector:{configured,file}}
//      /api/status += formula 块 + limits.max_eval_cases
```

---

## 交付物 1：公式模型集接线（`ModelPlan` → `/api/models`）

**改动前的根因**：`ModelPlan::resolve` 固定请求 `ModelRequest::text_only`，因此
`/api/models` 只有一个集合，页面永远看不到 566 MB 的公式模型；而且 M1 的
`ModelReport::is_complete()` / `blocking_names()` 是"**所有集合的并集**"——一旦把公式集合
加进来而沿用这个口径，**公式模型没下载就会让普通 OCR 变成 409**。这是本阶段要修的根因，
不是"加个字段"。

**改动后**：

1. `resolve` 请求 `ModelRequest::text_and_formula(selection)`（库已有的入口，与
   `ModelRequest::required_roles()` 是**同一份** role 定义），并在启动期立刻解析一次
   `formula_recognizer()`：同一 role 声明两个不同文件时**启动期**就报
   `AmbiguousRole`（`spec_for` 是唯一解析实现，公式 role 与文本 role 走同一条判定）。
2. 就绪判定按 **role 组**（`Pipeline::of`）而不是按集合或集合顺序：默认表把两条管线放在两个
   集合里，本地清单把两条管线放在**一个**集合里，两种来源下结论一致。
   文本组 = detector/classifier/recognizer/dictionary/tokenizer；公式组 = formula_detector/
   formula_recognizer。
3. `/api/models` 的顶层 `complete`/`missing`/`corrupt`/`blocked` 收窄为**文本管线**作用域
   （= 引擎真正加载的文件 = `/api/ocr` 的 409 与 `EngineState::BlockedModelsMissing` 用的
   那一份），新增 `formula` 块给出公式管线的同一组字段 + `routing`/`disabled_reason` +
   `detector`（只给文件名）。`complete` 仍然要求"每个文件 Present **且**声明了哈希"（§5.2）。
4. 公式集合本身**一行代码都不用改**就走通了 M2 的下载路径：它是模型集里的一个集合，
   `/api/models` 报 `download_bytes_total=593915961`（≈566 MB），页面按集合渲染
   `下载「PP-FormulaNet_plus-M」· 566 MB` 的按钮（点击才下载，`data-set-id` 就是集合 id，
   没有"默认下 sets[0]"）。

**修改前后行为**（真实服务 + 真实模型，见验证 4/6）：

| 项目 | 修改前 | 修改后 |
| --- | --- | --- |
| `ModelPlan` 请求的管线 | `text_only` | `text_and_formula` |
| `/api/models.sets` | 1 个（文本） | 2 个（文本 + `PP-FormulaNet_plus-M`） |
| `/api/models.complete` 的作用域 | "所有集合"的并集 | **文本管线**（公式缺失不再影响它） |
| 公式模型缺失时 `POST /api/ocr`（文本） | —（当时公式集合根本不在清单里） | 仍然 **202 → succeeded**（这是必须成立的回归，验证 6 用真实模型跑了） |
| 同一 role 两个文件 | 文本 role 会拒绝 | 文本与**公式** role 都拒绝（`AmbiguousRole`，启动期） |

## 交付物 2：公式作为真实的第二队列

**协议决策（已写进 `docs/05` §4.2.1）**：队列类别**就是**管线选择，
即 `POST /api/ocr?queue=formula` 走公式管线、进公式队列；**不**增加 `formula=1`。
理由写在文档里：队列与管线在 §8.3 里本来就是一一对应的，再加一个布尔开关会多出
`queue=text&formula=1` 这种自相矛盾的组合，而页面本来就已经按开关发送 `queue=formula`
（原型既有代码，未改）。

**改动后**：

- `OcrRouting` 的含义从"M1 的固定关闭"变成"启动期判据 + 文字理由"：
  `routing_for(detector)` 只在配置了页面公式检测模型时打开（`--formula-detector`，或本地清单
  声明的 `formula_detector` role，CLI 优先）。**没有检测模型就不打开路由**（仍是 400），
  而不是"接了但永远产不出公式区域"——`FormulaPolicy` 在没有 `detector_path` 时只处理
  `input_regions`，HTTP 请求里没有这种区域，那会是一条"成功且零公式区域"的假路径。
- 公式队列的任务在 worker 里得到**由模型集组装**的 `FormulaPolicy`：识别模型路径 +
  **集合声明的 SHA-256**（库在加载时校验，这就是"下到的东西对不对"的权威判定）+
  检测模型路径。队列类别是唯一的管线选择点（`recognize()` 里一个 `match class`）。
- 公式队列的准入预检：`queue=formula` 且公式 role 的文件**不在磁盘上** → 在**读 body 之前**
  409 `models_missing`/`models_corrupt`，`detail.scope="formula"`、`missing`/`corrupt`/
  `missing_on_disk` 三个清单。预检只 `stat`（公式模型 566 MB，放进每个请求的准入路径会让
  吞吐崩掉），哈希判定留给加载时——这一点在 `detail` 与文档里都说清楚了。
- 页面门禁：`QUEUE_ROLES.formula` 从 `['formula_detector','formula_recognizer']` 改成
  `['formula_recognizer']`（= 库的 `ModelRequest::formula_roles()`），再叠加
  `/api/models.formula.routing`。**这是页面唯一必要的语义修正**：默认表刻意不把检测模型登记为
  集合成员（§5.1 的注释、`assets/default_models.yaml` 的 `formula:` 段），把它当必需角色会让
  开关**永远**不可用。不可用时行内给出文字理由（缺角色 → "缺少 公式识别"；无检测模型 →
  服务端原文），不只靠禁用态/颜色（§9.4）。

**修改前后行为**：

| 项目 | 修改前（M3） | 修改后 |
| --- | --- | --- |
| `POST /api/ocr?queue=formula` | 生产路径固定 **400**（路由关闭；只有测试注入的 `OcrRouting{formula:true}` 能达到） | 202（模型齐备+检测模型在场）/ **409**（公式模型缺失，公式作用域 detail）/ 400（没有检测模型） |
| 公式任务执行 | 不存在 | 真实公式管线：检测 → 抹白 → 文本管线 → 批量公式识别，`stages.formula=completed`、`timings.formula_ms` 有值 |
| 普通 OCR 与公式缺口 | — | 文本 409/引擎状态只含文本文件；公式缺失时普通 OCR 照常 202→succeeded |
| 页面公式开关 | 因角色列表错误而**恒不可用** | 公式集合齐备 + 路由可用才可勾选，否则给文字理由 |

## 交付物 3：公式区域进入结果与导出

**库侧本来就支持**（`RegionKind::Formula`、`OcrRegion.formula`、`timings.formula_ms`、
`to_output_markdown`、`render_output_report`、`TimingLedger.formula_ms`），M3 的记录已说明
"生产路径跑不出公式区域，因此这一层只有单元/夹具证据"。M4 接上管线后不需要新协议，
用真实模型逐项验证（验证 4）：

- `/api/jobs/{id}/result`：50 个区域里 **8 个 `kind="formula"`**，每个带 `formula.latex` +
  `formula.model_id="pp_formulanet_plus_m"` + `detection.score`；`timings.formula_ms≈5798`、
  `stages.formula.state="completed"`；
- `export?format=json`：**与 `/result` 逐字节相同**，解析后 formula 区域的 latex 列表与结果一致；
- `export?format=md`：latex 出现、`公式` 标记出现、无 `<script`；
- `export?format=html`：latex 出现（39 处 "公式/formula" 标记）、**无 `<script`**（`ReportMode::Static`）、
  附件头与导出 CSP 未变（M3 的测试仍在守这条）；
- 页面：区域列表本来就按 `kind === 'formula'` 渲染"公式"标签 + 虚线框（第二视觉线索，
  不只靠颜色），文本取 `formula.latex`；诊断面板的"逐阶段耗时"表会自动列出 `formula_ms`
  （它渲染 `timings` 里除 `total` 外的全部数值字段）。**因此页面无需为 D3 改动**——
  这也是本阶段"先验证再改"的结果，而不是"没改所以没做"。

## 交付物 4：评估（复用库的 `evaluation`）

**范围（明确说清做了什么、故意没做什么）**：

- 做：`POST /api/evaluate`，请求体**恰好** `{"manifest": "<本机清单路径>"}`（白名单式校验，
  与 `parse_set_id` 同一形状）；清单格式 = `rapidocr evaluate --manifest` 的
  `[{image, text, boxes}]`，`image` 相对清单所在目录解析（与 CLI 同一规则）；
  逐例调用 `evaluation::ocr::evaluate_case`（CER / 精确匹配 / 检测精度与召回 / 多边形 IoU），
  汇总用 `EvaluationSummary::from_cases`——**一行指标实现都没有另写**；
- 报告字段与 CLI **同源**：抽出 `crate::evaluation_report_value(&EvaluationSummary)`
  （库的质量指标 + `peak_working_set_bytes`/`memory_source`/`ort_runtime`/`ort_runtime_version`），
  CLI 的 `rapidocr evaluate` 与 serve 都调它；serve 额外加 `iou_threshold` 与
  `manifest_file`（只给文件名，§7.4 脱敏）；
- 执行方式：**独立线程 + 单飞**（`Documents/05` §4.1 记录的唯一例外，理由写进文档）。
  第二个并发评估 503 `busy`；推理走与 OCR 任务**同一条**引擎锁路径；`--max-eval-cases`
  （默认 32）给一次请求的时间上界；模型缺失/引擎不可用分别是 409/503，与 `/api/ocr`
  **同一份**错误体（`ServeShared::error_body` 是唯一映射点）。
- **故意没做**（都比"半成品 UI"更诚实）：没有逐例进度、没有取消（推理不可中断，§4.3）；
  没有把评估做成队列任务（那需要给结果类型加第二种载荷，而它的中间状态没有可轮询语义）；
  没有上传清单 + multipart（§2.2 明确不引入 multipart，因此页面的输入是**路径**，
  由用户手输/粘贴）。公式领域的指标集（`evaluation::formula`）**不在本端点范围内**：
  `rapidocr evaluate` 的口径就是文本 OCR 的质量，混进公式指标会造出第二套"默认汇总"，
  而库的 `evaluation` 模块文档明确要求两类指标不得混合。

**页面**：右侧"评估"小节 = 一行输入（清单绝对路径）+ "评估"按钮 + 一张小表
（均值 CER / 精确匹配率 / 用例数 / IoU 阈值 + 逐例 CER/精确/图像名列）。事件在 nonce 脚本里
绑定（无内联 `onclick`），失败时显示服务端 `detail.reason` 原文。

---

## 验证

### 1. 静态检查与 feature 矩阵（全部 exit 0）

| # | 命令 | 结果 |
| --- | --- | --- |
| 1 | `cargo fmt --all -- --check` | 0 |
| 2 | `cargo clippy --all-targets -- -D warnings` | 0 |
| 3 | `cargo clippy --features serve --all-targets -- -D warnings` | 0 |
| 4 | `cargo test --all-targets` | **384 passed**（lib）+ 2 + 4 + 14，**0 failed** |
| 5 | `cargo test --features serve --all-targets` | **384 passed**（lib）+ 2 + 4 + 14 + **218 passed**（bin，含 4 个 M4 单测模块与 15 个新测试），**0 failed** |
| 6 | `cargo build --release --bins` | 0 |
| 7 | `cargo build --release --features serve --bins` | 0 |

日志：`target/m4-gate/gates.log`。第 5 项的 218 个 bin 测试里，
`serve::tests`（真实端口 + 原始 TCP）52 → 60 个，其中 M4 新增：

```text
serve::tests::the_formula_queue_runs_the_formula_pipeline_from_the_model_set
serve::tests::an_incomplete_formula_set_is_409_while_text_ocr_keeps_working
serve::tests::removing_the_formula_model_turns_the_route_into_a_409
serve::tests::the_evaluate_endpoint_returns_the_library_summary
serve::tests::an_invalid_evaluation_request_is_a_locating_400
serve::tests::evaluation_needs_the_same_model_admission_as_ocr
serve::tests::a_second_evaluation_is_refused_while_one_is_running
serve::model_plan::tests::a_missing_formula_model_never_blocks_the_text_pipeline
serve::model_plan::tests::the_formula_roles_resolve_through_the_same_rule
serve::model_plan::tests::an_ambiguous_formula_recognizer_is_rejected_at_startup
serve::server::tests::the_routing_refuses_to_silently_use_the_formula_queue
serve::server::tests::the_formula_routing_is_decided_by_the_detector_alone
serve::evaluate::tests::{the_evaluate_body_carries_only_a_manifest_path,
                         a_bad_manifest_is_rejected_with_a_locating_reason,
                         case_images_resolve_relative_to_the_manifest,
                         the_iou_threshold_matches_the_cli_default}
```

本轮**没有**弱化/跳过/删除任何断言：改动过的测试只有三类合法原因（fixture 必须声明公式 role；
`/api/models` 的集合数与作用域按 M4 语义变化；`OcrRouting` 多了 `disabled_reason` 字段），
每一处都在测试里写明了"为什么期望变了"。

### 2. 依赖隔离（默认依赖图**逐位不变**）

| 命令 | 结果 |
| --- | --- |
| `cargo tree -e normal -p rapid-ocr-rs` | **606 行**，SHA-256 `BD2AB5E41B1A6D649E2F80B0D3D3E55327B96EB7C6F861E55DFC7C8501C3F6FC`（与 M2b/M3 记录**逐位相同**）；`tiny_http` **0** 次 |
| `cargo tree -e normal --no-default-features` | 605 行，`tiny_http` **0** 次 |
| `cargo tree -e normal -p rapid-ocr-rs --features serve` | 611 行，`tiny_http` **1** 次 |

`Cargo.toml` 未改；M4 也没有引入任何新依赖（评估用的是库自带的 `evaluation` + 已有的
`serde_json`）。

### 3. 12 图 HTTP 与 CLI 逐张一致（**12/12**，真实服务 + 真实模型）

`target/m4-evidence/run-12-images.ps1`（release `rapidocr.exe serve --model-dir target/m4-model-dir
--config OCR-Model/test-config-small.yaml`）：

```text
models: complete=True formula.complete=True formula.routing=False
01基础多位置文本.png   serve= 42 cli= 42 count=True texts=True
02多语言与RTL混排.png  serve= 21 cli= 21 count=True texts=True
03旋转与倾斜.png       serve= 13 cli= 13 count=True texts=True
04表格与键值对.png     serve= 61 cli= 61 count=True texts=True
05代码与等宽字体.png   serve= 38 cli= 38 count=True texts=True
06低对比度与深色背景.png serve= 21 cli= 21 count=True texts=True
07小字号与密集排版.png serve= 37 cli= 37 count=True texts=True
08数字公式与符号.png   serve= 51 cli= 51 count=True texts=True
09竖排文本.png         serve= 14 cli= 14 count=True texts=True
10长段落与分栏.png     serve= 37 cli= 37 count=True texts=True
11文字样式与特效.png   serve= 22 cli= 22 count=True texts=True
12综合压力测试.png     serve= 61 cli= 61 count=True texts=True
TOTAL serve=418 cli=418 images=12
ALL_12_MATCH=True      （逐张 regions 数、recognition.text 序列全等）
```

每张图的完整响应与 CLI 输出分别在 `target/m4-gate/serve-<name>.json` 与 `cli-<name>.json`。
注意这一次 `formula.routing=False`（没给 `--formula-detector`）：**生产默认形态下普通 OCR 与
M1 完全一致**，公式能力的存在没有改变文本路径。

### 4. 公式队列（真实 566 MB 模型 + 真实页面）

`target/m4-evidence/run-formula.ps1`（`--formula-detector ...\pix2text-mfd-1.5.onnx`）：

```text
models.source=default_table top.complete=True
formula.complete=True formula.routing=True detector=pix2text-mfd-1.5.onnx
formula set: id=PP-FormulaNet_plus-M complete=True bytes=0
  file pp_formulanet_plus_m.onnx role=formula_recognizer state=present
       size=593915961 sha256=71b6d389cf7b857e45252a4b98cfced1a3ffca7bf24d9497d02d052a41d9493b
status.formula.routing=True limits.max_eval_cases=32

POST /api/ocr?queue=formula  -> 202 {"kind":"ocr","queue":"formula","state":"queued"}
job state=succeeded queue=formula queued_ms=449 elapsed_ms=6519
regions=50 formula_regions=8
formula_timings: formula_ms=5798.48486328125 total_ms=6510.61767578125 stages.formula=Completed
  latex=\scriptstyle \int 0 ^ { \wedge } \infty ... = \sqrt { \pi } / 2  model=pp_formulanet_plus_m score=0.6838
  latex=\sqrt { 2 } \approx 1 . 4 1 4 \quad \pi \approx 3 . 1 4 1 5 9        score=0.6219
  latex=\scriptstyle \mathbf { a } ^ { 2 } + \mathbf { b } ^ { 2 } = ...      score=0.5917
  latex=\mathrm { X } \leq \mathrm { y } \geq ...                              score=0.5587
  latex=1 \quad / \quad \backslash ...                                         score=0.3750
  latex=C - 9 2                                                                score=0.3394
  latex=\rightarrow \leftarrow \uparrow \downarrow ...                         score=0.3322
  latex=6.022 \times 10^{23} / 1.6 \mathrm{e} - 19 / 3.0 \mathrm{E} + 8 ...   score=0.3111

CLI(同一策略: --formula-model --formula-detector): regions=50 formula_regions=8 formula_ms=5278.98
MATCH_regions=True  MATCH_texts=True  MATCH_formula_latex=True
export json: bytes=40485 carries_latex=True same_as_result=True formula_regions=8
export md:   bytes=1450  carries_latex=True has_script=False formula_markers=1
export html: bytes=672818 carries_latex=True has_script=False formula_markers=39
```

（用 `OCR-test-image\08数字公式与符号.png`：一页含 8 个公式区域；服务端与 CLI 的
区域数、文本序列、latex 序列**三者全等**。）

**页面就绪判定（不是"读代码觉得对"，而是把页面的函数跑在真实 JSON 上）**：
`target/m4-page-probe.mjs` 从 `src/bin/web/index.html` 抽出**同一份**内联脚本，在最小 DOM 桩里
执行后调用页面自己的 `modelsReadyFor`/`formulaBlockedText`/`renderBanner`：

```text
# 公式集合齐备 + 路由可用（models-formula.json）
{"text_queue":{"ready":true,...},"formula_queue":{"ready":true},"banner":{"hidden":true}}
# 公式模型缺失（models-incomplete.json）
{"text_queue":{"ready":true},"formula_queue":{"ready":false,"missing":["公式识别"],
 "reason":"缺少 公式识别"},
 "banner":{"hidden":false,"incomplete_sets":["PP-FormulaNet_plus-M"],
           "shows_set_button":true,"shows_formula_size":true,"offers_only_clicked_set":true}}
# 没有检测模型（models-nodetector.json）
{"formula_complete":true,"formula_routing":false,"text_queue":{"ready":true},
 "formula_queue":{"ready":false,"reason":"服务端未启用公式路由（--formula-detector）"}}
```

第二条同时证明了交付物 1 的"体积提示 + 按集合点击"：横幅里出现的正是
`下载「PP-FormulaNet_plus-M」· 566 MB`（`fmtBytes(593915961)`）且按钮带 `data-set-id`。

### 5. 双向公平性（**真实工作**，两个方向都成立）

`target/m4-evidence/run-fairness.ps1`，参数 `--max-queue-text 2 --max-queue-formula 1
--max-consecutive-text 2 --max-consecutive-formula 1`，上界取自 `/api/status`：

```text
queues: text(capacity=2 quota=2 wait_bound=2) formula(capacity=1 quota=1 wait_bound=2)

=== direction 1: formula flood, text victim ===
flood: accepted=1 refused(503)=9 completed=2
flood saturation observed: 10/10 status polls had used >= capacity
max other-class job elapsed_ms=5957
victim(text): state=succeeded queued_ms=524 elapsed_ms=1105 wall_ms=7016
bound: (wait_bound=2 + 1) * 5957 + 2000 ms = 19871 ms
DIRECTION_1_FORMULA_FLOOD__TEXT_VICTIM_OK=True

=== direction 2: text flood, formula victim ===
flood: accepted=4 refused(503)=12 completed=5
flood saturation observed: 16/16 status polls had used >= capacity
max other-class job elapsed_ms=5197
victim(formula): state=succeeded queued_ms=7988 elapsed_ms=4430 wall_ms=11749
bound: (wait_bound=2 + 1) * 5197 + 2000 ms = 17591 ms
DIRECTION_2_TEXT_FLOOD__FORMULA_VICTIM_OK=True

served_in_round: text=1 formula=0    queue used: text=1 formula=0
BOTH_DIRECTIONS_OK=True
```

要点（如实）：

- 两个方向的"洪水"都由**真实推理**产生（方向 1 是真实公式 OCR，方向 2 是真实文本 OCR），
  调度器与队列代码**一行未改**（M0c 冻结的策略）；
- 脚本断言的不只是"受害者等得不久"，还包括"洪水在窗口内**确实被服务过**"
  （完成的任务数 2 / 5，以及 10/10、16/16 次 `/api/status` 观测到 `used >= capacity`）；
  否则"没被饿死"没有意义；
- 方向 1 的受害者只等了 **524 ms**：两个队列都非空时调度器先取文本（本轮文本配额 2），
  于是文本任务排在一个**正在跑的**公式任务之后、在洪水任务之前被服务——这正是 §8.3
  保底规则想要的结果，比上界强得多；
- 方向 2 的受害者等了 **7988 ms**（≈1.5 个文本任务），远在 17591 ms 的界内；
- 这里的"界"用墙钟表达，因此额外留了 2000 ms 的调度/上报余量；**规则本身**
  （另一类最多被服务 `wait_bound` 次）由 `queue.rs` 的确定性单测证明（M0c/M1 保留）。

### 6. 不完整的公式集 / 没有检测模型（普通 OCR 必须照常）

`target/m4-evidence/run-incomplete.ps1`：

```text
# A：文本模型齐备、公式模型缺失、给了 --formula-detector
A: top.complete=True top.missing=
A: formula.complete=False formula.missing=pp_formulanet_plus_m.onnx formula.routing=True reason=
A: queue=formula -> HTTP 409 code=models_missing
   detail={"blocked":[...],"missing":["pp_formulanet_plus_m.onnx"],
           "missing_on_disk":["pp_formulanet_plus_m.onnx"],"scope":"formula",
           "model_dir":"<redacted>","source":"default_table"}
A: queue=text -> HTTP 202 queue=text
A: text job state=succeeded regions=42
A: page probe -> {"text_queue":{"ready":true},
                  "formula_queue":{"ready":false,"reason":"缺少 公式识别"}}

# B：模型齐备、**没有** --formula-detector
B: formula.complete=True formula.routing=False detector.configured=False
B: disabled_reason=formula routing is not enabled on this server: no page formula detector is
                   configured, ... Pass --formula-detector <ONNX> ... ordinary OCR is unaffected
B: queue=formula -> HTTP 400 code=bad_request
B: queue=text -> HTTP 202      B: text job state=succeeded
B: page probe -> {"formula_queue":{"ready":false,
                  "reason":"服务端未启用公式路由（--formula-detector）"}}
```

再加一条运行期变化（单元/HTTP 测试 `removing_the_formula_model_turns_the_route_into_a_409`）：
真删掉 `<model-dir>/fx.onnx` 后 `/api/models.formula` 立刻变 `missing`（不缓存启动期快照），
公式请求变 409，文本集合仍 `complete=true`。

### 7. 评估端点复现库/CLI 的数字（真实 12 图标注清单）

`target/m4-evidence/run-evaluate.ps1`（`OCR-test-image/golden-manifest.json`，12 例真实标注）：

```text
POST /api/evaluate -> HTTP 200 in 12.3 s
serve: cases=12 mean_cer=0.44765135645866394 exact_match_rate=0 mean_det_precision= iou_threshold=0.5
       manifest_file=golden-manifest.json
serve: ort_runtime_version=1.28.0 memory_source=windows:GetProcessMemoryInfo.PeakWorkingSetSize
       peak_working_set_bytes=1219923968
cli:   cases=12 mean_cer=0.44765135645866394 exact_match_rate=0 mean_det_precision=
MATCH_cases=True MATCH_mean_cer=True MATCH_exact_match_rate=True MATCH_per_case=True
mean_cer 原始字面量： serve=0.44765135645866394  cli=0.44765135645866394
SERVE_MEAN_CER_IS_GATE_LITERAL=True
逐例 CER 字面量全等（12/12）：01=0.48523622751236 02=0.608949422836304 03=0.470119535923004
04=0.329704523086548 05=0.657448709011078 06=0.379396975040436 07=0.536519408226013
08=0.390644758939743 09=0.0838709697127342 10=0.575593948364258 11=0.441422581672668
12=0.412908941507339
page table row 0: image=01基础多位置文本.png cer=0.48523622751236 exact_text=False
```

即：HTTP 端点的 `mean_cer` 与 CLI/硬门槛的 `0.44765135645866394` **是同一个浮点值的同一串
字面量**，逐例 CER 也一一相同（报告字段还包含 `ort_runtime`/`peak_working_set_bytes` 等
CLI 附加项）。响应保存在 `target/m4-gate/evaluate-serve.json` 与 `evaluate-cli.json`。

### 8. 12 图硬门槛（`target/m4-gate/`，**没有**覆盖 `tests/baseline/`）

```powershell
target\release\bench_warm_e2e.exe --config <OCR-Model>\test-config-small.yaml `
  --images-dir <OCR-test-image> --warmup-rounds 1 --rounds 3 --max-side-len 2000 `
  --intra-threads 16 --output target\m4-gate\bench-cpu-2000.json
target\release\rapidocr.exe evaluate --manifest <OCR-test-image>\golden-manifest.json `
  --config <OCR-Model>\test-config-small.yaml --output target\m4-gate\evaluation-cpu.json
```

| 门槛 | 要求 | 本次实测（release） | 比较方式 | 结论 |
| --- | --- | --- | --- | --- |
| 12 图区域数均值 | `34.833333333333336` | **`34.833333333333336`**（36 样本） | 原始 JSON 的**数字字面量**精确比较 | True |
| 12 图 mean CER | `0.44765135645866394` | **`0.44765135645866394`**（12 例） | 同上 | True |

补充：`tests/baseline/windows-baseline/bench-cpu-2000.json` 的 `regions.avg` 与本次
字面量相同；`tests/baseline/evaluation-small.json` 里存的是**低精度**的 `0.44765136`
（epoch-0 快照，位数更少）——这是既有事实，不是 M4 的差异：M1/M3/M4 三次 `*-gate/evaluation-cpu.json`
的字面量都是 `0.44765135645866394`。`git status --porcelain -- tests/baseline` 为空。

### 9. 环境变量门控的 `formula_integration_tests`

```powershell
$env:RAPID_OCR_MODEL_ROOT='D:\100_Projects\110_Daily\SnapClip\OCR-Model'
$env:RAPID_OCR_FORMULA_TEST_ROOT='D:\100_Projects\110_Daily\SnapClip\Formula-TestSet'
cargo test --lib formula_integration_tests -- --test-threads=1
```

**11 passed, 0 failed, 0 ignored**（81.02 s），日志里 `skipping test` 出现 **0** 次
（→ 真实加载了公式模型与真实页面）。日志：`target/m4-gate/formula-integration.log`。

### 10. im2latex-100 smoke：**跳过，理由如下**

本阶段**没有改动任何公式推理代码**：`git status` 显示改动的 12 个跟踪文件全部在
`src/bin/serve/*`、`src/bin/web/index.html`、`src/bin/rapidocr.rs` 与 `docs/05`，
`src/formula/**`、`src/ocr/**`、`src/evaluation/**`、`src/bin/formula_eval.rs` **一个文件都没有动**
（库文件 0 改动，因此依赖图逐位不变）。im2latex-100 smoke 跑的是 `formula_eval` 这条
公式推理路径，其代码与依赖在 M4 中完全没有变化；作为替代证据，公式路径由验证 4
（真实 566 MB 模型 + 真实页面，与 CLI 的 regions/text/latex 三者全等）与验证 9
（环境变量门控的公式集成测试，11 passed / 0 skipped）覆盖。若审查方要求"即使没改也复跑"，
命令与上一里程碑相同（`formula_eval --dataset im2latex --split test --limit 100`），
本阶段基于上面的理由选择不占用这段时间。

---

## 关键行为对比（AGENTS.md §9）

| 项目 | 修改前（M3） | 修改后（M4） | 预期结果 |
| --- | --- | --- | --- |
| `ModelPlan` 请求的管线 | `text_only` | `text_and_formula`；公式 role 缺失/歧义都在启动期报错 | §5.3、§5.4 |
| 就绪判定的粒度 | 所有集合的并集 | **按 `files[].role` 分组**（文本 / 公式） | §5.2、§8.1 |
| 公式模型缺失对普通 OCR 的影响 | （公式集合当时不在清单里，无从发生） | **无影响**：`/api/models.complete`、409 `detail`、`BlockedModelsMissing.missing` 都只含文本文件 | §7.6、§10.8 |
| `POST /api/ocr?queue=formula` | 生产路径固定 400 | 202 / 409（公式作用域 detail，读 body 前判定）/ 400（无检测模型） | §4.2.1、§4.4 |
| 公式任务执行 | 不存在（只有测试注入的路由） | 真实公式管线；`FormulaPolicy` 的模型与 SHA-256 来自模型集 | §5.3、§10.8 |
| 公式区域 | 生产路径跑不出来（只有夹具证据） | 8/50 个 `RegionKind::Formula` + latex；JSON/MD/HTML 三种导出都带上 | §9.2、§9.5 |
| 页面公式开关 | 角色列表错误 → **恒不可用** | 公式集合齐备 + 路由可用才可勾选；否则给文字理由（含服务端原文） | §9.4、§10.8 |
| 页面模型横幅 | 文本集合一个按钮 | 每个不齐备集合各自一个按钮，公式集合显示 `566 MB`，点击才下载 | §5.4、§9.2 |
| 评估 | 无 | `POST /api/evaluate`：复用库指标，字段与 CLI 同源 | §11 M4 |
| CLI 选项面 | 21 个 | 23 个（`--formula-detector`、`--max-eval-cases`），仍无 `--host`/`--ocr-workers` | §3、§7.1、§8.2 |
| 库 / 依赖 / 推理链路 | — | **库文件 0 改动**；`Cargo.toml` 未改；三份依赖树与 M3 快照 0 差异；两个 12 图硬门槛逐位相同 | §2.1 |

**证据：未触碰的文件**

- `Temp/demo3-v2.html`：`git status --porcelain -- Temp` 只列 `?? demo1/2/3.html`（本阶段之前
  就未跟踪）；`demo3-v2.html` 的 SHA-256 仍是
  `14871FED101D11451F9B799FD199144D6CEC7874C5682D0D630DED1F5E3D46EE`（与 M3 记录相同）；
- `docs/03-windows-only-optimization-tasks.md`：`git status --porcelain` 为空；
- `tests/baseline/`：`git status --porcelain -- tests/baseline` 为空（门槛输出写在 `target/m4-gate/`）；
- 页面注入契约未动：`__CSP_NONCE__` ×4（其中 3 个是真实 `nonce="…"` 属性）、`__SRV_TOKEN__` ×3；
  两个内联脚本各自 `node --check` exit 0（主脚本 73,542 字符）。

---

## 与 `docs/05` §11「M4」验收清单的对照

| §11 M4 条目 | 本阶段 | 证据 |
| --- | --- | --- |
| 公式模型下载（566 MB，显式点击 + 体积提示） | ✅ 完成 | 交付物 1；`/api/models` 报告 `download_bytes_total=593915961` 与 SHA-256；页面探针确认横幅渲染 `下载「PP-FormulaNet_plus-M」· 566 MB` 且按钮带该集合的 `data-set-id`（绝不自动下载、绝不回落 `sets[0]`） |
| 公式 OCR 走独立队列（§8.3） | ✅ 完成 | 交付物 2；验证 4（真实 202 `queue=formula` → succeeded，8 个公式区域）与验证 5（两个方向都用真实工作） |
| 公式区域展示与诊断 | ✅ 完成 | 交付物 3；验证 4：`/result` 8/50 个 `kind="formula"` + `formula_ms≈5798` + `stages.formula=completed`；三种导出都带 latex；页面本就按 `kind` 渲染"公式"标签 + 虚线框（第二视觉线索） |
| 上传标注样本 → CER / 精确匹配（复用 `evaluation`，不另写指标） | ✅ 完成（范围明确） | 交付物 4；验证 7：12 例真实标注，逐例 CER 与均值与 CLI **字面量全等**；7 个 HTTP/单元测试覆盖拒绝路径、准入与单飞 |
| **M4 的隐含验收**：普通 OCR 不受公式缺口影响 | ✅ 完成 | 验证 3（12/12，`formula.routing=False` 的默认形态）与验证 6（公式缺失/无检测模型时文本仍 202→succeeded） |

---

## 接缝（留给后续 / 发布前）

1. **公式检测模型没有可信下载来源**：`--formula-detector` 是本机路径，用户必须自己准备
   `pix2text-mfd-1.5.onnx`。将来若登记了可信来源，它应当作为 `formula_detector` role 进入
   默认表（`ModelRegistry::formula_model_set` 只做了一次 `ModelFileSpec::new`，不需要新抽象），
   届时 `--formula-detector` 自然退化为"覆盖"；`/api/models.formula.detector` 的形状不用改。
2. **`/api/models` 每次调用都会重新哈希 566 MB 的公式模型**（本机约 1 s，
   `target/m4-model-dir` 实测）。请求路径不受影响（公式准入只用 `stat` 的
   `missing_on_disk`），但页面若把 `/api/models` 轮询变密就会明显变慢。要改就得引入
   "带失效条件的缓存"（例如按 mtime+size），这是 M1 接缝 8 的放大版。
3. **评估是同步返回报告的**（§4.1 记录的唯一例外）：没有逐例进度、没有取消。要进度就得把
   它变成队列任务，并给结果类型加第二种载荷（`ResultStore` 目前只装 `Arc<OcrOutput>`）。
4. **评估的清单路径来自请求体**：带 token 的本机页面可以指向本机任意清单 JSON，并由其中的
   `image` 字段读本机图片。这是本机单用户工具既定信任模型（§7.1/§7.2）下的一致行为，
   但没有"路径必须在某个根目录内"的限制；要收窄需要新增 `--eval-root`（文档尚未要求）。
5. **页面的评估输入是路径而不是上传**：浏览器给不出本机路径输入体验（用户需手输/粘贴）。
   要上传清单 + 图片就得引入 multipart，而 §2.2 明确不引入 multipart 解析依赖。
6. **公式检测模型的执行 provider / 批次**沿用引擎的运行时档案（库内共享，
   `runtime_profile().session_runtime()`），serve 没有为它加开关——这是有意的：
   provider 结论只应有一处（§7.5）。
7. **无检测模型时公式路由整体不可用（400）**：这是刻意的取舍（见交付物 2），但意味着
   "文件都在、却没有检测模型"的用户拿不到任何公式区域。页面的文字理由与服务端
   `disabled_reason` 已经把这件事讲清楚；若将来把"显式区域"接进 HTTP（例如请求体里的
   `input_regions`），这条路由就可以在没有检测模型时也工作。

## 未覆盖风险（如实）

1. **未提交、未在干净 clone 上验证**：按要求不 commit；证据来自当前工作树（`c2f35d6` + 本阶段改动）。
2. **真实浏览器手工闭环未做**：公式开关的点击、566 MB 下载按钮、评估表格与路径输入都只做了
   数据面（真实 JSON）+ 页面函数探针（`node`）验证；没有人工在浏览器里点过（M1/M2/M3 的同类
   未覆盖风险仍在）。评估表格的 CSS 也没有在真实浏览器里复核。
3. **没有真的下载一遍 566 MB 公式模型**：本机已有该文件，证据用的是硬链接；下载路径本身与
   文本集合共用同一条库实现（`download_model_set_observed`，与集合无关），M2/M2b 已用真实网络
   验证过它；但"公式集合的 566 MB 从 ModelScope 下下来"这一步**没有**在 M4 重新跑。
4. **公平性验证是墙钟上界**：规则本身（另一类最多被服务 `wait_bound` 次）由 `queue.rs`
   的确定性单测证明；HTTP 层的墙钟数字受机器负载影响，因此加了 2000 ms 余量，并把
   "洪水确实被服务过"作为断言。上界与实测之间差一到两个数量级（524/7988 ms vs 19871/17591 ms），
   因此这个余量不影响结论，但它毕竟是余量而不是等式。
5. **`--max-eval-cases` 的默认 32 是新的**（§3 之前没有这一项）：一张 32 例的清单在真实图片上
   约 30–60 s（12 例实测 12.3 s），期间该端点占着一个线程与引擎锁；评估与 OCR 任务因此会互相
   排队（引擎只有一个 worker）。这是"单引擎"的固有约束（§8.2），不是 M4 引入的。
6. **`AmbiguousRole` 只验证到启动期**：本地清单里同一 role 两个文件（文本与公式各一例）都会
   拒绝启动；"两个**集合**声明同一 role 的不同文件"在默认表里无法构造（默认表的两个集合
   role 不重叠），因此那条组合只有代码路径上的同一实现，没有独立夹具。
7. **`tests/baseline/evaluation-small.json` 的精度差异**（见验证 8）是既有事实；本阶段
   没有修改任何 baseline 文件，但审查时容易误判，故在此点名。

---

# M1 评审修复轮：P1-1 / P1-2 / P2-1 / P2-2 / P2-3 / P2-4 / P3 + `/api/models` 性能

**基线**：HEAD `1b0f860`（M4 交付），**未提交**（按要求）。本轮**不改 `docs/03`**，
`Temp/demo3-v2.html` 与 `src/bin/web/index.html` 一个字节都没动
（前者 SHA-256 仍是 `14871FED…3D46EE`，与 M3/M4 记录相同；后者 `git status` 为空）。

**本轮的性质**：M4 交付后的独立评审发现 6 个实现缺口 + 1 处文档陈旧状态。全部按
"根因优先"修，不做最小补丁；`docs/05` 相应地改正了被实现否掉的表述（逐处见 §7）。

| 编号 | 根因（一句话） | 修复位置 |
| --- | --- | --- |
| P1-1 | 检测模型只传路径、集合声明的 SHA-256 被丢掉，加载时**从不校验** | `model_plan.rs` / `run.rs` / `server.rs` / `api.rs` / `formula/detect.rs` / `rapid_ocr.rs` |
| P1-2 | 公式准入只看**存在性**；会话缓存只按**路径**失效 | `model_verify.rs`（新）/ `model_set.rs` / `server.rs` / `http.rs` / `rapid_ocr.rs` |
| P2-1 | 队列容量"先检查、后入队"是两个临界区 | `admit.rs` / `http.rs` / `server.rs` |
| P2-2 | 无 body 的 `POST /api/engine/reload` 在 accept 线程上建会话 | `http.rs` / `server.rs` |
| P2-3 | `/api/evaluate` 接受任意本机路径 | `evaluate.rs` / `cli.rs` / `run.rs` / `server.rs` |
| P2-4 | 令牌熵来自时间/PID/计数器/栈地址（自认非密码学） | `security.rs` / `run.rs` |
| P3 | `docs/05` 第 3 行仍写"M0 待冻结，未进入实现" | `docs/05` |

**变更规模**：`git diff --stat` = 17 个跟踪文件 **+2138 / −311**，另加新文件
`src/model_verify.rs`（368 行，含 5 个单测）。

## 环境

| 项目 | 值 |
| --- | --- |
| target | `x86_64-pc-windows-msvc`（Windows x64 + MSVC ABI） |
| 工具链 | rustc 1.98.1 / cargo 1.98.1 |
| crate | `rapid-ocr-rs` 0.7.0（独立 git 仓库，基线 `1b0f860`，**未提交**） |
| 真实资产（文本） | `OCR-Model/small/`、`OCR-Model/test-config-small.yaml`、`OCR-test-image/`（12 图 + `golden-manifest.json`） |
| 真实资产（公式） | `OCR-Model/Formula-Recognition-Models/onnx/pp_formulanet_plus_m.onnx`（593,915,961 B）、`OCR-Model/Formula-Detection-Model/pix2text-mfd-1.5.onnx`（80,311,115 B）、`Formula-TestSet/` |
| 证据目录 | `target/review-gate/`（本轮全部门禁与实测）、`target/m1-review-evidence/`（脚本）、`target/m1-review-*/`（夹具） |

**改动过的对外签名（新增/改变的部分）**：

```rust
// src/model_verify.rs（新模块，库内唯一的"校验证据"入口）
pub struct FileIdentity { /* path + size + mtime */ }
impl FileIdentity { pub fn of(&Path) -> Result<Self>; pub fn describe(&self) -> String; }
pub struct VerificationOutcome { pub sha256: String, pub computed: bool, pub identity: FileIdentity }
pub struct VerificationStats { pub cold_verifications: u64, pub cache_hits: u64,
                               pub last_cold_micros: Option<u64>, pub last_cold_bytes: u64,
                               pub entries: usize }
pub fn verify_file(&Path) -> Result<VerificationOutcome>;          // computed 是**单次调用**的证据
pub fn sha256_file_cached(&Path) -> Result<String>;
pub fn verify_sha256(&Path, Option<&str>) -> Result<Option<String>>;
pub fn verification_stats() -> VerificationStats;
pub fn clear_verification_cache();

// src/model_set.rs
impl ModelFileSpec { pub fn state_in_probed(&self, root: &Path) -> (ModelFileState, bool) }
impl ModelSet      { pub fn status_probed(&self, root: &Path) -> (ModelSetStatus, usize) }
pub fn validate_model_files_probed(&[ModelFileSpec], &Path) -> (Vec<(ModelFileSpec, ModelFileState)>, usize);

// src/api.rs —— 与 expected_model_sha256 **同一条规则**的第二个字段
pub struct FormulaPolicy { /* … */ pub expected_detector_sha256: Option<String> }

// src/formula/detect.rs —— 与识别器**对称**的入口
impl FormulaDetector { pub fn from_model_with_hash(&Path, &RuntimeConfig, Option<&str>) -> Result<Self> }

// src/bin/serve/admit.rs —— 判定即预留
pub enum QueueAdmission<'a> { NotQueued, Reserve(&'a dyn QueueSlots) }
pub trait QueueSlots { fn reserve(&self) -> Option<QueueReservation> }
pub struct QueueReservation { /* Drop = 归还容量 */ }
pub struct Admit { pub max_body: u64, pub expected_body: Option<u64>, pub reservation: Option<QueueReservation> }

// src/bin/serve/model_plan.rs
pub(super) struct FormulaDetectorSpec { pub path: PathBuf, pub expected_sha256: Option<String> }
impl ModelPlan { pub fn resolve_formula_detector(&self, cli: Option<&Path>) -> Result<Option<FormulaDetectorSpec>, ModelPlanError> }
// 删除：ModelPlan::missing_on_disk（"只看存在性"的第二套清单，正是 P1-2 的缺口）

// src/bin/serve/server.rs
impl ServeShared {
    pub fn formula_models_ready(&self) -> bool;               // 取代 formula_models_on_disk
    pub fn formula_detector_status(&self) -> Option<FormulaDetectorStatus>;
    pub fn reserve_queue_slot(self: &Arc<Self>, class: QueueClass) -> Option<QueueReservation>;
    pub fn submit_ocr(&self, Vec<u8>, QueueClass, Option<u32>, Option<QueueReservation>) -> Result<Value, ServeError>;
    pub fn eval_root(&self) -> Option<&EvalRoot>;
}
// /api/models += verification 块；formula.detector += sha256/state；409 detail 去掉 missing_on_disk
// 删除：ServeShared::queue_full / ServeShared::apply_provider_request

// src/bin/serve/evaluate.rs
pub(super) struct EvalRoot { /* 规范化后的沙箱根 */ }
impl EvalRoot { pub fn new(&Path) -> Result<Self, String>;
                pub fn root(&self) -> &Path;
                pub fn resolve(&self, &Path, what: &str) -> Result<PathBuf, ServeError>;
                pub fn resolve_case(&self, &Path, &EvaluationCase) -> Result<PathBuf, ServeError> }

// src/bin/serve/security.rs
pub enum RandomError { Bcrypt { status: i32 }, Empty }
pub const RANDOM_SOURCE: &str = "windows:BCryptGenRandom(bcrypt.dll, BCRYPT_USE_SYSTEM_PREFERRED_RNG)";
pub fn random_source() -> &'static str;
pub fn random_hex(usize) -> Result<String, RandomError>;      // 旧签名返回 String（弱熵）
impl ServeToken { pub fn generate() -> Result<Self, RandomError> }
pub fn generate_nonce() -> Result<String, RandomError>;

// CLI：--eval-root <DIR>（选项面 23 → 24，逐项枚举测试同步）
```

## P1-1 公式**检测**模型的哈希被丢掉，损坏的检测模型照样加载

**根因**：`ModelPlan::formula_detector()` 只返回 `PathBuf`，`ModelFileSpec.sha256` 在解析处被丢弃；
`FormulaDetector::from_model` 不校验任何东西；`FormulaPolicy` 只有识别模型的
`expected_model_sha256`。于是完整性规则对两个公式模型**不对称**：识别模型有校验，检测模型没有。

**修复**（统一到**同一条**规则、**同一个**机制，不新增第二套）：

1. `FormulaPolicy` 增加 `expected_detector_sha256`（与 `expected_model_sha256` 对称）；
2. `FormulaDetector::from_model_with_hash(path, runtime, expected)` 成为**对称入口**，
   `from_model` 只是 `from_model_with_hash(.., None)`；校验发生在打开 ONNX **之前**；
3. `ModelPlan::resolve_formula_detector(cli)` 返回**路径 + 集合声明的哈希**：
   选中的路径正是模型集声明过的那个文件时（含 `--formula-detector` 指向同一文件、
   只是写法不同的情况，用 `canonicalize` 比较）声明的 SHA-256 随路径一起传递；
   CLI 指向集合没声明过的文件时如实为 `None`（"没有可信摘要"），而不是借用另一个哈希；
4. 端到端接线：`run.rs → ServeContext.formula_detector: Option<FormulaDetectorSpec> →
   ServeShared → formula_policy() → detect_formula_candidates() → from_model_with_hash`。

**修改前后行为**：

| 场景 | 修改前 | 修改后 |
| --- | --- | --- |
| 集合声明了检测模型 + 文件损坏 | 检测器**加载成功**（只检查存在性），公式任务产出错误结果或崩溃 | `HashMismatch{path, expected, actual}`（定位错误），绝不加载 |
| `/api/models.formula.detector` | 只有 `{configured, file}` | `+ {sha256, state}`（`state ∈ present/corrupt/missing`） |
| `--formula-detector` 指向集合声明的文件 | 声明的哈希被丢弃 | 声明的哈希随路径保留并校验 |
| `--formula-detector` 指向集合外的文件 | — | `sha256: null` + `state: present`（如实说明"只能证明文件在"） |

**证据（真实 80 MB 检测模型，`target/review-gate/detector-integrity.log`）**：

```text
A: formula.complete=True routing=True detector.state=present detector.file=mfd.onnx declared_sha256_matches=True
A: POST /api/ocr?queue=formula -> HTTP 202
A: job state=succeeded regions=50 formula_regions=8 detector_model=pp_formulanet_plus_m
A: VALID_DETECTOR_REALLY_LOADED=True
B: detector.state=corrupt formula.corrupt=mfd.onnx cold_this_call=1
B: POST /api/ocr?queue=formula -> HTTP 409 code=models_corrupt scope=formula corrupt=mfd.onnx detector_state=corrupt
B: raw bodyless request -> HTTP/1.1 409 Conflict in 4.4 ms
B: REFUSED_WITHOUT_READING_THE_BODY=True
C: valid bytes + wrong declared sha256 -> detector.state=corrupt declared=000…000 file=mfd.onnx
C: DECLARED_HASH_IS_ENFORCED=True
C: raw bodyless request -> HTTP/1.1 409 Conflict in 0.8 ms
C: REFUSED_WHEN_THE_DECLARED_HASH_MISMATCHES=True
```

B 的原始请求**声明 1 MiB 却一个字节都没发**：服务端 4.4 ms 就回了 409（若它先读 body，
这个请求会卡在 30 s 的读取超时里）。C 用的是**有效的**真实模型 + manifest 里一个错误的
声明摘要（必须重启服务：期望摘要是启动期解析的，与其它模型集决策同源）。

单元/集成层同样覆盖（不依赖资产的那两条在任何环境都跑）：

```text
formula::detect::tests::a_declared_detector_hash_is_verified_before_the_model_is_opened
formula::detect::tests::a_valid_detector_of_the_declared_hash_still_loads          （真实模型，env-gated）
serve::tests::a_corrupt_formula_detector_is_reported_and_refused_before_the_body
serve::tests::a_cli_formula_detector_is_verified_by_the_same_rule
serve::model_plan::tests::the_formula_detector_hash_travels_with_the_selected_file
```

## P1-2 损坏的公式模型溜过准入 + 流水线缓存会留住陈旧模型

**根因（两半）**：

1. `formula_models_on_disk()` 只 `stat`：损坏但存在的文件通过准入 → 客户端把整个 body 传完
   → 建出任务 → worker 里才 409 `models_corrupt`。`docs/05` 要求的是**读 body 之前**的 409；
2. `RapidOcrEngine` 的公式识别器/检测器缓存只比较**路径**：文件在第一次加载之后被替换或损坏，
   第二次请求发现"路径没变"就继续用内存里那份旧会话——改对了也不生效、坏了也不报错。

**修复**：

1. **新增库内的身份键控校验缓存**（`src/model_verify.rs`，唯一实现）：键 = `路径 + 体积 + mtime`，
   值 = 上一次真正算出的 SHA-256。三个原来看似无关的调用点
   （`/api/models` 的逐文件状态、公式队列准入、两条公式加载路径）收口到**同一份证据**上；
   冷验证真的读盘并记账（`VerificationStats`），命中只花一次 `stat`；每次调用都回答
   `computed`（**单次调用**的属性，因此"命中不重新哈希"可以被直接断言，而不是靠计数器差值推断）；
2. **准入按哈希状态**：`formula_models_ready()` 用与 `/api/models` 同一份报告判定，
   检测模型的状态也算在公式管线里；`missing_on_disk` 与被它在 409 里暴露的 `missing_on_disk`
   字段**删除**（那正是"第二套更弱的清单"）；
3. **会话缓存按文件身份失效**：`formula_recognizer`/`formula_detector` 的键从 `PathBuf` 改为
   `FileIdentity`；期望摘要由 `FormulaPolicy` 传进加载路径，**库自己**拒绝不匹配的文件。

**修改前后行为**：

| 场景 | 修改前 | 修改后 |
| --- | --- | --- |
| 公式模型存在但内容错 | 准入通过 → 读 body → 建任务 → worker 失败 | 读 body **之前** 409 `models_corrupt`，`detail.scope="formula"` |
| 检测模型存在但内容错 / 不在磁盘上 | 同上 | 同上（检测模型同属公式管线） |
| 模型文件在首次加载后被替换 | 继续用内存里的旧会话（按路径比较） | 身份变化 → 重新校验 → 不匹配即 `HashMismatch` |
| `/api/models` 每次调用 | 重新哈希每个文件（566 MB ~313 ms） | 身份未变 → 只 `stat`；`verification.cold_this_call == 0` |

**证据（真实资产）**：见上文 P1-1 的 A/B/C（B 与 C 同时是 P1-2 的准入证据），以及：

```text
target/review-gate/http-vs-cli.log:
second /api/models: cold_this_call=0 cache_hits=8
POLLING_DOES_NOT_REHASH=True

公式集成测试（真实 566 MB + 真实检测器，0 skipped）:
test ocr::pipeline::rapid_ocr::formula_integration_tests::a_replaced_formula_detector_is_reverified_instead_of_reused ... ok
```

单元层（不依赖资产，任何环境都跑）：

```text
model_verify::tests::{the_cached_digest_equals_the_uncached_one_and_is_only_computed_once,
                      a_size_change_forces_a_reverification,
                      a_mtime_change_forces_a_reverification,
                      a_mismatch_is_a_locating_error_and_a_match_passes_through,
                      a_missing_file_is_an_error_and_is_not_cached,
                      clearing_the_cache_only_costs_another_read}
serve::tests::{models_reuses_the_verified_digest_until_the_file_identity_changes,
               a_cli_formula_detector_is_verified_by_the_same_rule}
```

**如实的残留盲区**：身份由 `(size, mtime)` 近似，**体积不变且 mtime 不变**的内容替换
（例如 `SetFileTime` 把时间戳写回原值，或在同一时间戳粒度内原地改写）不会被识别为"变了"，
缓存因此会返回旧摘要；`mtime` 不可得时身份里是 `None`，同类替换同样落在盲区。
这一点同时写进 `src/model_verify.rs` 的模块文档、`docs/05` §4.2.1 与 `/api/models` 的
`verification.residual_blind_spot`（响应里就能读到，不必翻文档）。

## P2-1 队列容量预检不是原子的

**根因**：`queue_full()`（第一个临界区：读 `queued_len >= capacity`）与 `submit_ocr()` 里的
`enqueue`（第二个临界区）之间可以插入任意多个并发请求，每一个都已经把大 body 读进内存。
"拒绝时不读 body"因此在并发下不成立。

**修复**：把"检查"与"占位"合成**一个**动作——`admit()` 在第 4 步调用
`QueueSlots::reserve()`，服务端在**同一把 `jobs` 锁**里同时判定与占用（容量判定改成
`queued_len + reserved >= capacity`）；预留凭据随 `Admit` 一路传到 `submit_ocr`：
入队成功即提交（**先入队、后释放**：多算的那一格只会让并发请求被保守拒绝，绝不超卖），
任何提前返回/读 body 失败/panic 展开都由 `Drop` 归还容量。

**修改前后行为**：见下表（表里"容量 1"指 `--max-queue-text 1`）。

| 场景 | 修改前 | 修改后 |
| --- | --- | --- |
| 队列已满，N 个并发请求 | 理论上可能有请求通过预检、读入 body 后才拿到 503 | 全部 503，**没有一个**读 body（预留判定在读 body 之前） |
| 预留凭据被丢弃（请求后续失败） | 不适用（没有预留） | 容量归还，不泄漏；多释放也不会放大容量（`saturating_sub`） |

**证据**：`test result` 里的两条新用例（`target/review-gate/gates.log` 的 serve 段）：

```text
serve::tests::a_full_queue_rejects_every_concurrent_request_without_reading_a_body
serve::tests::the_queue_reservation_is_atomic_and_never_leaks_capacity
```

第一条：队列满时 8 个并发请求**全部** 503 `busy`，每个都声明 1 MiB 而**一个字节都不发**
（任何一个只要越过第 4 步就会卡在 30 s 读取超时里，测试因此会超时失败）。
第二条：8 个线程抢同一个容量 1 的队列，**恰好 1 个**拿到凭据；全部归还后容量完好
（下一次预留成功，再下一次仍然失败——既不泄漏也不放大）。

**如实说明**：`accept_loop` 目前是**单线程**串行处理请求（准入 → 读 body → 入队都在
`handle` 的一次调用里），因此这条竞态在**当前线程模型下**本来就不可达；修复的价值在于
把这条保证变成**数据结构本身**的性质，而不是依赖那个线程模型的事实。并发本身在
`reserve_queue_slot` 这一层被真正跑出来（8 线程竞争），测试注释里写明了这个分工。

## P2-2 无 body 的 `POST /api/engine/reload` 仍然阻塞 accept 线程

**根因**：`http.rs` 对**无 body** 的 reload 直接内联调用 `shared.reload_engine(None)`，
ONNX Runtime 建会话发生在 accept 线程上；而带 `{"provider":…}` 的那一支早已在独立线程里跑。

**修复**：把 `Dispatch::SwitchProvider(ProviderPreference)` 改成
`Dispatch::EngineWork(Option<ProviderPreference>)`，两种形态走**同一条** `spawn_engine_work`
线程路径（同一个"同时只有一个建会话序列"资格、同一个由那个线程写响应的契约）。
删除了因此不再需要的 `ServeShared::apply_provider_request`。

**修改前后行为**：

| 场景 | 修改前 | 修改后 |
| --- | --- | --- |
| 无 body 的 reload 期间 `/api/status` | 阻塞（直到建会话结束） | 立即返回，`engine.state = "loading"` |
| 无 body 的 reload 期间 `POST /api/ocr` | 阻塞 | 202 `queued`（`Loading` 期间入队） |
| 客户端何时拿到响应 | 序列结束后 | **不变**：序列结束（或失败）之后 |

**证据**：

```text
serve::tests::a_bodyless_reload_keeps_the_accept_loop_live
  闸门把第 2 次建会话按住 → /api/status 显示 loading；POST /api/ocr 返回 202 state=queued；
  放行后 reload 返回 200 outcome=ready load_ms 有值，建会话次数 = 2（明确是"重建"）。
```

## P2-3 `/api/evaluate` 可以读任意本机路径

**根因**：端点接受请求体里的**任意清单路径**，并按清单里的 `image` 字段（相对清单目录解析、
或绝对路径原样使用）读任意图片。loopback + token 让它不是越权入口，但它不是安全的发布默认值。

**修复**：新增 `--eval-root <DIR>` 沙箱：

- **默认不配置** → `/api/evaluate` 整体拒绝（400 `bad_request`，`detail.reason` 点名 `--eval-root`
  并要求显式开启）；拒绝发生在碰任何本机路径之前；
- 配置后，清单与清单引用的**每一张图**都必须 `canonicalize` 到该根之内：`..`、绝对路径逃逸、
  符号链接逃逸都在规范化之后暴露，拒绝时是**可定位**错误并点名违规路径（清单与图片分别点名）；
- 沙箱根在**启动期**规范化（不存在/不是目录 → 拒绝启动，`ServeStartError::EvalRoot`），
  绝不"先开着、第一次评估才发现沙箱是空的"。

**修改前后行为**：

| 场景 | 修改前 | 修改后 |
| --- | --- | --- |
| 没有 `--eval-root` | 接受任意清单路径 | 400，`detail.reason` 点名 `--eval-root` |
| 清单在沙箱外 | 读它 | 400，点名清单路径 + 沙箱根 |
| 清单里的图片越界（绝对路径/`..`/符号链接） | 读它 | 400，点名图片路径 |
| 根内的合法清单 | 工作 | 工作，且报告与 CLI **逐字段同值** |

**证据（真实 12 图标注清单，`target/review-gate/evaluate-sandbox.log`）**：

```text
outside sandbox -> HTTP 400 code=bad_request reason=the manifest `…\outside-manifest.json`
   resolves to `\\?\…\outside-manifest.json`, which is outside the --eval-root sandbox
   \\?\D:\100_Projects\110_Daily\SnapClip\OCR-test-image; evaluation only reads files inside that directory
OUTSIDE_REFUSED=True
POST /api/evaluate (in-root) -> HTTP 200 in 12.3 s
serve: cases=12 mean_cer=0.44765135645866394 exact_match_rate=0
cli:   cases=12 mean_cer=0.44765135645866394 exact_match_rate=0
MATCH_cases=True MATCH_mean_cer=True MATCH_exact_match_rate=True MATCH_per_case=True
raw literals: serve=0.44765135645866394 cli=0.44765135645866394
SERVE_MEAN_CER_IS_GATE_LITERAL=True    SERVE_LITERAL_EQUALS_CLI_LITERAL=True
```

（`SERVE_MEAN_CER_IS_GATE_LITERAL` 比较的是**原始 JSON 字面量**：PowerShell 的
`double → string` 只保留 15 位有效数字，经 `ConvertFrom-Json` 比较会把
`0.44765135645866394` 显示成 `0.447651356458664`——第一版脚本正是这样误报了一次 `False`，
改为正则取字面量后为 `True`。这一点如实记录，避免后来者重踩。）

单元测试：`serve::evaluate::tests::{the_eval_root_must_be_a_real_directory,
the_sandbox_resolves_inside_and_refuses_outside}`（后者用 `sandbox-elsewhere` 同级目录证明
"字符串前缀比较会误放行、规范化包含关系不会"）；HTTP 三条：
`{evaluation_is_refused_without_an_explicit_eval_root,
an_evaluation_manifest_outside_the_eval_root_is_refused,
an_evaluation_image_outside_the_eval_root_is_refused}`（第三条覆盖绝对路径、`..`
与符号链接；符号链接在无权限/无开发者模式的机器上会打印一行说明并跳过该子路径，
另外两条走的是同一条规范化包含规则）。

## P2-4 服务令牌不是 CSPRNG

**根因**：`random_hex` 用时间（秒+纳秒）+ PID + 进程内计数器 + 栈地址哈希派生，注释自认
"不是密码学 RNG"。令牌是防跨站触发的**共享密钥**，这些量对同机其它进程是可观察/可猜的。

**修复**：手写 `unsafe extern "system"` 绑定 `bcrypt.dll` 的 `BCryptGenRandom`
（`BCRYPT_USE_SYSTEM_PREFERRED_RNG`，不新增依赖，与 `runtime/memory.rs` 同一风格）：

- `ServeToken::generate()` 与 `generate_nonce()` 返回 `Result<_, RandomError>`；
- **fail-closed**：失败时 `run.rs` 直接**拒绝启动**（`ServeStartError::Random`），
  不存在"退回弱熵"的分支；启动日志打印熵源字符串（`RANDOM_SOURCE`）；
- 常量时间比较（`ServeToken::matches`）保持不变。

**证据**：

```text
serve::security::tests::the_token_is_random_per_run_and_compared_exactly（生成成功、64 hex、
   每次不同、熵源被报告、前缀/加长/首末字节翻转都不匹配）
serve::security::tests::a_failing_entropy_source_is_propagated_and_never_falls_back
   （注入一个必然失败的填充器 → RandomError 被传播、文案含 BCryptGenRandom 与 refuses to start；
     0 字节同样是错误；生产填充器在健康主机上成功）
```

**关于失败分支的诚实说明**：`BCryptGenRandom` 在一台健康的 Windows 上无法被弄失败，因此
**运行期**没有"RNG 真的返回非 0"的测试。失败分支通过 `random_hex_with` 的**注入点**被真实执行
（不是只留注释）：注入失败 → 错误被传播。启动期拒绝本身是一个 `?`（`run.rs` 里两处调用点），
没有独立的运行期用例——这是本轮**没有**做到的最后一米，如实记录。

## 性能：`/api/models` 的冷验证 vs 缓存命中（实测）

`target/m1-review-evidence/run-api-models-latency.ps1`（真实 release 服务 + 真实 566 MB 公式模型；
"冷"用**只改 mtime**的方式让同一个文件的身份失效，因此走的是与旧实现完全相同的
"重新读盘并哈希"路径，而不是估算）：

```text
startup: cold_this_call=0 cold_verifications=4 wall_ms=21.78

round,wall_ms,cold_this_call,cold_verifications,cache_hits,last_cold_ms,last_cold_bytes
cold round 1 (identity invalidated),312.7,1,5,7,311.016,593915961
cold round 2 (identity invalidated),317.62,1,6,10,316.509,593915961
cold round 3 (identity invalidated),315.03,1,7,13,313.779,593915961
cache hit round 1,0.81,0,7,17,313.779,593915961
cache hit round 2,0.65,0,7,21,313.779,593915961
cache hit round 3,1.34,0,7,25,313.779,593915961
cache hit round 4,0.66,0,7,29,313.779,593915961
cache hit round 5,0.55,0,7,33,313.779,593915961

COLD   /api/models (566 MB re-hashed): mean=315.12 ms over 3 rounds; per-call cold_this_call=1
CACHED /api/models (identity unchanged): mean=0.80 ms, max=1.34 ms over 5 rounds; per-call cold_this_call=0
SPEEDUP: 393.9x
```

- **冷验证**（= 旧实现**每一次** `/api/models` 的成本，也是页面 8 s 轮询一次的成本）：
  566 MB 读+哈希 **≈313 ms**，端到端 **≈315 ms**；`verification.last_cold_ms/last_cold_bytes`
  就是这次实测的原始数字（313.779 ms / 593,915,961 B）。
  注：这里磁盘页缓存是热的（≈1.9 GB/s）；`docs/06` M4 一节记的"约 1 s"是更冷的状态，
  两者不矛盾，本轮数字是**本机当下的实测**。
- **缓存命中**（稳态）：**0.80 ms**（max 1.34 ms）——只做 5 次 `stat` + 5 次内存比较，
  与文件大小无关；端到端 **约 394×**。
- 页面 8 s 轮询的真实行为由 `target/review-gate/http-vs-cli.log` 的
  `second /api/models: cold_this_call=0 cache_hits=8` / `POLLING_DOES_NOT_REHASH=True` 佐证。

## 验证

### 1. 静态检查、feature 矩阵与 release 构建（全部 exit 0）

`target/review-gate/gates.log`：

| # | 命令 | 结果 |
| --- | --- | --- |
| 1 | `cargo fmt --all -- --check` | 0 |
| 2 | `cargo clippy --all-targets -- -D warnings` | 0 |
| 3 | `cargo clippy --features serve --all-targets -- -D warnings` | 0 |
| 4 | `cargo test --all-targets` | **393 passed**（lib）+ 2 + 4 + 14 + 0，**0 failed** |
| 5 | `cargo test --features serve --all-targets` | **393 passed**（lib）+ 2 + 4 + 14 + **232 passed**（bin），**0 failed** |
| 6 | `cargo build --release --bins` | 0 |
| 7 | `cargo build --release --features serve --bins` | 0 |

本轮新增 **9 个库单测**（384 → 393）与 **13 个 bin 测试**（219 → 232），没有跳过、没有弱化、
没有删除任何既有断言。改动过的既有测试只有三类**预期行为发生正确变化**的原因：

1. 公式路由的测试夹具必须让检测模型**真的在磁盘上**（新的准入按哈希状态判定；
   `with_formula_routing` 现在会写一份占位检测模型，与 `run.rs` 的启动期不变量一致）；
2. `an_incomplete_formula_set_is_409_while_text_ocr_keeps_working` 里的
   `detail.missing_on_disk` 断言随该字段一起删除，替换为**更强**的断言：
   `detail.detector.state == "present"` 且 `detail.sha256 == null`（"没有可信摘要"被如实报告），
   并断言该字段确实不存在；
3. `admit.rs` 的准入顺序用例的 `queue_full: true` 改成
   `QueueAdmission::Reserve(&FullQueue)`（测试替身），并**新增**两条断言：
   第 4 步只调用一次、以及第 5 步失败时预留被归还。

### 2. 依赖隔离（三份依赖树与 M4 快照**逐行 0 差异**）

`Cargo.toml` 未改，本轮也没有新增任何依赖（`bcrypt.dll` 是系统组件，用手写绑定调用）。

| 命令 | 结果 |
| --- | --- |
| `cargo tree -e normal -p rapid-ocr-rs` | **606 行**，与 `target/m4-gate/tree-default.txt` **逐行相同**（规范化换行后 SHA-256 `CB12F133…59DF52`，三次采样稳定）；`tiny_http` **0** 次 |
| `cargo tree -e normal --no-default-features` | **605 行**，与 M4 快照逐行相同；`tiny_http` **0** 次 |
| `cargo tree -e normal -p rapid-ocr-rs --features serve` | **611 行**，与 M4 快照逐行相同；`tiny_http` **1** 次 |

（`docs/06` 早期里程碑记的 `BD2AB5E4…` 是另一种字节序列化口径下的哈希；本轮改用
"与保存的 M4 树文件逐行比较 + 规范化换行后的 SHA-256"，两者都指向同一个结论：**逐行 0 差异**。）

### 3. `cargo package --allow-dirty`（库文件被改动，因此重跑）

```text
Packaged 190 files, 14.0MiB (3.3MiB compressed)
Verifying rapid-ocr-rs v0.7.0 (…\target\package\rapid-ocr-rs-0.7.0)
Finished `dev` profile [unoptimized + debuginfo] target(s) in 16.63s      （exit 0）
```

`cargo package --list` 里 `src/model_verify.rs`、`src/bin/web/index.html`、
`src/bin/serve/model_plan.rs` 都在；打包树上的 `cargo check --features serve --all-targets`
（`target/package/rapid-ocr-rs-0.7.0`）**exit 0**（31.64 s），日志：`target/review-gate/package-serve-check.log`。

### 4. 12 图硬门槛（`target/review-gate/`，**没有**覆盖 `tests/baseline/`）

```powershell
target\release\bench_warm_e2e.exe --config <OCR-Model>\test-config-small.yaml `
  --images-dir <OCR-test-image> --warmup-rounds 1 --rounds 3 --max-side-len 2000 `
  --intra-threads 16 --output target\review-gate\bench-cpu-2000.json
target\release\rapidocr.exe evaluate --manifest <OCR-test-image>\golden-manifest.json `
  --config <OCR-Model>\test-config-small.yaml --output target\review-gate\evaluation-cpu.json
```

| 门槛 | 要求 | 本次实测（release） | 比较方式 | 结论 |
| --- | --- | --- | --- | --- |
| 12 图区域数均值 | `34.833333333333336` | **`34.833333333333336`**（36 样本） | 原始 JSON 的**数字字面量**精确比较 | True |
| 12 图 mean CER | `0.44765135645866394` | **`0.44765135645866394`**（12 例） | 同上 | True |

`git status --porcelain -- tests/baseline` 为空；`tests/baseline/windows-baseline/bench-cpu-2000.json`
的 `regions.avg` 与本次字面量相同。

### 5. 12 图 HTTP 与 CLI 逐张一致（**12/12**，真实服务 + 真实模型）

`target/m1-review-evidence/run-12-images.ps1`（`target/review-gate/http-vs-cli.log`）：

```text
models: complete=True formula.complete=True formula.routing=False
…（逐张 regions 数与 recognition.text 序列全等，明细在 http-vs-cli.log）
TOTAL serve=418 cli=418 images=12
ALL_12_MATCH=True
second /api/models: cold_this_call=0 cache_hits=8
POLLING_DOES_NOT_REHASH=True
```

### 6. 环境变量门控的 `formula_integration_tests`（真实模型，0 skipped）

```powershell
$env:RAPID_OCR_MODEL_ROOT='D:\100_Projects\110_Daily\SnapClip\OCR-Model'
$env:RAPID_OCR_FORMULA_TEST_ROOT='D:\100_Projects\110_Daily\SnapClip\Formula-TestSet'
cargo test --lib formula_integration_tests -- --test-threads=1
```

**12 passed, 0 failed, 0 ignored**（98.47 s），`skipping test` 出现 **0** 次
（→ 真的加载了 566 MB 公式识别模型与真实页面）。日志：`target/review-gate/formula-integration.log`。
11 → 12 的那一条是本轮新增的
`a_replaced_formula_detector_is_reverified_instead_of_reused`（真实检测器被替换后必须重新校验）。

### 7. im2latex-100 smoke（**本轮的库公式路径被改动，因此必须跑**）

```powershell
target\release\formula_eval.exe --model <OCR-Model>\…\pp_formulanet_plus_m.onnx `
  --dataset-root D:\100_Projects\110_Daily\SnapClip\Formula-TestSet `
  --dataset im2latex --split test --limit 100 `
  --expect-manifest target\formula-eval\manifest-im2latex-100.json `
  --output target\review-gate\formula-im2latex-100.json
```

`target/review-gate/im2latex-smoke.log`：

```text
formula_eval: dataset=im2latex split=test samples=100 scored=100 batch=8 manifest=271424c18c000f95
done: total=100 scored=100 exact=0.2400 normalized=0.2500 mean_cer=0.0863 pipeline_failures=0
      model_mismatches=76 truncated=0 load_ms=1332.1 wall_ms=68456.8
new summary: total=100 scored=100 pipeline_failures=0 exact=24 normalized=25 truncated=0
             exact_rate=0.24 normalized_rate=0.25 mean_cer=0.0863135185950055
old summary: 与上面逐字段相同
manifest_sha256 new=271424c18c000f956c31facd68d137068254e4e93d7e6b27501e9ebce67c2b6d
MANIFEST_MATCH=True  SAMPLE_SET_MATCH=True  CONTENT_MATCH=True
records new=100 old=100   PER_SAMPLE_IDENTICAL=True   TOKEN_IDS_IDENTICAL=True
GATE_EXACT_RATE_24=True  GATE_NORMALIZED_RATE_25=True
GATE_MEAN_CER_LITERAL=0.0863135185950055  GATE_ZERO_PIPELINE_FAILURES=True
```

即 **24.00% / 25.00% / 0.0863 / 0 pipeline failures** 全部复现，且与上一里程碑的
`target/formula-eval/im2latex-100.json` **逐样本**相同（路径、参考 LaTeX、输出 LaTeX、
失败分类、CER、编辑距离、EOS、truncated、两个匹配标志，以及**全部 token ids**）。

### 8. `docs/05` 的改动（只改被实现否掉的地方）

| 位置 | 改动 |
| --- | --- |
| 第 3 行状态 | "M0 待冻结，未进入实现" → "M0–M4 已实现并通过验收"，并留下这一处陈旧声明的说明（同文件末尾的"实施完成记录（M0-M4）"当时已勾选全部里程碑） |
| §1.1 哈希计算行 | 注明状态判定与公式加载路径改走 `model_verify` 的身份键控缓存 |
| §3 | 新增 `--eval-root <DIR>`（含默认关闭与拒绝语义） |
| §4.2 `/api/evaluate` 行 | 补 `--eval-root` 前置条件与三条越界拒绝 |
| §4.2.1 409 行 | 删掉"只 `stat`、不哈希"的旧规则，改成"与 `/api/models` 同一份哈希状态"，并新增 P1-1/P1-2 三条修正与残留盲区 |
| §4.4 第 4 步 | "队列容量预检" → "**队列容量预留**（判定与占位同一个动作）" |
| §5.4 响应 | 新增 `verification` 块与 `formula.detector.{sha256,state}`，并解释"成本可被验证"的意义 |
| §7.2 令牌行 | CSPRNG + fail-closed + 熵源可见；新增"评估的本机路径"一行 |
| §10 | 新增第 10 条（校验结论按文件身份缓存） |
| §11 | 新增"M1 评审修复轮（P1/P2/P3）"清单，逐条指向本文档被改正的章节 |
| §12 | 新增 5 行验证计划（完整性、校验缓存、队列预留、引擎重建线程、评估沙箱、令牌熵） |
| §13/§14/§15 | 陈旧标题"实施后填实测值"/"待确认问题（M0 冻结前）"改正；风险表新增 4 行（损坏模型静默加载、替换后旧会话、评估读任意路径、令牌熵） |

`docs/03` 未改动（`git status --porcelain -- docs/03-…` 为空）。

## 关键行为对比（AGENTS.md §9）

| 项目 | 修改前 | 修改后 | 预期结果 |
| --- | --- | --- | --- |
| 损坏的公式**检测**模型 | 加载成功（只查存在性） | `HashMismatch` 定位错误；`/api/models` 报 `corrupt`；409 在读 body 前 | 完整性规则对两个公式模型对称 |
| 损坏的公式**识别**模型（存在） | 准入通过 → 读 body → 建任务 → worker 409 | 读 body **之前** 409 `models_corrupt`（`detail.scope="formula"`） | §4.2.1 |
| 模型在首次加载后被替换 | 继续用内存里的旧会话 | 身份变化 → 重新校验 → 拒绝/重建 | 改对了就生效，改坏了就报错 |
| `/api/models`（566 MB 公式模型在盘） | 每次重读重哈希（≈315 ms/次，页面每 8 s 一次） | 命中：**0.80 ms**（max 1.34 ms），`cold_this_call=0` | 轮询不再产生持续磁盘 I/O |
| 队列满时的并发请求 | 预检与入队分属两个临界区 | 判定即预留，全部 503 且不读 body | §4.4 |
| 无 body 的 `POST /api/engine/reload` | 阻塞 accept 线程 | 独立线程；期间 `/api/status` 与 `/api/ocr` 照常 | §4.2、§7.6 |
| `/api/evaluate` 的本机读取范围 | 任意路径 | 必须 `--eval-root`，越界即可定位 400 | §4.2、§7.2 |
| 令牌熵 | 时间/PID/计数器/栈地址派生 | `BCryptGenRandom`，失败拒绝启动 | §7.2 |
| 库依赖图 | 606/605/611 行 | **逐行相同**（`tiny_http` 0/0/1） | 不得泄漏进默认构建 |
| 12 图硬门槛 / HTTP-vs-CLI / im2latex-100 | `34.833333333333336` / `0.44765135645866394` / 12-12 / 24.00%·25.00%·0.0863 | **完全相同** | 无回归 |

## 未覆盖风险与**做不到的事**（如实）

1. **未提交、未在干净 clone 上验证**：按要求不 commit；证据来自当前工作树（`1b0f860` + 本轮改动）。
2. **RNG 的真失败路径没有运行期测试**：`BCryptGenRandom` 在健康 Windows 上无法失败；
   失败分支通过注入点被真实执行（错误被传播），但"启动期拒绝"本身只是两处 `?`，
   没有独立的运行期用例（见 P2-4）。
3. **同体积 + 同 mtime 的内容替换是缓存盲区**：`(size, mtime)` 无法识别它；
   已写进模块文档、`docs/05` 与 `/api/models.verification.residual_blind_spot`。
4. **P2-1 的并发在 HTTP 层不可构造**：`accept_loop` 单线程串行处理，准入/读 body/入队不会交错；
   并发在预留原语层被真实跑出来（8 线程竞争），HTTP 层只证明"满队列的 N 个并发请求全部 503
   且不读 body"。修复的价值是让保证由数据结构成立，而不是依赖线程模型。
5. **符号链接逃逸用例依赖机器权限**：Windows 上创建符号链接需要权限/开发者模式；
   无权限时该子路径会打印一行说明并跳过，其余两条（绝对路径、`..`）走同一条规范化包含规则。
6. **`--eval-root` 的符号链接与 junction/挂载点**：只验证了文件符号链接；
   NTFS junction / 卷挂载点指向沙箱外的情况没有独立用例（`canonicalize` 会解析它们，
   但没有实测证据）。
7. **冷/缓存延迟数字是单机、单次采样**：3 次冷 + 5 次命中，页缓存是热的；
   磁盘更冷时冷验证会更慢（M4 记录过约 1 s），命中侧（`stat` + 比较）与文件大小无关，
   因此结论不依赖那个假设，但**倍数**会随冷侧变化。
8. **`cargo package` 的 serve 检查在打包树上跑**（`target/package/rapid-ocr-rs-0.7.0`），
   不是在一个全新的干净 clone 上；依赖来自本机 cargo 缓存，无网络。
9. **`docs/05` 的 M0-M4 章节仍保留历史表述**（例如 §1.1 的"实现前必须先补齐"表格、
   §5.3 里"当前 `ModelManifest` 与 `default_models.yaml` 是两套权威"这类**设计时**的描述）。
   本轮只改正了**与实现状态矛盾**的地方（状态行、§3/§4.2/§4.2.1/§4.4/§5.4/§7.2/§10/§11/§12/§13/§14/§15），
   没有重写历史盘点章节——那会让"当时的判断"不可追溯。

# A1 / A2 / B 轮：启动期冷验证、运行期"重新校验"、身份加局部摘要

**基线**：HEAD `c541486`（M1 评审修复轮交付），**未提交**（按要求）。本轮**不改 `docs/03`**
（`git status --porcelain -- docs/03-…` 为空），`Temp/demo3-v2.html` 一个字节都没动
（SHA-256 仍是 `14871FED101D11451F9B799FD199144D6CEC7874C5682D0D630DED1F5E3D46EE`，
与 M3/M4/评审轮记录相同）。

**本轮的性质**：M1 评审修复轮交付后的一轮复审提出 A1/A2/B 三项。三件事都是**保证强度**问题，
不是"能被利用的洞"（服务仍然只监听 loopback 且每次启动一个令牌）——因此本轮的产出是
"把保证写成它真正成立的样子"，并且把**确定性重查**做成两个真实入口。

| 编号 | 根因（一句话） | 修复位置 |
| --- | --- | --- |
| A1 | 验证发生在**首次使用**：服务先跑起来，坏模型要等到第一次 OCR 才发现 | `cli.rs`（新开关）/ `run.rs`（`reverify_gate`）/ `README` |
| A2 | 缓存按身份失效，但"同体积同 mtime 的替换"可能不被察觉 → **内存里的旧会话会继续服务一个磁盘上已经不是这个文件的模型**；而当时没有任何"重新读盘"的入口 | `model_plan.rs` / `server.rs` / `http.rs` / `web/index.html` |
| B | 身份 `(path, size, mtime)` 对"同体积 + 同 mtime"的内容替换是盲区 | `model_verify.rs` / `model_set.rs` / `docs/05` |

**变更规模**：`git diff --stat` = 13 个跟踪文件 **+2235 / −112**，另加新文件
`tests/serve_startup.rs`（112 行，进程边界的集成用例）。

## 环境

| 项目 | 值 |
| --- | --- |
| target | `x86_64-pc-windows-msvc`（Windows x64 + MSVC ABI） |
| 工具链 | rustc 1.98.1 / cargo 1.98.1 |
| crate | `rapid-ocr-rs` 0.7.0（独立 git 仓库，基线 `c541486`，**未提交**） |
| 真实资产（文本） | `OCR-Model/small/`、`OCR-Model/test-config-small.yaml`、`OCR-test-image/`（12 图 + `golden-manifest.json`） |
| 真实资产（公式） | `OCR-Model/Formula-Recognition-Models/onnx/pp_formulanet_plus_m.onnx`（593,915,961 B）、`OCR-Model/Formula-Detection-Model/pix2text-mfd-1.5.onnx`、`Formula-TestSet/` |
| 证据目录 | `target/reverify-gate/`（本轮全部门禁与实测）、`target/m1-review-evidence/`（沿用上一轮的脚本） |

**改动过的对外签名（新增/改变的部分）**：

```rust
// src/model_verify.rs —— 身份的第二半 + "忽略缓存"的唯一入口 + 可见的原因
pub const PARTIAL_DIGEST_WINDOW_BYTES: u64 = 64 * 1024;
pub struct PartialDigest { /* size + 首 64 KiB 的 SHA-256 + 尾 64 KiB 的 SHA-256 */ }
impl PartialDigest { pub fn compute(&Path, size: u64) -> Result<Self>; pub fn describe(&self) -> String; }
pub struct FileIdentity { /* path + size + mtime + PartialDigest */ }
pub enum ReverifyCause { FirstSight, StatChanged, ContentChanged, CacheHit }
impl ReverifyCause { pub const fn as_str(self) -> &'static str; pub const fn computed(self) -> bool; }
pub struct VerificationOutcome { /* … */ pub cause: ReverifyCause }
pub struct VerificationStats { /* … */ pub partial_mismatches: u64, pub partial_reads: u64 }
pub fn force_verify_file(&Path) -> Result<VerificationOutcome>;   // 不查缓存，现在就读盘

// src/model_set.rs —— 让"这次真的算出来的摘要"可以被取走，比较规则仍然只有一份
impl ModelFileSpec { pub fn state_in_digested(&self, root: &Path, digest: &mut String) -> (ModelFileState, bool) }

// src/bin/serve/model_plan.rs
impl ModelPlan { pub fn required_files(&self, use_cls: bool) -> Vec<(&ModelFileSpec, PathBuf)>;
                 pub fn reverify(&self, use_cls: bool) -> PlanReverification }
pub(crate) struct PlanReverification { /* files + digests_computed + content_changed */ }
pub(crate) struct VerifiedPlanFile { pub name, pub role, pub pipeline, pub declared_sha256,
                                     pub sha256, pub state, pub cause }

// src/bin/serve/run.rs
pub(crate) fn reverify_gate(&ModelPlan, use_cls: bool) -> Result<PlanReverification, ServeStartError>
pub(crate) enum ServeStartError { /* … */ ModelsUnusable { report: Box<PlanReverification> } }

// src/bin/serve/server.rs
impl ServeShared { pub fn reverify_models(&self) -> Result<Value, ServeError> }
pub(super) struct ServeContext { /* … */ pub post_verify: Option<Box<dyn Fn() + Send + Sync>> }

// src/bin/serve/http.rs
enum Route { /* … */ ModelsReverify }
enum EngineWork { Reload(Option<ProviderPreference>), ReverifyModels }   // 同一条线程路径

// CLI：--reverify-models（选项面 24 → 25，逐项枚举测试同步）
```

## A1 `--reverify-models`：启动期冷验证 + fail-fast

**根因**：进程内的校验缓存是新进程里的空缓存，因此"清缓存"不是这个开关的价值；真正的缺口是
**验证时机**——旧实现下服务先起来、第一次用到某个模型时才验证，一个 566 MB 的坏文件在
"服务已经 Ready"之后才暴露。运维需要的是"这次要用的东西在启动时就核对过，不对就别起来"。

**实现**（`run.rs::reverify_gate`，唯一实现，`run()` 与测试共用）：

1. **范围 = 这次运行真正会加载的文件**（`ModelPlan::required_files`）：文本管线的
   detector/recognizer/dictionary（`use_cls` 时再加 classifier）+ 公式管线的
   formula_recognizer。**不是**整张默认表——把当前配置永远不会加载的模型也哈希一遍既是
   纯浪费（566 MB 量级），也会让"启动失败"指向一个无关的文件。`--formula-detector` 声明的
   检测模型不属于模型集，由加载路径自己按集合声明的摘要校验（P1-1 那条规则不变）；
2. **冷验证**：每个文件都走库的 `force_verify_file`（不查缓存），逐文件打印一行
   `状态 | cause | 这一次算没算摘要`，最后一行汇总文件数、真正算出的摘要数、缺失/损坏数；
3. **fail-fast**：任一文件缺失或损坏 → `ServeStartError::ModelsUnusable`（错误里点名那些文件
   + 逐文件结论），`run()` 在**绑定端口之后、建会话之前**返回，进程以非零状态退出；
4. **自检**：拒绝启动前把 `report` 的文本阻塞清单与 `ModelPlan::snapshot()` 的清单比一次
   （同一个磁盘状态上必须一致）。不一致只打印 WARNING 并继续按 `report` 拒绝——如果这里
   `debug_assert`，那么"两次观察之间文件恰好被改动"会让服务直接 panic（一个拒绝启动的
   路径不应该有这种失败模式）。

**修改前后行为**：

| 场景 | 修改前 | 修改后 |
| --- | --- | --- |
| 计划内模型损坏 + `--reverify-models` | 与不加开关完全一样：服务起来，第一次 OCR 才失败 | 启动期逐文件核对 → **拒绝启动**，`stderr` 点名文件与状态，`stdout` 留下逐文件结论 |
| 计划内模型健康 + `--reverify-models` | — | 每个文件在启动日志里各有一行，`digests_computed == 本轮会加载的文件数` |
| 默认表里**不会**被加载的模型 | — | **不参与**这次验证（不读它的几百 MB） |
| 不加开关 | 缓存仍然在首次使用时验证（行为不变） | 同上，且启动日志明确写出"为什么现在不查、怎么让它查" |

**证据（单元层，`serve::run::tests`，3 条）**：

```text
serve::run::tests::the_startup_gate_verifies_every_file_the_run_uses_with_a_fresh_digest
serve::run::tests::the_startup_gate_refuses_to_start_and_names_the_corrupt_file
serve::run::tests::the_startup_gate_refuses_a_missing_file_too
```

第一条断言的是**绝对数字**：`digests_computed == files.len()`（`force_verify_file` 从不命中
缓存，因此这个数字不可能是"命中来的"），每个文件 `cause == first_sight`、
`state == present`、`sha256` 有值；并且清单里**不**含 `manifest.json` 这类不在本轮范围内的
文件。后两条断言错误文本里点名文件名 + 状态 + 开关名，且 `ModelsUnusable` 的清单里**只有**
那一个坏文件。

**证据（进程边界，`tests/serve_startup.rs`，1 条）**：

```text
tests/serve_startup.rs::the_serve_command_refuses_to_start_when_the_plan_model_is_missing
  （真实的 rapidocr.exe + 真实命令行 + 真实退出码；CARGO_BIN_EXE_rapidocr 由 cargo 注入，
    因此它一定指向本次 --features serve 构建出来的那个可执行文件）
```

它单独放在 `tests/` 而不是 `src/bin/serve/tests.rs` 的原因写在两个文件的注释里：单元测试
二进制在 `target/<profile>/deps/` 下，cargo 在那里**不保证**注入 `CARGO_BIN_EXE_rapidocr`，
"猜上一级目录"可能拿到 `cargo build` 留下的、没有 `serve` feature 的旧可执行文件——本轮
第一版正是这样，测试实际上在对一个与本次构建无关的二进制断言。这是一处**被实测纠正的
设计**，如实记录。

## A2 `POST /api/models/reverify`：一次动作完成"重新读盘"

**根因**：`RapidOcrEngine` 的公式会话缓存按文件身份失效，而身份对"同体积 + 同 mtime"的替换
是盲区（B 收窄了它，但没有消灭它）。因此存在这样一条路径：磁盘上的模型已经被换掉，报告里
看不出变化，**内存里的旧会话继续服务那个已经不是这个文件的东西**。当时没有任何入口能让
用户/运维强制"现在重新读一遍盘"。

**实现**（三步，缺一不可）：

1. `clear_verification_cache()` —— 否则"重新校验"会在身份未变时立刻命中，什么都不会重算；
2. `ModelPlan::reverify(use_cls)` —— 与 `--reverify-models` **同一个实现**、同一份声明
   哈希比较规则（`ModelFileSpec::state_in_digested`），忽略缓存全部重算；
3. `ensure_engine_loaded(true)` —— **引擎此前就绪就重建会话**。这一条是本端点的价值：
   只清缓存不重建引擎，就等于一个按下去什么都不会变的按钮。

响应（`docs/05` §5.5 冻结）给：逐文件 `{name, role, pipeline, state, declared_sha256, sha256,
cause, digest_computed_this_call}`、`computed`（这一轮真的重算了几个**完整摘要**）、
`content_changed`（stat 身份相同而首尾 64 KiB 不同的文件名）、以及
`outcome`/`engine`/`load_ms`/`rollback_ms`/`error`（与 `POST /api/engine/reload` **同一套**
语义）+ `verification` 成本账。

**单飞与线程**：整个序列在独立线程里跑，资格用的是 `begin_provider_switch`（与
`POST /api/engine/reload` **同一把**）——第二个并发调用立刻 **503 `busy`**，而 `busy` 的响应
由 accept 线程产生，这正是"它没有被这个序列占住"的证据。请求体**必须为空**（有 body → 400）。

**页面**（`src/bin/web/index.html`）：引擎面板下方新增常驻"重新校验"按钮 + 结论区
（`role="status"`）。放在常驻位置而不是模型横幅里，因为横幅在模型齐备时是隐藏的，而这个动作
在齐备时同样有意义。按页面既有写法接线：`addEventListener`（无内联 `onclick`）、CSS 类
（无内联 `style`）、结果为 `null` 时明确渲染缺失（不伪造结论）、离线预览模式如实说明"没有
服务端"。`__CSP_NONCE__` 仍是 4 处、`__SRV_TOKEN__` 仍是 3 处（冻结的占位符计数不变）。

**修改前后行为**：

| 场景 | 修改前 | 修改后 |
| --- | --- | --- |
| 模型文件在运行期被换掉 | 没有任何入口强制重新读盘；报告与内存会话都可能是旧的 | 一次点击 → 清缓存 → 冷验证 → 重建会话；响应逐文件说明状态与原因 |
| 损坏但存在的模型 | — | 端点报 `corrupt`，引擎进入**可定位的错误态**（不是"旧引擎继续 ready"） |
| 健康文件被还原 | — | 同一端点恢复 `ready` 并**真的重建会话**（建会话次数 +1） |
| 同时对引擎做两件事 | `engine/reload` 之间互相单飞 | `reverify` 与 `reload` **共用同一把资格**，第二个 503 `busy` |

**证据（HTTP 层，`serve::tests`，4 条）**：

```text
serve::tests::reverify_reports_a_corrupt_model_and_leaves_the_service_blocked_not_stale_ready
serve::tests::reverify_restores_ready_and_rebuilds_the_engine_after_a_healthy_revert
serve::tests::reverify_recomputes_digests_that_the_cache_would_have_answered
serve::tests::concurrent_reverifications_are_single_flight_and_never_run_on_the_accept_thread
serve::tests::the_page_carries_the_reverify_button_without_inline_handlers_or_styles
```

- 第 1 条：`rec.onnx` 被换成**同体积**内容并把 mtime 写回原值 → 启动期就报
  `blocked_models_missing`（不建会话），端点报 `corrupt: ["rec.onnx"]`、`computed == 4`，
  之后的 `POST /api/ocr` 是 409 `models_corrupt` 且点名文件；
- 第 2 条：写坏 → 端点 `failed`；还原 → 端点 `ready` 且建会话次数 1 → 2（**真的重建**），
  随后普通 OCR 成功（不是"只把状态字段改回 ready"）；
- 第 3 条：预热缓存（`cold_this_call == 0`）之后调端点，`computed == 4` 且每个文件的
  `cause != cache_hit`；有 body → 400；
- 第 4 条：用 `ServeContext::post_verify` 这个**测试注入点**把第一次调用精确停在
  "校验已算完、引擎未重建"的窗口里（生产路径该字段恒为 `None`），此时第二个连接立刻拿到
  503 `busy`（< 5 s），随后第一次调用照常返回 200。**这条例外需要说明**：为了确定性地
  观察单飞，`ServeContext` 多了一个仅测试可设的钩子；它不是生产路径的一部分。
- 第 5 条：页面里 `id="verifyBtn"`、`/api/models/reverify`、`addEventListener`、
  `role="status"` 都在，且**不存在**任何 `onclick=` / ` style="`；请求体为空字面量
  `xhrSend('POST', '/api/models/reverify', '')`。

## B 身份加"首尾各 64 KiB 的局部摘要"

**根因**：`(path, size, mtime)` 对"同体积 + 同 mtime"的内容替换是盲区。这不是理论问题：
`SetFileTime`（或同一时间戳粒度内的原地改写）就能构造它，而**同一个盲区**同时影响
`/api/models` 的报告、公式准入与两条加载路径（它们共用这一份证据）。

**实现**：

- `PartialDigest::compute(path, size)` 一次 `open` + 两次定位读：首 `min(size, 64 KiB)` 字节、
  尾 `min(size, 64 KiB)` 字节，各算一次 SHA-256，与体积一起构成摘要（体积参与编码）；
- 命中判据从"stat 身份相同"变成"**体积相同 且 局部摘要相同**"。`mtime` 退回它本来的角色
  （元数据），只在**报告/日志**里出现——把它当判据会让"原样复制文件"报出错误的
  `content_changed`（见下一条）；
- stat 相同而局部摘要不同 → **作废并重新完整哈希**，并如实留下 `cause = ContentChanged`；
- 新增可见计数：`verification.partial_mismatches`（"内容变了"被抓住的次数）与
  `verification.partial_reads`（局部读次数；每次校验恰好一次）；
- `force_verify_file`（不查缓存）成为"确定性重查"的唯一库入口，A1/A2 都用它。

**修改前后行为**：

| 场景 | 修改前 | 修改后 |
| --- | --- | --- |
| 同体积 + 同 mtime，改动落在**头部** 64 KiB | 命中缓存 → 返回旧摘要（**漏报**） | `ContentChanged` → 完整重哈希 → 报 `corrupt` |
| 同体积 + 同 mtime，改动落在**尾部** 64 KiB | 同上 | 同上 |
| 同体积 + 同 mtime，改动只落在**中段** | 漏报 | **仍然漏报**（这是被测试钉住的限制，不是意外） |
| 文件 ≤ 128 KiB | 漏报 | **不漏报**：首尾切片重叠，局部摘要覆盖整个内容 |
| `mtime` 变了但内容一个字节没变 | 身份不等 → 完整重哈希（**多花一次 300 ms**） | 命中（内容没变），并如实报 `cache_hit` |
| 每次校验的成本 | 一次 `stat` | 一次 `stat` + 128 KiB 顺序读 |

**证据（库层，`model_verify::tests`，14 条；本轮新增/改写的 8 条）**：

```text
a_same_size_same_mtime_edit_in_the_head_is_detected_and_rehashed    （B-1a）
a_same_size_same_mtime_edit_in_the_tail_is_detected_and_rehashed    （B-1b）
a_same_size_same_mtime_edit_confined_to_the_middle_is_not_detected  （B-2，**限制本身**被钉住）
a_file_smaller_than_the_window_is_fully_covered_by_the_partial_digest（B-3）
empty_and_exactly_window_sized_files_are_well_defined               （边界：0 字节与正好 128 KiB）
the_partial_digest_binds_the_first_and_last_window_only             （窗口只绑首尾；中段不动它）
a_cache_hit_reads_only_the_windows_and_never_rehashes               （B-4：命中不完整哈希）
an_mtime_only_change_still_hits_and_is_not_reported_as_a_content_change（原因必须是真实原因）
```

`a_same_size_same_mtime_edit_confined_to_the_middle_is_not_detected` 刻意断言**限制**：它同时
断言 `force_verify_file` 会给出真实摘要并纠正缓存（"确定性逃生口真的有效"）。这样"把窗口
当成保证"的改动会让测试失败，而不是只让文档变得不准确。

**证据（HTTP 层）**：`serve::tests::models_detects_a_same_size_same_mtime_swap_through_the_content_windows`
—— 同体积 + mtime 写回原值后，`/api/models` 报 `corrupt: ["rec.onnx"]`，
`verification.partial_mismatches` 增长，且**这次调用**至少重算了 1 个摘要
（`cold_this_call >= 1`）。

**一处被实测纠正的断言**（如实记录）：这条用例最初断言 `cold_this_call == 1`（"只有它被重新
哈希"）与 `partial_mismatches` 差值，在 `cargo test --all-targets` 下**偶发失败**——库的缓存
与累计账都是**进程级**的，而"预热那一次 `/api/models`"是否真的命中取决于同一进程里另一个
测试二进制（`--lib` 与 `serve` bin）的并行校验。修正方式不是放宽结论，而是把断言改成
不依赖累计计数器的形式：枚举顺序（`FileIdentity::of`）先证明"体积与 mtime 完全相同、只有
窗口不同"，再断言 `cold_this_call >= 1`（**旧身份下这里必然是 0**，也就是漏报）与
`partial_mismatches` 增长。反过来，`model_verify` 的 14 条用例保留 `serial()` 串行锁，
让"这一次算没算"这类**单次调用**的属性仍然可以被逐条断言。

## 性能：`/api/models` 轮询成本（加局部摘要前后）与两条"强制重查"路径

`target/reverify-gate/run-reverify-latency.ps1`（真实 release 服务 + 真实 566 MB 公式模型；
输出 `target/reverify-gate/latency.log`）：

```text
startup: cold_this_call=0 cold_verifications=4 partial_reads=8 wall_ms=19.3

round,wall_ms,cold_this_call,cold_verifications,cache_hits,partial_reads,partial_mismatches,last_cold_ms,last_cold_bytes
cold round 1 (head window byte changed, stat identity restored),316.27,1,5,7,12,1,314.264,593915961
cold round 2 (head window byte changed, stat identity restored),319.28,1,6,10,16,2,317.617,593915961
cold round 3 (head window byte changed, stat identity restored),314.43,1,7,13,20,3,312.464,593915961
restore re-hash (content windows changed back),338.04,1,8,16,24,4,336.372,593915961
cache hit (identity unchanged),1.53,0,8,20,28,4,336.372,593915961
cache hit (identity unchanged),1.29,0,8,24,32,4,336.372,593915961
cache hit (identity unchanged),1.5,0,8,28,36,4,336.372,593915961
cache hit (identity unchanged),1.45,0,8,32,40,4,336.372,593915961
cache hit (identity unchanged),1.15,0,8,36,44,4,336.372,593915961

COLD   /api/models (566 MB re-hashed because the head window changed): mean=316.66 ms over 3 rounds; per-call cold_this_call=1
CACHED /api/models (identity unchanged, 128 KiB windows read): mean=1.38 ms, max=1.53 ms over 5 rounds; per-call cold_this_call=0
RESTORE (the 566 MB copy came back): 338.04 ms, cold_this_call=1 partial_mismatches=4
SPEEDUP: 229.5x
BASELINE (before B, same machine/protocol): cached mean=0.80 ms, max=1.34 ms; cold mean=315.12 ms
POLL DELTA: 0.58 ms per /api/models (this is the price of the 128 KiB windows)
```

- **轮询成本（前 → 后）**：稳态均值 **0.80 ms → 1.38 ms**（max 1.34 ms → 1.53 ms），即
  **每个 `/api/models` 多约 0.58 ms**；这 0.58 ms 就是 4 个文件 × 128 KiB 的顺序读。
  这个变化在页面 8 s 轮询的尺度上不可观察，而且它买到的是"同体积 + 同 mtime 的替换不再
  静默通过"。
- **冷验证成本**：**316.66 ms**（`last_cold_bytes = 593,915,961`），与加局部摘要前的
  315.12 ms 在噪声内相同——因为 128 KiB 相对于 566 MB 是 0.02%。
- **"只改 mtime"不再是重哈希的理由**（这一条与 B 的设计直接相关）：第一版测量脚本用
  `LastWriteTime = Get-Date` 制造"冷"，结果 `cold_this_call=0`——因为新判据看的是**内容窗口**
  而不是 mtime，内容没变就该命中。要真正强制一次完整重哈希，必须让**首 64 KiB 变化**
  （脚本改成改写第 0 个字节并把 mtime 写回原值），这正是 `cold round` 那一组做的事，
  `partial_mismatches` 逐轮 +1 就是证据。
  **这处"测量脚本第一版测错了东西"如实记录**：旧口径下的"冷"在新实现里根本不是冷。
- **启动期 `--reverify-models` 成本**（`target/reverify-gate/serve-reverify-startup.log`）：

```text
serve: --reverify-models PP-OCRv6_det_small.onnx (detector) present sha256=090f04abcd9d9a74 | first_sight | digest computed by this call: true
serve: --reverify-models PP-OCRv6_rec_small.onnx (recognizer) present sha256=6f327246b50388f3 | first_sight | digest computed by this call: true
serve: --reverify-models ppocrv6_dict.txt (dictionary) present sha256=b5f2bfe2bdd94484 | first_sight | digest computed by this call: true
serve: --reverify-models pp_formulanet_plus_m.onnx (formula_recognizer) present sha256=71b6d389cf7b857e | first_sight | digest computed by this call: true
serve: --reverify-models verified 4 file(s) in 343 ms (4 full digest(s) computed by this call, 0 missing/corrupt)
```

  **启动期验证的墙钟成本 = 343 ms**（4 个文件，其中 566 MB 那个占绝大部分），每个文件一行，
  `digest computed by this call: true` 逐行可见。

- **运行期 `POST /api/models/reverify` 成本**（同一脚本，引擎已是 ready）：

```text
POST /api/models/reverify: HTTP 200 wall_ms=453.39 outcome=ready computed=4 content_changed= load_ms=448
  reverify file PP-OCRv6_det_small.onnx role=detector pipeline=text state=present cause=first_sight computed=True
  reverify file PP-OCRv6_rec_small.onnx role=recognizer pipeline=text state=present cause=first_sight computed=True
  reverify file ppocrv6_dict.txt role=dictionary pipeline=text state=present cause=first_sight computed=True
  reverify file pp_formulanet_plus_m.onnx role=formula_recognizer pipeline=formula state=present cause=first_sight computed=True
```

  **453 ms**，其中 `load_ms = 448 ms` 是重建会话（真实 ONNX 会话创建），余下是 4 个文件的
  冷验证。也就是说这个端点的成本**由"重建引擎"主导**，而不是由多读一次盘主导——这正是
  "必须有第 3 步"的量化理由。

## 验证

### 1. 静态检查、feature 矩阵与 release 构建（全部 exit 0）

`target/reverify-gate/run-gates.ps1` → `target/reverify-gate/gates.log`：

| # | 命令 | 结果 |
| --- | --- | --- |
| 1 | `cargo fmt --all -- --check` | 0 |
| 2 | `cargo clippy --all-targets -- -D warnings` | 0 |
| 3 | `cargo clippy --features serve --all-targets -- -D warnings` | 0 |
| 4 | `cargo test --all-targets` | **401 passed**（lib）+ 2 + 4 + 14 + 0，**0 failed** |
| 5 | `cargo test --features serve --all-targets` | **401 passed**（lib）+ 2 + 4 + 14 + **242 passed**（bin）+ **1 passed**（`serve_startup` 集成），**0 failed** |
| 6 | `cargo build --release --bins` | 0 |
| 7 | `cargo build --release --features serve --bins` | 0 |

本轮新增 **17 个库单测**（384 → 401：`model_verify` 11 → 14、`cli` +1、`run` +3、
`serve::tests` +5、集成 +1，另有若干条既有用例的断言被**加强**而不是弱化）。改动过的既有
断言只有两类**预期行为发生正确变化**的原因：

1. `/api/models.verification.identity` 的字面量（`path + size + mtime` →
   `… + SHA-256 of the first and last 64 KiB`），并**新增** `partial_window_bytes`、
   `partial_reads`、`residual_blind_spot` 含 "not a security boundary" 的断言；
2. `the_serve_command_cold_verifies_at_startup_and_exits_non_zero_on_a_corrupt_model` 从
   `src/bin/serve/tests.rs`（单元测试目标，拿不到 `CARGO_BIN_EXE_rapidocr`）**搬到**
   `tests/serve_startup.rs`（集成测试，cargo 保证注入）——不是因为断言太严，而是因为它在
   原来的位置上断言的是**另一个二进制**。

**没有跳过、没有弱化、没有删除任何既有断言。** 另外 `cargo fmt --all` 改动了若干处本轮之前
就不符合 rustfmt 的格式（`docs/05` 未涉及）。

### 2. 依赖隔离（三份依赖树与 M4 / 评审快照**逐行 0 差异**）

`Cargo.toml` 的 `[dependencies]` / `[features]` 未改；新增的只是一个**测试目标**声明
（`[[test]] serve_startup` + `required-features = ["serve"]`，见 `src/bin/serve/mod.rs` 的边界
测试与下表）。

| 命令 | 结果 |
| --- | --- |
| `cargo tree -e normal -p rapid-ocr-rs` | **606 行**，与 `target/review-gate/tree-default-normalized.txt` **逐行相同**；`tiny_http` **0** 次 |
| `cargo tree -e normal --no-default-features` | **605 行**，与快照逐行相同；`tiny_http` **0** 次 |
| `cargo tree -e normal -p rapid-ocr-rs --features serve` | **611 行**，与快照逐行相同；`tiny_http` **1** 次 |

（`serve::dependency_boundary::{the_http_dependency_is_optional_and_outside_the_default_feature,
the_http_dependency_lives_only_in_the_http_module}` 两条边界断言同时通过：
`tiny_http` 只出现在 `src/bin/serve/http.rs`。）

### 3. `cargo package --allow-dirty` 与打包树的 `--features serve` 检查

```text
Packaged 191 files, 14.2MiB (3.3MiB compressed)
Verifying rapid-ocr-rs v0.7.0 (…\target\package\rapid-ocr-rs-0.7.0)
Finished `dev` profile [unoptimized + debuginfo] target(s) in 15.90s     （exit 0）
```

`cargo package --list` 里 `src/model_verify.rs`、`src/bin/web/index.html`、
`src/bin/serve/model_plan.rs`、`tests/serve_startup.rs` 都在；打包树上的
`cargo check --features serve --all-targets`（`target/package/rapid-ocr-rs-0.7.0`）**exit 0**
（24.23 s），日志 `target/reverify-gate/package-serve-check.log`。

### 4. 12 图硬门槛（`target/reverify-gate/`，**没有**覆盖 `tests/baseline/`）

`target/reverify-gate/run-hard-gates.ps1` → `hard-gates.log`：

| 门槛 | 要求 | 本次实测（release） | 比较方式 | 结论 |
| --- | --- | --- | --- | --- |
| 12 图区域数均值 | `34.833333333333336` | **`34.833333333333336`**（36 样本） | 原始 JSON 的**数字字面量**精确比较 | True |
| 12 图 mean CER | `0.44765135645866394` | **`0.44765135645866394`**（12 例） | 同上 | True |

`tests/baseline` 未被改动（脚本内 `git status --porcelain -- tests/baseline` 为空，
`bench-cpu-2000.json` 的 `regions.avg` 与本次字面量相同）。

### 5. 12 图 HTTP 与 CLI 逐张一致（**12/12**，真实服务 + 真实模型）

`target/reverify-gate/run-12-images.ps1` → `http-vs-cli.log`：

```text
models: complete=True formula.complete=True formula.routing=False
verification: identity='path + size + mtime + SHA-256 of the first and last 64 KiB' window=65536
verification: cold_this_call=0 cold_verifications=4 cache_hits=4 partial_reads=8 partial_mismatches=0
…（逐张 regions 数与 recognition.text 序列全等，明细在 http-vs-cli.log）
TOTAL serve=418 cli=418 images=12
ALL_12_MATCH=True
second /api/models: cold_this_call=0 cache_hits=8 partial_reads=12
POLLING_DOES_NOT_REHASH=True
POST /api/models/reverify: HTTP 200 wall_ms=546 outcome=ready computed=4 content_changed= load_ms=541
```

（真实资产上的 `reverify` 比测量脚本里的 453 ms 稍慢：这一次服务刚跑完 12 张图，
引擎重建 541 ms。**数值如实记录，不取最好的一次。**）

### 6. 环境变量门控的 `formula_integration_tests`（真实模型，0 skipped）

```powershell
$env:RAPID_OCR_MODEL_ROOT='D:\100_Projects\110_Daily\SnapClip\OCR-Model'
$env:RAPID_OCR_FORMULA_TEST_ROOT='D:\100_Projects\110_Daily\SnapClip\Formula-TestSet'
cargo test --lib formula_integration_tests -- --test-threads=1
```

**12 passed, 0 failed, 0 ignored**（89.59 s），`skipping test` 出现 **0** 次
（→ 真的加载了 566 MB 公式识别模型与真实页面）。日志：
`target/reverify-gate/formula-integration.log`。其中
`a_replaced_formula_detector_is_reverified_instead_of_reused` 正是"会话缓存按文件身份失效"
那一条，本轮改了身份的定义，因此它是必须重跑的回归。

### 7. im2latex-100 smoke（**本轮改动了公式加载路径共用的校验入口，因此必须跑**）

```powershell
target\release\formula_eval.exe --model <OCR-Model>\…\pp_formulanet_plus_m.onnx `
  --dataset-root D:\100_Projects\110_Daily\SnapClip\Formula-TestSet `
  --dataset im2latex --split test --limit 100 `
  --expect-manifest target\formula-eval\manifest-im2latex-100.json `
  --output target\reverify-gate\formula-im2latex-100.json
```

`target/reverify-gate/im2latex-smoke.log`：

```text
done: total=100 scored=100 exact=0.2400 normalized=0.2500 mean_cer=0.0863 pipeline_failures=0
      model_mismatches=76 truncated=0 load_ms=1306.8 wall_ms=69121.2
new summary: total=100 scored=100 pipeline_failures=0 exact=24 normalized=25 truncated=0
             exact_rate=0.24 normalized_rate=0.25 mean_cer=0.0863135185950055
old summary: 与上面逐字段相同
MANIFEST_MATCH=True  SAMPLE_SET_MATCH=True  CONTENT_MATCH=True
records new=100 old=100   PER_SAMPLE_IDENTICAL=True   TOKEN_IDS_IDENTICAL=True
GATE_EXACT_RATE_24=True  GATE_NORMALIZED_RATE_25=True
GATE_MEAN_CER_LITERAL=0.0863135185950055  GATE_ZERO_PIPELINE_FAILURES=True
```

即 **24.00% / 25.00% / 0.0863 / 0 pipeline failures** 全部复现，且与上一里程碑的
`target/formula-eval/im2latex-100.json` **逐样本**相同（含全部 token ids）。

### 8. `docs/05` 与 `README` 的改动（A3：把保证写成它真正成立的样子）

| 位置 | 改动 |
| --- | --- |
| `docs/05` §3 | 新增 `--reverify-models`（含范围、代价、与"清缓存"的区别） |
| §4.2 端点表 | 新增 `POST /api/models/reverify`（三步、空 body、单飞、独立线程） |
| §4.2.1 残留盲区 | 从"`(size, mtime)` 近似"改成"**同体积 + 同 mtime + 首尾 64 KiB 逐字节相同**"，并写明局部摘要是**启发式而不是安全边界**、两个确定性入口、以及小于 128 KiB 的明确规则 |
| §5.4 响应 | `verification` 增加 `partial_window_bytes`/`partial_reads`/`partial_mismatches`/`guarantee`/`force_check`，`residual_blind_spot` 换成收窄后的文本 |
| **§5.5（新）** | `POST /api/models/reverify` 的响应形状与逐字段语义（含"为什么必须有第 3 步"） |
| §9.2 布局 | 引擎面板下方的"重新校验"按钮与结论区（含"为什么放在常驻位置"） |
| §10 第 10 条 | 身份定义、命中成本、以及"需要确定性重查时的两个入口" |
| §11 | 新增"A1/A2/B"清单，逐条指向被改正的章节 |
| §12 | 新增 3 行验证计划（启动期冷验证 / 运行期重新校验 / 局部摘要） |
| `README` "Model integrity" | 新增一节"What the digest cache does — and does not — guarantee"：把保证原句、盲区、启发式定位与两个确定性入口写清楚 |

`docs/03` 未改动。

## 关键行为对比（AGENTS.md §9）

| 项目 | 修改前 | 修改后 | 预期结果 |
| --- | --- | --- | --- |
| 启动期验证时机 | 首次使用（服务已 Ready 之后） | `--reverify-models` 时在**建会话之前**，缺失/损坏 → 非零退出并点名文件 | A1 |
| 运行期"重新读盘" | **没有入口** | `POST /api/models/reverify`：清缓存 → 冷验证 → 重建会话 | A2 |
| 模型被换成"同体积 + 同 mtime"的另一份内容 | 缓存命中 → 报告与内存会话都停在旧结论 | 首尾 64 KiB 变化 → 重新完整哈希 → 报 `corrupt`（`content_changed`/`partial_mismatches` 可见） | B |
| 同一替换只改**中段** | 漏报 | **仍然漏报**（被测试钉住的限制，文档同时写明） | 如实 |
| `size ≤ 128 KiB` 的替换 | 漏报 | 不漏报（首尾重叠 ⇒ 覆盖整个内容） | B |
| 只改 mtime（内容不变） | 完整重哈希（约 300 ms） | 命中（`cache_hit`），并如实报"不是内容变化" | 原因必须真实 |
| `/api/models` 稳态轮询 | 0.80 ms | **1.38 ms**（+0.58 ms = 4 × 128 KiB） | 轮询成本仍可忽略 |
| 引擎序列的单飞 | `engine/reload` 之间 | `reverify` 与 `reload` **共用同一把资格**（第二个 503 `busy`） | §4.2、§7.6 |
| 库依赖图 | 606 / 605 / 611 行 | **逐行相同**（`tiny_http` 0/0/1） | 不得泄漏进默认构建 |
| 12 图硬门槛 / HTTP-vs-CLI / im2latex-100 | `34.833333333333336` / `0.44765135645866394` / 12-12 / 24.00%·25.00%·0.0863 | **完全相同** | 无回归 |

## 未覆盖风险与**做不到的事**（如实）

1. **未提交、未在干净 clone 上验证**：按要求不 commit；证据来自当前工作树（`c541486` + 本轮改动）。
2. **局部摘要不是安全边界**（本轮的核心限制，重复一次）：能写文件的人可以保留首尾、只改中段；
   `mtime` 不可得时身份里没有它，同类替换同样落在盲区里。它收窄的是"误判"概率。
3. **A2 的单飞测试用了一个仅测试可设的钩子**：`ServeContext.post_verify` 生产路径恒为 `None`。
   没有它，唯一的替代是 sleep 猜时序（那会让"第一个序列仍在进行"变成概率事件）。这是
   "为了可断言性引入一个测试注入点"的取舍，注入点本身在代码里被点名说明。
4. **`Ready → (文件被破坏) → ?` 的状态机边**：`docs/05` §7.6 的转换表里没有
   `Ready → BlockedModelsMissing`，因此"引擎已就绪之后计划内文件被破坏"的诚实结论文案是
   `Failed` + `reason` 点名文件（`Loading → Failed`），不是 `blocked_models_missing`。
   本轮的用例因此断言"**不是** stale ready + 点名文件 + 后续 409"，而不是一个文档里不存在的状态。
   这是上一轮就存在的设计事实，本轮**首次把它写清楚**（测试注释与本节）。
5. **`/api/models` 的冷/热数字是单机、单次采样**：3 次冷 + 5 次命中，页缓存是热的；
   磁盘更冷时冷验证会更慢（M4 记录过约 1 s），命中侧（`stat` + 128 KiB）与文件大小无关，
   因此结论不依赖那个假设，但**倍数**会随冷侧变化。基线（0.80 ms）来自上一轮同一台机器、
   同一份脚本协议。
6. **A1 的"健康模型"用例在单元层不断言"服务真的接受请求"**：夹具文件不是有效的 ONNX，
   真实建会话会在更后面失败。进程边界的那条用例证的是**拒绝启动**（损坏/缺失），
   "健康 → 真的开始服务"由本轮的 12 图 HTTP 用例（真实模型 + 真实服务）覆盖。
7. **`cargo package` 的 serve 检查在打包树上跑**（`target/package/rapid-ocr-rs-0.7.0`），
   不是在一个全新的干净 clone 上；依赖来自本机 cargo 缓存，无网络。
8. **页面按钮只做了结构与接线断言**：`serve::tests` 断言按钮、端点字面量、
   `addEventListener`、`role="status"`、无内联 handler/style；**没有**浏览器自动化
   （本轮没有运行 Playwright 一类的驱动）。JS 语法用 `node --check` 对抽取出的
   `<script>` 块验证通过（`target/tmp/page-script.js`）。

---

# 计划轮：一份运行计划（公式检测模型进计划 + 逐管线报告）

**基线**：HEAD `a04d6bc`（A1/A2/B 轮交付），**未提交**（按要求）。本轮**不改 `docs/03`**
（`git status --porcelain -- docs/03-…` 为空），`Temp/demo3-v2.html` 一个字节都没动
（SHA-256 仍是 `14871FED101D11451F9B799FD199144D6CEC7874C5682D0D630DED1F5E3D46EE`，
与 M3/M4/评审/A1-A2-B 轮记录相同）。证据目录：`target/plan-gate/`。

**变更规模**：`git diff --stat` = 9 个跟踪文件 **+2973 / −692**，其中 8 个是本轮的实现/测试/
文档（**+2566 / −692**），第 9 个是本节（`docs/06`，+407）。**库源码（`src/*.rs` 顶层）一行
未改**：`src/model_verify.rs`、`src/model_set.rs`、`src/ocr/`、`src/formula/` 都没动，因此
im2latex smoke 的必跑条件（"库公式代码改动"）不成立——本轮仍然把它跑了，作为额外证据
（见验证 7）。

## 根因

"**这次运行会加载哪些文件**"这一个问题，旧实现里有**三份说法**：

| 说法 | 位置 | 它包含什么 | 谁读它 |
| --- | --- | --- | --- |
| `ModelPlan::required_files(use_cls)` | `model_plan.rs`（本轮之前的 ~327 行） | 文本三个（+ `use_cls` 时的 classifier）+ **无条件**的 `formula_recognizer` | A1（`--reverify-models`）、A2（`POST /api/models/reverify`） |
| `snapshot()` / `blocking_files(statuses, …)` | `model_plan.rs` | 库存里**各 role 的全部文件**，按管线过滤 | 引擎状态机、`/api/models`、`/api/ocr` 的 409 |
| `formula_detector_status()` | `server.rs` | 临时用 `ModelFileSpec::new(...)` 重建的**检测模型**单独一份 | 公式准入、`formula.detail.detector` |

由此同时存在两个**方向相反**的错误：

1. **检测模型漏在计划外**：`required_files` 的 role 列表里没有 `formula_detector`
   （旁边那行注释甚至写着"它不属于模型集，因此不在本清单里重复列出"）。于是
   `--reverify-models` **从不冷验证**一个 `--formula-detector`（或集合声明的检测模型），
   `POST /api/models/reverify` 也**从不列它、从不为它算摘要**——一个"同体积 + 同 mtime、
   改动只落在中段"的检测模型可以一直留在陈旧摘要上，直到某次普通公式请求路径碰巧重新
   校验。检测模型是**真的会被加载**的（`FormulaPolicy.detector_path` →
   `rapid_ocr.rs::formula_detector`），所以这是"报告的一份、加载的另一份"的经典缺口。
2. **公式识别模型被无条件加入计划，而 fail-fast 只看 `text_blocking()`**：于是
   (a) 公式路由**未启用**时，A1/A2 仍然把 566 MB 的 `formula_recognizer` 读一遍（纯浪费）；
   (b) 公式路由**已启用**时，一个损坏的 `formula_recognizer` **不会**让 `--reverify-models`
   失败（与开关"验证这次运行会用的整份计划"的契约矛盾），运行期的"重新校验"也因此可能
   回答"引擎就绪"而公式模型其实是坏的。

**根因归类**：不是局部实现错误，而是**数据结构/模块边界错误**——"运行计划"没有作为一个
显式结构存在，而是散落成三处各自维护的清单。因此本轮的修法是把它变成 `ModelPlan` 的
一个字段（`planned: Vec<PlanFile>`），并让所有消费者读它，而不是在旧的三份清单上打补丁。

## 仲裁后的设计（不重新讨论）

**一份运行计划，所有消费者共用**：

- 文本管线：`detector` + `recognizer` + `dictionary`（`global.use_cls` 为真时再加 `classifier`）；
- 公式管线：**仅当公式路由启用时**（解析出了检测模型：`--formula-detector` 优先，其次是
  模型集声明的 `formula_detector` role）加入 `formula_recognizer` **与** `formula_detector`；
  CLI 指到别处时集合里那份不会被加载，因此**不进计划**；
- 公式未启用 ⇒ 公式模型**不在计划里**，A1/A2 一个字节都不读。

**两种语义（互相兼容，都写进文档与错误文案）**：

- `--reverify-models` = 显式 opt-in："启动时验证**这一次运行的整份计划**，计划内任何文件不可用
  都拒绝启动"。公式启用时损坏的公式模型（检测器**或**识别器）让启动失败，错误**按管线分组**
  并点名文件与所属管线；
- 运行期准入**按管线**不变：损坏的公式模型只让公式队列在读 body 之前 409
  （`detail.scope="formula"`），普通 OCR 照常；
- `POST /api/models/reverify` **按管线**报告：`text.outcome` 与
  `formula.{routing,in_plan,complete,missing,corrupt,blocked,detector}` 分开给出，
  "文本就绪、公式损坏"是显式的。

**没有声明摘要时"验证"的含义（新文档化规则）**：集合之外的 `--formula-detector` 没有可信
摘要，此时 "verified" 只意味着**存在 + 可读 + 像 ONNX**（protobuf 序言：字段 1 =
`ir_version`，tag `0x08` + varint 1..=64；空文件被拒绝）。响应里它的 `sha256` 是 `null`，
但 A1/A2 仍然**真的算一个摘要**并如实报告算出来的值；集合声明了摘要时哈希始终是权威。

## 修改过的对外签名

```rust
// src/bin/serve/model_plan.rs
pub(super) struct PlanFile { pub name, pub role, pub path, pub declared_sha256, pub pipeline }
impl PlanFile { pub fn has_declared_digest(&self) -> bool;
                pub fn state(&self) -> ModelFileState;   // 唯一状态判定：有摘要→哈希；没有→文档化规则
                pub fn status(&self) -> PlanFileStatus }
pub(super) struct PlanFileStatus { pub name, pub role, pub pipeline, pub state }

impl ModelPlan {
    // 第三个参数是 CLI 的 --formula-detector：计划在这里一次算完
    pub fn resolve(dir: &Path, engine: &EngineConfig, formula_detector: Option<&Path>) -> Result<Self, ModelPlanError>;
    pub fn plan_files(&self) -> &[PlanFile];
    pub fn plan_files_in(&self, pipeline: Pipeline) -> Vec<&PlanFile>;
    pub fn plan_file(&self, role: ModelRole) -> Option<&PlanFile>;
    pub fn plan_blocking(&self, pipeline: Pipeline) -> Vec<BlockingFile>;
    pub fn plan_complete(&self, pipeline: Pipeline) -> bool;
    pub fn formula_in_plan(&self) -> bool;
    pub fn formula_detector(&self) -> Option<&FormulaDetectorSpec>;
    pub fn reverify(&self) -> PlanReverification;   // 不再收 use_cls：计划在解析期固定
}
// 删除（公开形状）：required_files()、formula_recognizer()、resolve_formula_detector()
// 新增内部实现：build_plan()、detector_is_declared()、undeclared_digest_failure()、file_name()

// src/bin/serve/model_plan.rs —— 报告与快照口径
impl PlanReverification { pub fn blocking_in(&self, Pipeline) -> Vec<&VerifiedPlanFile>;
                          pub fn files_in(&self, Pipeline) -> Vec<&VerifiedPlanFile>;
                          pub fn blocking_summary(&self) -> String }   // 错误文案按管线分组
impl ModelReport { /* 库存口径：formula_inventory_blocking() / formula_inventory_complete() */ }
impl ModelSnapshot { /* 按运行计划分组 + formula_in_plan() */ }

// src/bin/serve/run.rs
impl OcrRouting { pub fn from_plan(plan: &ModelPlan) -> Self }   // 路由的唯一来源
pub(crate) fn reverify_gate(model_plan: &ModelPlan) -> Result<PlanReverification, ServeStartError>;
ServeStartError::ModelsUnusable { report }   // Display 改为按管线分组

// src/bin/serve/server.rs
pub(super) struct ServeContext { /* 去掉 routing / formula_detector：两者都由计划决定 */ }
impl ServeShared {
    pub fn routing(&self) -> OcrRouting;                          // OcrRouting::from_plan(&self.model_plan)
    pub fn formula_policy(&self) -> Option<FormulaPolicy>;        // 计划里的识别 + 检测
    pub fn formula_models_ready(&self) -> bool;                   // plan_blocking(Formula).is_empty()
    pub fn formula_detector_status(&self) -> Option<PlanFileStatus>;
    fn pipelines_json(&self, Option<&PlanReverification>, text_outcome: &str) -> Value;  // /api/models 与 A2 共用
}
```

## 关键行为对比（AGENTS.md §9）

| 项目 | 修改前 | 修改后 | 预期结果 |
| --- | --- | --- | --- |
| 公式检测模型（`--formula-detector` / 集合声明） | **不在计划里**：A1 不冷验证、A2 不列不算摘要；状态由 `server.rs` 单独重建一份 | 在计划里：A1 冷验证、A2 列出并**真的重算摘要**、准入与报告读同一份状态 | 根因修复 |
| 集合**之外**的检测模型（无声明摘要） | 由 `server.rs` 的临时 spec 判定，`state_in` 无哈希 → 无条件 `present` | 文档化规则：存在 + 可读 + 像 ONNX；`sha256: null`；A2 仍报告算出来的摘要 | 如实 |
| 公式路由**未启用** + `formula_recognizer` 损坏 | `--reverify-models` 通过（只看 `text_blocking()`），但计划里**还是**把它哈希了 | 不在计划里：不哈希（`digests_computed == 3`）、不拦启动 | 两个方向都修 |
| 公式路由**启用** + `formula_recognizer` 损坏 | `--reverify-models` **通过**（与契约矛盾） | 启动失败，错误点名 `formula pipeline: fx.onnx (…) corrupt` | 契约 |
| 公式路由启用 + 检测模型损坏 | 启动**通过**；A2 的 `files[]` 里没有它 | 启动失败并点名它；A2 `files[]` 有它且 `digest_computed_this_call=true` | 根因修复 |
| 运行期公式模型损坏 | 公式队列 409（`scope=formula`），普通 OCR 正常 | **不变** | 不回归 |
| A2 的响应 | `outcome` + 逐文件（混两条管线） | 加 `pipelines.text`/`pipelines.formula`（`text.outcome` 与公式结论分开）；顶层 `missing`/`corrupt` 改成这次冷验证的**文本**结论 | 显式 |
| `/api/models` | 顶层=文本（库存并集）、`formula` 块=公式 | 顶层=**计划**的文本；`formula` 块=**库存**（页面下载口径不变）；**新增** `pipelines`（计划口径，含 `in_plan`） | 加而不改 |
| 页面结论区 | 只显示引擎 `outcome` + 逐文件状态 | 多一行"公式管线：…（检测模型 …）"，并按 `pipelines` 区分"普通 OCR 409"与"仅公式队列 409" | 最小改动 |
| `--reverify-models` 启动成本（公式未启用，同一目录） | **343 ms**（4 个文件，含 566 MB） | **17.3 ms**（3 个文件；566 MB 不在计划里） | 见实测 |
| `--reverify-models` 启动成本（公式启用） | 不可能（检测模型从不进计划） | **379.7 ms**（5 个文件：文本 3 + 566 MB + 80 MB 检测器） | 见实测 |

## 测试（新增 17 条；既有断言的期望变更逐条给理由）

测试数量：**lib 401（不变）+ bin 257（242 → 257，+15）+ 集成 2（1 → 2，+1）**。
bin 内 `#[test]` 计数：`model_plan` 10 → 17、`run` 9 → 13、`serve::tests` 74 → 78、`server` 5 → 5。

### 新用例（逐条对应要求的 5 项）

| 要求 | 用例 |
| --- | --- |
| 1（检测模型损坏 → A1 失败点名 + A2 列出并重算摘要） | `serve::run::tests::the_startup_gate_fails_on_a_corrupt_formula_detector_and_names_it`、`serve::tests::reverify_cold_verifies_the_formula_detector_and_reports_it_per_pipeline`、`tests/serve_startup.rs::the_serve_command_refuses_to_start_when_the_formula_detector_is_corrupt` |
| 2（识别模型损坏：启用时拦截 / 未启用时不影响且不哈希） | `serve::run::tests::the_startup_gate_fails_on_a_corrupt_formula_recognizer_when_formula_is_enabled`、`serve::run::tests::a_corrupt_formula_recognizer_is_out_of_scope_when_formula_is_disabled`、`serve::model_plan::tests::a_corrupt_formula_recognizer_blocks_the_formula_pipeline_when_it_is_planned`、`serve::tests::reverify_recomputes_digests_that_the_cache_would_have_answered`（断言 `computed == 3`） |
| 3（文本就绪 + 公式损坏显式） | `serve::tests::reverify_reports_text_ready_and_formula_corrupt_explicitly` |
| 4（集合之外、无声明摘要 → 在计划里、被冷验证、状态如实、规则生效） | `serve::model_plan::tests::an_external_detector_without_a_declared_digest_follows_the_documented_rule`、`serve::run::tests::the_startup_gate_covers_an_external_detector_without_a_declared_digest`、`serve::tests::an_external_detector_is_reported_honestly_without_a_declared_digest` |
| 5（A1/A2/`/api/models`/准入/加载路径同一份计划） | `serve::tests::every_entry_point_reads_the_same_run_plan`、`serve::model_plan::tests::one_plan_feeds_reverification_the_snapshot_and_the_loading_paths` |
| 计划形状（新增） | `serve::model_plan::tests` 的 `the_formula_pipeline_is_planned_only_when_a_detector_is_resolved`、`a_set_declared_detector_is_planned_once_with_its_declared_digest`、`a_declared_digest_is_enforced_and_is_the_authority`、`reverification_covers_exactly_the_plan`、`the_classifier_joins_the_plan_when_use_cls_is_on` |

关键断言（都是**绝对数字**或**逐字段**，不是"看起来对"）：

- 需求 1：损坏的真实检测模型 → `files[]` 里 `{role: formula_detector, pipeline: formula,
  state: corrupt, declared_sha256: <集合声明值>, sha256: <这一次算出来的值>,
  digest_computed_this_call: true}`，`computed == 5`，`pipelines.text.outcome == "ready"`
  与 `pipelines.formula.corrupt == ["mfd.onnx"]` 同时出现；公式队列 409
  （`detail.corrupt == ["mfd.onnx"]`、`detail.scope == "formula"`）而普通 OCR 202 → 任务
  `succeeded`；进程边界上 `mfd.onnx` + `formula pipeline` + `corrupt` 都在 stderr，stdout 里
  有 `--reverify-models mfd.onnx … digest computed by this call: true`。
- 需求 2：未启用公式时同一份损坏目录 `digests_computed == 3`、`files` 里没有 `fx.onnx`、
  gate 返回 `Ok`；启用时同一份损坏让 gate 返回 `Err` 且错误里有 `formula pipeline`、
  **没有** `text pipeline:`（避免把文本管线误报成肇事者）。
- 需求 4：集合之外的文件 `declared_sha256: null`、`has_declared_digest: false`、
  `sha256 == sha256_file(该文件)`（独立算一遍对照）；空文件/非 ONNX 内容 → `corrupt` →
  运行期公式队列 409 点名它；文本 OCR 全程 202。
- 需求 5：同一个 `ModelPlan::resolve` 算出的期望清单，经 `/api/models.pipelines.*.files`、
  A2 的 `files[].name`（同序）、`ServeShared::formula_policy()` 的
  `model_path`/`detector_path`/`expected_*_sha256`、公式队列 409 点名的文件名四个入口读回来
  必须一致。

### 既有断言的期望变更（都是"预期行为发生了正确变化"）

1. `reverify_recomputes_digests_that_the_cache_would_have_answered`：`computed == 4` → **3**
   （旧值把本轮不会加载的 566 MB 也算进去了），并**新增**"`fx.onnx` 不在 `files[]` 里"、
   "`pipelines.formula.in_plan == false`"、"`files[].name == [det, rec, dict]`"三条断言。
   同一用例里"第一次 `/api/models` 的 `cold_this_call == 0`"改成"**第二次**必须为 0"：
   计划不再为不在计划里的公式模型读盘，因此**库存**报告第一次见到它时仍会算一次摘要
   （页面需要它的状态来给下载按钮），这是本轮**可见的**行为变化，不是缓存退化。
2. `reverify_reports_a_corrupt_model_and_leaves_the_service_blocked_not_stale_ready`：
   `computed == 4` → **3**，并新增 `pipelines.text.outcome`/`pipelines.formula.in_plan` 的断言。
3. `concurrent_reverifications_are_single_flight_and_never_run_on_the_accept_thread`：
   `computed == 4` → **3**（同上）。
4. `a_cli_formula_detector_is_verified_by_the_same_rule`：旧用例给一个**任何模型集都没有声明
   过**的文件硬塞了一个"声明哈希"（`with_verified_formula_detector`），那正是"造一个看起来
   校验过的形状"。现在该文件的 `sha256` 是 `null`，夹具改成"像 ONNX"的内容，并把
   "内容被换掉 → 公式队列在读 body 之前 409"这条断言**保留并加强**（不靠任何伪造摘要）。
5. `a_corrupt_formula_detector_is_reported_and_refused_before_the_body`：去掉测试注入的声明
   哈希（`detector_model_dir` 的清单本来就声明了 `mfd.onnx`，解析器自己会带上它），并新增
   `pipelines.formula.files == ["fx.onnx","mfd.onnx"]` 与 `in_plan` 断言。
6. `serve::run::tests` 的夹具签名从 `fixture_dir(name)` / `plan_for(dir)` 改成
   `fixture_dir(name, with_detector)` / `plan_for(dir, detector)`；"健康模型"用例从
   `files.len() >= 4` 改成**恰好 5**（`digests_computed == files.len()` 仍然成立），
   并断言公式管线在计划里（2 个文件）。
7. `models_reuses_the_verified_digest_until_the_file_identity_changes`：**断言没变**
   （仍然要求第一次之后 `cold_this_call == 0`、替换后 `== 1`），但新增了
   `cache_serial()` 串行守卫：本轮新增 4 个调用 `POST /api/models/reverify`（= 清**进程级**
   缓存）的用例，会让"精确计数"变成概率事件（A1/A2/B 轮已经记录过同类偶发失败）。
   凡是"清缓存"或"断言这一次算了几个"的用例现在都持有同一把锁。

### 被删除/搬迁的既有用例

- **删除** `serve::model_plan::tests::the_formula_roles_resolve_through_the_same_rule`：
  它断言的是本轮删掉的公开形状（`ModelPlan::formula_recognizer()` +
  `resolve_formula_detector()`）。它的两条断言被**搬进**新用例：
  默认表下公式识别模型必须是 `pp_formulanet_plus_m.onnx` 且带 64 字符声明摘要
  （`the_formula_pipeline_is_planned_only_when_a_detector_is_resolved`），
  检测模型的声明摘要跟随被选中的文件（`the_formula_detector_hash_travels_with_the_selected_file`，
  保留并加强了"计划里只有它一个"的断言）。**没有**删除任何断言本身。
- **无**其它删除；`docs/03`、`tests/baseline/` 未改动。

## 实测

### `--reverify-models` 的启动成本（公式启用 vs 未启用）

`target/plan-gate/run-reverify-startup-cost.ps1` → `startup-cost.log`（真实 release 二进制、
真实 `target/m4-model-dir` 的默认表模型、真实 80 MB 检测器副本；每档 3 轮）：

```text
ROUND[formula-enabled/1] files=5 ms=379  digests=5 blocking=0
ROUND[formula-enabled/2] files=5 ms=379  digests=5 blocking=0
ROUND[formula-enabled/3] files=5 ms=381  digests=5 blocking=0
SUMMARY[formula-enabled]  rounds=3 files=5 digests=5 mean_ms=379.7  min_ms=379 max_ms=381
ROUND[formula-disabled/1] files=3 ms=18  digests=3 blocking=0
ROUND[formula-disabled/2] files=3 ms=17  digests=3 blocking=0
ROUND[formula-disabled/3] files=3 ms=17  digests=3 blocking=0
SUMMARY[formula-disabled] rounds=3 files=3 digests=3 mean_ms=17.3 min_ms=17 max_ms=18
```

- **公式启用 = 379.7 ms / 5 个文件**（文本 3 + 566 MB 公式识别 + 80.3 MB 检测器）；
- **公式未启用 = 17.3 ms / 3 个文件**（566 MB 与检测器都不在计划里，一个字节都不读）；
- **对照（修改前，同一台机器/同一份协议/同一个模型目录）**：A1/A2/B 轮记录的启动期验证是
  **343 ms / 4 个文件**（当时公式未启用，却仍然包含 566 MB 的 `formula_recognizer`）——
  即同一场景从 **343 ms → 17.3 ms**，差别就是"不再读本轮不会加载的 566 MB"；
- `/api/models` 的**库存**报告在公式未启用时仍会在第一次见到 566 MB 那个文件时算一次摘要
  （页面要显示它的状态并给下载按钮），这一次会计入 `verification.cold_this_call`；
  A1/A2/启动快照都不碰它。这一条写在 `docs/05` §10 第 10 条里。

### A2 与 `/api/models`（真实 566 MB + 真实 80 MB 检测器）

`target/plan-gate/run-12-images.ps1` → `http-vs-cli.log`（真实 release 服务，公式路由开启，
检测器是 `OCR-Model/Formula-Detection-Model/pix2text-mfd-1.5.onnx` 的副本）：

```text
plan.text.files=PP-OCRv6_det_small.onnx,PP-OCRv6_rec_small.onnx,ppocrv6_dict.txt
plan.formula.files=pp_formulanet_plus_m.onnx,pix2text-mfd-1.5.onnx in_plan=True complete=True
plan.formula.detector: configured=True file=pix2text-mfd-1.5.onnx sha256= state=present
POST /api/models/reverify: HTTP 200 wall_ms=549.75 outcome=ready computed=5 content_changed= load_ms=545
pipelines.text: outcome=ready complete=True files=PP-OCRv6_det_small.onnx,PP-OCRv6_rec_small.onnx,ppocrv6_dict.txt
pipelines.formula: in_plan=True routing=True complete=True files=pp_formulanet_plus_m.onnx,pix2text-mfd-1.5.onnx blocked=
  reverify file pp_formulanet_plus_m.onnx role=formula_recognizer pipeline=formula state=present declared=True cause=first_sight computed=True
  reverify file pix2text-mfd-1.5.onnx role=formula_detector pipeline=formula state=present declared=False cause=first_sight computed=True
detector edited (first byte): state=corrupt blocked=pix2text-mfd-1.5.onnx
ordinary OCR with a corrupt real detector: status=202
formula queue with a corrupt real detector: status=409 code=models_corrupt scope=formula corrupt=pix2text-mfd-1.5.onnx
detector restored: state=present blocked=
second /api/models: cold_this_call=0 cache_hits=30 partial_reads=34
POLLING_DOES_NOT_REHASH=True
```

- **检测模型真的进了计划**：`computed == 5`（旧实现是 4，且 `files[]` 里没有它）；
  `declared=False`（集合没有声明它）但 `computed=True`（这一次真的算了摘要）——正是本轮
  要的"如实"。
- **按管线**：`text.outcome=ready` 与 `formula.blocked=[]`（健康时）分别可见；把真实检测器
  的首字节改坏之后，`/api/models` 报 `corrupt`、**普通 OCR 仍然 202**、公式队列 409
  `models_corrupt` + `scope=formula` + 点名文件，恢复后回到 `present`。
- A2 的墙钟 **549.8 ms** 里 `load_ms=545`（含 566 MB + 80 MB 的冷验证与一次真实建会话），
  与 A1/A2/B 轮同一端点的 453–546 ms 同一量级（多读的 80 MB 在这个尺度上不可分辨）。

## 验证

### 1. 静态检查、feature 矩阵与 release 构建（全部 exit 0）

`target/plan-gate/run-gates.ps1` → `gates.log`：

| # | 命令 | 结果 |
| --- | --- | --- |
| 1 | `cargo fmt --all -- --check` | 0 |
| 2 | `cargo clippy --all-targets -- -D warnings` | 0 |
| 3 | `cargo clippy --features serve --all-targets -- -D warnings` | 0 |
| 4 | `cargo test --all-targets` | **401 passed**（lib）+ 2 + 4 + 14 + 0，**0 failed** |
| 5 | `cargo test --features serve --all-targets` | **401 passed**（lib）+ 2 + 4 + 14 + **257 passed**（bin）+ **2 passed**（`serve_startup` 集成），**0 failed** |
| 6 | `cargo build --release --bins` | 0 |
| 7 | `cargo build --release --features serve --bins` | 0 |

### 2. 依赖隔离（三份依赖树与 A1/A2/B 快照**逐行 0 差异**）

`Cargo.toml` 的 `[dependencies]` / `[features]` 一个字未改；本轮没有新增测试目标。

| 命令 | 结果 |
| --- | --- |
| `cargo tree -e normal -p rapid-ocr-rs` | **606 行**，与 `target/reverify-gate/tree-default.txt` **逐行相同**；`tiny_http` **0** 次 |
| `cargo tree -e normal --no-default-features` | **605 行**，逐行相同；`tiny_http` **0** 次 |
| `cargo tree -e normal -p rapid-ocr-rs --features serve` | **611 行**，逐行相同；`tiny_http` **1** 次 |

（`tiny_http` 仍然只出现在 `src/bin/serve/http.rs`：`serve::dependency_boundary` 的两条边界
断言同时通过。）

### 3. `cargo package --allow-dirty` 与打包树的 `--features serve` 检查

```text
Packaged 191 files, 14.3MiB (3.4MiB compressed)
Verifying rapid-ocr-rs v0.7.0 (…\target\package\rapid-ocr-rs-0.7.0)
Finished `dev` profile … in 16.93s        （exit 0；target/plan-gate/package.log）
```

`cargo package --list` 里 `src/bin/serve/model_plan.rs`、`src/bin/serve/server.rs`、
`src/bin/serve/tests.rs`、`src/bin/web/index.html`、`tests/serve_startup.rs` 都在；打包树上
`cargo check --features serve --all-targets` **exit 0**（25.02 s，
`target/plan-gate/package-serve-check.log`）。

### 4. 12 图硬门槛（`target/plan-gate/`，**没有**覆盖 `tests/baseline/`）

`target/plan-gate/run-hard-gates.ps1` → `hard-gates.log`：

| 门槛 | 要求 | 本次实测（release） | 比较方式 | 结论 |
| --- | --- | --- | --- | --- |
| 12 图区域数均值 | `34.833333333333336` | **`34.833333333333336`**（36 样本） | 原始 JSON 的**数字字面量**精确比较 | True |
| 12 图 mean CER | `0.44765135645866394` | **`0.44765135645866394`**（12 例） | 同上 | True |

`tests/baseline` 未被改动（脚本内 `git status --porcelain -- tests/baseline` 为空，
`bench-cpu-2000.json` 的 `regions.avg` 与本次字面量相同）。

### 5. 12 图 HTTP 与 CLI 逐张一致（**12/12**）

`target/plan-gate/run-12-images.ps1` → `http-vs-cli.log`（本次**打开**了公式路由 + 真实检测器，
这是本轮唯一新增的端到端维度）：

```text
TOTAL serve=418 cli=418 images=12
ALL_12_MATCH=True
```

逐张 `serve=N cli=N count=True texts=True`（明细在日志里）；`/api/models` 第二次
`cold_this_call=0`（`POLLING_DOES_NOT_REHASH=True`）。

### 6. 环境变量门控的 `formula_integration_tests`（真实模型，0 skipped）

```powershell
$env:RAPID_OCR_MODEL_ROOT='D:\100_Projects\110_Daily\SnapClip\OCR-Model'
$env:RAPID_OCR_FORMULA_TEST_ROOT='D:\100_Projects\110_Daily\SnapClip\Formula-TestSet'
cargo test --lib formula_integration_tests -- --test-threads=1
```

**12 passed, 0 failed, 0 ignored**（83.71 s），`skipping test` 出现 **0** 次；
`target/plan-gate/formula-integration.log`。

### 7. im2latex-100 smoke（本轮**没有**改库公式代码，仍然重跑作为额外证据）

```text
done: total=100 scored=100 exact=0.2400 normalized=0.2500 mean_cer=0.0863 pipeline_failures=0
      model_mismatches=76 truncated=0 load_ms=1271.6 wall_ms=67205.4
```

即 **24.00% / 25.00% / 0.0863 / 0 pipeline failures** 全部复现
（`target/plan-gate/im2latex-smoke.log`）。

### 8. `docs/05` 与 `README` 的改动

| 位置 | 改动 |
| --- | --- |
| `docs/05` §3 | `--reverify-models` 的范围改成"整份运行计划"（含公式启用时的检测模型）；`--formula-detector` 说明它进计划、且集合之外时"验证"按 §4.2.1 的规则 |
| §4.2 端点表 | `/api/models` 的作用域（计划/库存）与 `/api/models/reverify` 的"按管线报告" |
| §4.2.1 | 公式 409 的清单改成**运行计划**口径（含 `in_plan`）；新增"一份运行计划"、"库存 vs 计划"、"没有声明摘要时的文档化规则"、"启动期与运行期的两种语义"四段 |
| §5.4 响应 | 新增 `pipelines` 块；把四个顶层字段改成**计划**的文本管线口径；说明 `formula` 块是**库存**口径（页面下载依赖它）；`complete` 的两个口径分别写清 |
| §5.5 响应 | 新增 `pipelines` 与逐字段语义（含 `has_declared_digest`、`declared_sha256: null` 时仍报告算出的摘要） |
| §7.6 启动顺序第 3 步 | 新增 `--reverify-models` 的整份计划 fail-fast 与"运行期仍按管线" |
| §9.2 布局 | 结论区显示 `pipelines` 的公式结论（"文本就绪、公式损坏"不被合并） |
| §10 第 10 条 | 两个确定性入口**都只碰运行计划**；库存报告首次见到计划外文件仍会算一次摘要 |
| §11 | 新增"计划轮"清单（P1 的根因、文档化规则、逐管线报告），逐条指向被改正的章节 |
| §12 | 更新 A1/A2 两行，新增"一份运行计划"与"逐管线报告"两行 |
| `README` "Model integrity" | 新增 "One run model plan" 与 "What \"verified\" means without a declared digest" 两节（把两种语义与规则写进英文文档） |

`docs/03` 未改动（`git status` 为空）。

## 未覆盖风险与**做不到的事**（如实）

1. **未提交、未在干净 clone 上验证**：按要求不 commit；证据来自当前工作树
   （`a04d6bc` + 本轮改动）。
2. **"像 ONNX"是启发式**：`0x08` + `ir_version` varint 是 protobuf 序言，能拦住"随便一个
   文件冒充模型"，**拦不住**能构造一个看起来像 ONNX 的坏文件的人。没有可信摘要时服务
   **不声称**内容被校验过（`sha256: null` 写在响应里），这一条同时写进 `docs/05` §4.2.1
   与 `README`。
3. **`/api/models`（库存）在公式未启用时仍会为计划外的 566 MB 文件算一次摘要**：页面用它
   显示状态、决定下载按钮，因此这是有意的；A1/A2/启动快照都不碰它（实测 17.3 ms）。
   如果将来要让 `/api/models` 也完全不读它，就得放弃"无哈希/无读取也能报 `present`"的
   诚实性，属于另一个决策。
4. **`pipelines.formula.complete=false` when `in_plan=false`**：这是有意选择（"没计划"不是
   "计划齐备"），消费者必须同时看 `in_plan`；文档与测试都写明这一点，但它是协议里的一个
   新约定，客户端需要读它。
5. **公式准入的"计划外"分支不可达**：路由未启用时 `queue=formula` 在读 body 之前就是 400
   （M1 起不变），因此公式准入只会在"公式在计划里"时执行；"不在计划里"的形状只在
   `/api/models` 的 `pipelines` 与 A2 的响应里被测试到。
6. **A2 的墙钟由"重建会话"主导**（549.8 ms 里 545 ms），冷验证 646 MB 的读写被包在同一个
   窗口里，因此本轮没有单独测"只做冷验证"的耗时；启动期那一组（379.7 ms / 17.3 ms）
   是纯冷验证的数字。
7. **页面只做了结构与接线断言**（`c.pipelines`/公式管线行/无内联 handler/style/占位符计数），
   **没有**浏览器自动化；页面真正的 `<script nonce=…>` 块用 `node --check` 验证通过
   （`target/plan-gate/page-script-2.js`）。
8. **夹具跨进程残留被发现并修掉（如实记录）**：`target/m1-serve-tests/` 下的夹具目录在
   进程之间保留，而 `unique()` 每次从 0 开始；`with_formula_routing` 最初只在"文件不存在"
   时写夹具，于是**上一轮遗留的旧内容**（不像 ONNX）让两个公平性用例在第一轮全量测试里
   失败（公式队列 409 而不是 202/503）。修法是"无条件重写夹具"，不是放宽断言。
9. **进程级校验缓存的测试串行化**：本轮新增 4 个"清缓存"用例，因此给"清缓存/精确计数"
   的用例加了 `cache_serial()` 守卫（与库内 `model_verify::tests::serial()` 同一思路）。
   不持有它的用例不断言精确的 `cold_this_call`，因此不受影响；但这条依赖是**约定**，
   将来新增"精确计数"用例必须一并持有它。


---

## 流程轮（浏览器闭环）：上传后一直转圈的根因、流动日志与契约钉住

本轮由报告者的**真实浏览器行为**驱动，而不是由 curl 驱动的既有 12 图门槛：

```powershell
.\target\release\rapidocr.exe serve --port 8760 `
  --model-dir "D:\100_Projects\110_Daily\SnapClip\OCR-Model\small" `
  --config    "D:\100_Projects\110_Daily\SnapClip\OCR-Model\test-config-small.yaml" `
  --reverify-models --open
```

启动健康（BCryptGenRandom 熵、3 个文件 17 ms 冷验证、监听 127.0.0.1:8760、模型目录
`<...>\OCR-Model\small` source `default_table`、文本管线齐备、公式不在本次运行里、
provider cpu / selected_ep cpu / fallback false、`service state = Ready, engine state = ready`、
浏览器已打开）。**症状**：上传一张图之后，左栏的扫描动画一直转，右栏永远没有结果。

### 1. 先确认服务端没问题（把"引擎 / 任务流水线"从嫌疑名单里划掉）

按**页面发请求的方式**（不带查询串、`application/octet-stream`、`X-RapidOCR-Token` +
`Origin: http://127.0.0.1:8760`）驱动报告者**仍在运行的那个进程**：

```text
token=cb48aa2ebef941e70ae55731c987ba5b48e0abf5345e7ef1956bde56def8bfba   （从 GET / 里按页面的方式取出）
POST /api/ocr -> 202 in 18.3 ms
body: {"job_id":"job-0000000000000001","kind":"ocr","position":0,"queue":"text","state":"queued"}
polls=6 terminal state=succeeded
GET /result -> 200 content-type=application/json; charset=utf-8 bytes=35203
```

原始响应体的形状（**逐字段**核对，不看推测）：

```text
TOP KEYS: engine,formulas,image,items,plain_text,regions,schema_version,stages,text,timing_ledger,timings
regions is Array: true len: 42
first region keys: classification,detection,kind,polygon,recognition,source
kind counts: {"text":42}
regions without polygon: 0
regions whose polygon.points is not exactly 4: 0
text regions without recognition: 0
regions with bad recognition.text/score: 0
plain_text present: string      timings present: object      timing_ledger present: object
```

即**失败模式 (a)（响应形状被页面拒绝）不成立**：这份字节完全符合页面冻结契约里的每一个
必需路径。原始体已存为夹具 `tests/fixtures/serve/result-real-42-regions.json`
（35 203 字节，SHA-256 `fda2f71337aa8b03f147d5fb4b0065981fe0444703c30dfb74573439f691fd27`）。

### 2. 真实浏览器点击闭环：稳定复现，并定位到**页面控制流**

用 headless Chrome + CDP 打开**真实服务**上的**真实页面**，用
`DOM.setFileInputFiles` 选一张 `OCR-test-image` 里的真图，再点"开始识别"——即报告者的动作。
仪器是页面**自己**的 `XMLHttpRequest`（`Page.addScriptToEvaluateOnNewDocument` 包装
`open`/`send`，记录方法 / URL / 状态 / 耗时 / 响应体），因此"页面到底发了什么"不是推断：

```text
=== 页面自己的 XHR 记录（修复前）===
  GET  /api/status   -> 200
  GET  /api/models   -> 200
  POST /api/ocr      -> 202  6ms
  GET  /api/status   -> 200          ← 8 秒一次的状态轮询
  GET  /api/status   -> 200
```

**页面一次 `GET /api/jobs/{id}` 都没发。** 同一时刻的 DOM 状态（20 秒采样，每 500 ms 一次）：

```text
stage class: "stage scanning"     ← 扫描动画一直在转
region items: 0                   ← 右栏永远空
#jobLine hidden: true             ← 任务行从未出现
toasts: ["任务已提交（202）"]      ← 202 收到了
console exceptions: []            ← 没有任何异常
network failures: []              ← 没有任何失败的请求
```

于是**失败模式 (b) 成立，但成因不是 HTTP 错误**：没有 401/403/409/413/415/422/503，
也没有契约失败——**页面在收到 202 之后把任务丢了**，所以既没有轮询，也没有任何一条
"终结路径"来关掉动画。服务端 meanwhile 把活干完了：报告者那个进程里被页面丢弃的任务
（`job-0000000000000003`…`0000000000000006`，含浏览器那一次收到的
`job-0000000000000006`）逐个查回来都是：

```text
job-0000000000000003 state=succeeded elapsed_ms=1277 /result=200 bytes=35221 regions=42
job-0000000000000004 state=succeeded elapsed_ms=858  /result=200 bytes=35215 regions=42
job-0000000000000005 state=succeeded elapsed_ms=1181 /result=200 bytes=35202 regions=42
job-0000000000000006 state=succeeded elapsed_ms=885  /result=200 bytes=35201 regions=42
```

### 3. 根因：`state.job` 在装好之后**下一行**就被清掉了

`src/bin/web/index.html` 的 `submitImage()` 成功分支（修复前）：

```js
state.job = { id: data.job_id, kind: data.kind || 'text', … };   // ① 装好刚被 202 接受的任务
clearResult(false);                                             // ② 它里面有一句 state.job = null
toast('任务已提交（202）', 'ok');
renderJob(); renderButton(); scanOn();                          // ③ 看到"没有任务"：隐藏任务行
startPolling();                                                 // ④ tick() 在 if(!j) return; 直接返回
```

而 `clearResult()`（第 1082 行）是：

```js
function clearResult(resetList){
  state.result = null; state.selected = -1; state.job = null;   // ← 把①装好的任务清掉
  scanOff(); renderJob(); drawOverlays();
  …
}
```

链条因此完全闭合，且与上面每一条观测一一对应：

| 观测 | 由哪一行造成 |
| --- | --- |
| 页面一次 `GET /api/jobs/{id}` 都没发 | ④ `startPolling` 的 `tick()` 第一句 `if(!j) return;` |
| `#jobLine` 隐藏 | ③ `renderJob()` 的第一句 `if(!j){ jl.hidden = true; return; }` |
| `stage scanning` 一直在 | ③ 的 `scanOn()` 点亮，而 `scanOff()`（只在 `fetchResult`/`onJobEnd` 里）永远到不了 |
| 右栏永远空 | `finishJob()` / `renderRegions()` 永远到不了 |
| 没有任何异常 / 报错 toast | 代码路径本身"成功"了：它只是做了一件错的事 |

**这是"职责边界"的错，不是"两行顺序写反"的错**：`clearResult()` 同时承担了两件事
（清上一次的**结果** / 清上一次的**任务**），于是任何"刚装好一个新任务"的调用方都会被它
吃掉。旁证：离线预览的 `demoUpload()` 里顺序是 `clearResult(false); state.job = {…}`——
同一个函数、同一个意图，只因为顺序不同就正常工作。

**顺带发现的第二个契约漂移（同一段代码）**：`docs/05` §2.2/§4.2 写着"页面的公式识别路由
开关**只**通过 `?queue=formula` 表达"，"而页面本来就已经按开关发送 `queue=formula`"。
实际上 `submitImage()` 发的是**不带查询串**的 `/api/ocr`：公式开关点开之后没有任何效果
（服务端按 `queue=text` 跑文本管线）。修复后页面显式发送 `?queue=<text|formula>`，
并由 `serve::tests::the_page_reads_exactly_the_pinned_result_paths` 钉住源码里这一句。

### 4. 修复：把任务生命周期交给**唯一的**所有者，并让每条终结路径都清场

**页面**（`src/bin/web/index.html`）：

1. **拆职责**：`clearResult()` 只重置上一次的**结果**（不再碰 `state.job`）；
   新增 `clearJob()` 只重置**任务状态行**；`setImage()`/`clearImage()` 显式调用两者。
2. **两个所有者**：`beginJob(submitted, queueFallback, poll)` 是唯一的入队入口，
   `endJob(message, type, icon)` 是唯一的终结入口——`scanOff()` + `state.job = null` +
   `renderJob()` + `renderButton()` 只在 `endJob()` 里做一次。**每一条**终结路径都走它：
   成功、失败、取消、淘汰（404/410）、鉴权/契约类拒绝（401/403/400/409）、
   结果获取失败（413/422/503/…）、**契约失败**（`normalizeResult` 返回 null）、
   `POST /api/ocr` 失败。
3. **202 的响应体也校验**：`job_id` 不是非空字符串就**可见地**失败（toast + 诊断面板），
   绝不进入"没有 job id 却在轮询"的状态。
4. **瞬时错误有终点**：轮询失败从"无限退避重试"改成 8 次上限 + 指数退避（480 ms → 4 s，
   约 23 s 窗口），到顶时给出**点名原因**的可见错误。这不是"用超时掩盖问题"：它把一个
   原本永远不会出现的结论变成一条明确的消息，而 404/410/401/403/400/409 立即终结。
5. **不再有"只有一个 toast"的失败**：终结与失败同时进诊断面板（`diagPush`），
   因此"看得见"（toast）与"查得到"（诊断面板）两条路径都有。

**页面侧的详细模式**（报告者要的"不打开 devtools 也能看见为什么什么都没渲染"）：
`?verbose=1`（`?verbose=0` 关闭）或 **Ctrl+Alt+L** 切换，选择记在 `localStorage`；
诊断面板顶部因此出现"最近请求"一节：方法、URL、HTTP 状态、耗时、错误码，**失败时附响应体原文**
（截断 4 KB）。开关那个复选框**永远**在面板里（一个只有知道 URL 参数的人才能打开的开关不算可用）；
关闭时这一节只在最近一次请求失败时出现。

**修复后的同一条真实浏览器闭环**（`tools/run-page-flow-cdp.ps1`，同一个服务、同一张图，
跑的是**最终** release 二进制）：

```text
=== 页面自己的 XHR 记录（修复后）===
  GET  /api/status                     -> 200
  GET  /api/models                     -> 200
  POST /api/ocr?queue=text             -> 202
  GET  /api/jobs/job-0000000000000000  -> 200
  GET  /api/jobs/job-0000000000000000  -> 200
  GET  /api/jobs/job-0000000000000000  -> 200
  GET  /api/jobs/job-0000000000000000  -> 200
  GET  /api/jobs/job-0000000000000000/result -> 200

.stage class            -> stage        ← 动画已停
rendered regions        -> 42           ← 右栏 42 个区域
#jobLine hidden         -> false
toasts                  -> ["任务已提交（202）· 文本队列", "识别完成 · 1.22 s"]
console exceptions      -> []
first region            -> "01 文本 VISUAL TEXT BENCHMARK"
```

**如实说明**：为了验证修复，我停掉了报告者那个仍在运行的 `rapidocr serve`（PID 50568，
命令行与报告里的命令逐字相同）——它锁住了 `target\release\rapidocr.exe`，任何 release 构建
都会失败（`os error 5`）。"修复前"的全部证据（页面自己的 XHR 记录、DOM 采样、原始 `/result`
字节、被丢弃任务的回查结果）在停掉它之前已经落盘到 `target/flow-gate/`。之后的"修复后"
验证用的是**同一条命令行**（只去掉 `--open`，避免自动弹出桌面浏览器与自动化打架）。

### 5. 服务端流动日志（报告者明确要求的那一条）

新增 `src/bin/serve/flowlog.rs`。开关：`--log-level <off|flow>`（默认 **off**）或环境变量
`RAPID_OCR_SERVE_LOG`，**CLI 优先**。真实输出（`target/flow-gate/flow-demo-output.txt`）：

```text
unknown level: exit=1
unknown level stderr: error: startup configuration rejected: --log-level / RAPID_OCR_SERVE_LOG
  rejected: unknown log level `chatty`; expected `off` or `flow`

=== startup line with --log-level flow ===
serve: flow logging: --log-level=flow (RAPID_OCR_SERVE_LOG=<unset>); one line per HTTP request and
  per job lifecycle is on: request id, method/path/status/bytes/duration, and the job's
  admission/running/terminal lines share that request id

=== startup line with the default (off) ===
serve: flow logging: --log-level=off (RAPID_OCR_SERVE_LOG=<unset>); one line per HTTP request and
  per job lifecycle is off (pass --log-level flow to turn it on)
flow lines written while off: 0
```

`--log-level flow` 下的一次真实上传（14 行里属于这次上传的 4 行都带 `req=3`）：

```text
serve-flow: req=3 job=job-0000000000000000 queue=text admission=accepted decision=run queued position=0
serve-flow: req=3 job=job-0000000000000000 queue=text running wait_ms=0
serve-flow: req=3 POST /api/ocr -> 202 bytes=91 in 5ms
serve-flow: req=3 job=job-0000000000000000 queue=text terminal state=succeeded result_bytes=35215 backend_ms=876
```

（`RAPID_OCR_SERVE_LOG=flow` 走同一个开关，实测写出 3 行；**默认 `off` 时一行都没有**。
请求行按响应写出的顺序出现，因此 `POST /api/ocr` 那一行排在 `running` 之后——worker 在响应
写回之前就把它取走了；把一次上传串起来靠的是请求 id，不是行序。）

设计要点（都可核对）：

| 要点 | 实现 |
| --- | --- |
| 请求行覆盖**每一个**请求 | 只在 `http::respond` 落笔；它是所有响应的唯一出口（accept 线程、引擎工作线程、评估线程都经过它） |
| 一次请求只有**一个**请求 id | `RequestFlow` 在 `handle()` 里创建一次；交给别的线程时传**克隆**（同一个 id），线程起不来时原件仍在调用方手里，绝不因为换了分支就换 id |
| 请求 id 贯穿任务生命周期 | 建任务时把 `flow.id()` 存进 `JobRecord.request_id`（**不进** `/api/jobs/{id}`——冻结契约不因为日志需要内部字段而变），worker 的 `running`/终态行从记录里读回它 |
| 准入结论**带原因** | `admit_ocr()` 返回原因串，取值直接来自驱动判定的那一份 `OcrAdmission`（`run` / `queue waiting_for=loading\|rebuilding` / `models_on_disk`），不是日志里另拼的说法 |
| 被拒绝的上传也留痕 | 没有任务 id，请求 id 就是唯一关联键；`code`/`detail` 来自用户实际收到的**同一个** `ServeError`（含读 body 之前的队列满 / token / Host / Origin / 长度拒绝，以及公式模型的 409 预检、下载准入的拒绝）。`?queue=formla` 这种非法取值记的是**原值**，不冒充 `text` |
| 失败的任务行自带原因 | `status=` + `code=` + `detail=`（与 `/result` 上重放的是同一份错误体） |
| 后端时长是**实测**的 | `ocr_worker` 在调用两侧取 `Instant`；引擎建不起来那一支根本不调后端，因此 `backend_ms=0` 是事实而不是缺省值 |
| 关闭时不付代价 | 只有一个级别开关（`Off` 默认 / `Flow`）；`Off` 下每个记录函数第一句就返回，不做任何格式化。测试出口 `FlowSink::Capture` 在 `#[cfg(test)]` 之下，**发布二进制里根本不存在**这段代码 |
| 可断言 | `serve::flowlog` 的 **6** 条单元用例 + `serve::tests` 的 **4** 条流动日志端到端用例逐行断言这些行（含"同一个请求 id 恰好 4 行""默认一行都不写""失败带 code+detail"） |

### 6. 契约钉住：四条互相独立的腿，外加"检查会失败"的证据

| 腿 | 名字 | 钉住什么 |
| --- | --- | --- |
| Rust（服务端） | `serve::tests::the_served_result_carries_every_path_the_page_reads` | 真实端点（脚本化后端产出 3 个文本 + 1 个公式区域）的 `/result` 满足页面读的每个路径：`regions` 数组、`kind` ∈ {text,formula}、`polygon.points` **恰好四点**且每点**恰好两个有限数**、文本区域有 `recognition.text`/`recognition.score`、公式区域有 `formula.latex`（且**不要求** `recognition`）、`plain_text`/`timings`/`timing_ledger` |
| Rust（页面源码） | `serve::tests::the_page_reads_exactly_the_pinned_result_paths` | `PAGE_RESULT_CONTRACT` 表里的 11 条访问表达式在页面源码里**逐字**存在（含 `!Array.isArray(pts) \|\| pts.length !== 4` 与 `p.length !== 2`）、`POST /api/ocr` 必须带 `?queue=`、且**不再出现**任何别名（`r.boxes`/`r.items`/`region.text`/`polygon.box`/`polygon.coords`） |
| Rust（真实字节） | `serve::tests::the_captured_real_result_body_still_satisfies_the_page_contract` | 报告者那次真实运行的 35 203 字节 `/result` 体（42 区域，SHA-256 钉住）满足同一份契约，且顶层 8 个字段仍在 |
| node（页面**自己的**解析器） | `tools/check-page-result-contract.mjs` | 把页面自己的 `normalizeResult`/`parsePolygon`/`parseRecognition`/`parseFormula`/`diagPush`/`contractFail` 抽出来在 `node:vm` 里对**真实捕获的响应体**执行：**42 个区域必须渲染出来**；随后 9 个变异用例必须**拒绝渲染并点名字段路径** |

第 4 条既是"检查本身能不能失败"的证据（不会失败的检查不是检查），也是唯一**执行**页面
解析器的地方。它的真实输出（`target/flow-gate/page-contract-check.log`）：

```text
regions 42
extracted 9 page functions (4377 bytes)
=== the page parses the real captured body ===
regions rendered: 42
first three: VISUAL TEXT BENCHMARK | 多位置文本定位实验 | 状态：识别队列运行中
plain text: 1011 chars, ledger: read
=== the check bites: a renamed or missing field is reported with its path ===
  ok   regions renamed to items -> ["响应字段缺失: regions"]
  ok   region kind renamed to type -> ["响应字段缺失: regions[0].kind"]
  ok   polygon.points truncated to three corners -> ["响应字段缺失: regions[0].polygon.points"]
  ok   polygon point flattened to a number -> ["响应字段缺失: regions[0].polygon.points[1]"]
  ok   recognition.text removed -> ["响应字段缺失: regions[0].recognition.text"]
  ok   recognition.score removed -> ["响应字段缺失: regions[0].recognition.score"]
  ok   recognition renamed to rec -> ["响应字段缺失: regions[0].recognition"]
  ok   a formula region without formula.latex -> ["响应字段缺失: regions[0].formula.latex"]
  ok   a polygon missing on a detected region -> ["响应字段缺失: regions[0].polygon"]
```

另外 `tools/extract-page-scripts.mjs` 把页面的两个 `<script nonce=…>` 块分别抽出交给
`node --check`（页面自身语法）；这也是本轮改了页面之后的第一道闸。

### 7. 本轮的回归测试：`tools/check-page-flow-cdp.mjs`

**上面那四条契约测试都抓不到本轮这个 bug**——它们钉的是"字段名与嵌套"，而这次的响应体
逐字节正确、页面解析器也逐字正确；坏掉的是页面在 `state.job` 上的**控制流**。
因此把报告者的动作本身变成一条可执行的检查（`tools/check-page-flow-cdp.mjs` +
`tools/run-page-flow-cdp.ps1`，headless Chrome + CDP，对**真实**服务与**真实**图片）：

四条不变量（修复前的页面**每一条都不满足**）：

1. 上传之后页面**至少发一次** `GET /api/jobs/{id}`；
2. 它到达 `GET /api/jobs/{id}/result` 且是 **200**；
3. `.stage` 不再带 `scanning`；
4. `#regionList` 至少渲染出一个区域，且该区域带文本。

**它确实会失败**：对修复前的捕获跑同一条检查（`target/flow-gate/page-flow-check-before.log`）：

```text
capture: http://127.0.0.1:8760
FAIL the page must poll GET /api/jobs/{id} at least once — this is exactly what the pre-M5 page never did
FAIL the page must fetch GET /api/jobs/{id}/result
FAIL the scanning animation must be cleared, stage class was `stage scanning`
FAIL the region list must render at least one region, got 0
FAIL the first rendered region must carry text, got ""
page flow check FAILED (5 problem(s))
exit against PRE-FIX capture: 1
```

对修复后的最终二进制：`page flow check PASSED`（见 §8 第 17 项）。

**逐条回答"这个测试本来能抓到什么"**（如实，不含推测）：

| 检查 | 本轮这个 bug | 之后哪一类漂移会被它抓到 |
| --- | --- | --- |
| `check-page-result-contract.mjs`（9 个变异 + 真实体） | **抓不到**（响应体与解析器都是对的） | 任一侧把 `regions`/`kind`/`polygon.points`/`recognition.text`/`recognition.score`/`formula.latex` 改名、删字段、把四点写成三点、把点写成数字——都会被点名路径 |
| `the_page_reads_exactly_the_pinned_result_paths` | **抓不到** | 页面改成读别的名字、或偷偷加回别名兼容；页面不再发 `?queue=`（**这个它抓得到**——那正是本轮第二个缺陷） |
| `the_served_result_carries_every_path_the_page_reads` | **抓不到** | 服务端序列化改了字段名 / 嵌套（例如把 `polygon.points` 变成 `box`，或让文本区域不再带 `score`） |
| `the_captured_real_result_body_still_satisfies_the_page_contract` | **抓不到** | 契约在真实数据上失效（例如公式区域不再带 `latex`）；同时防止夹具被悄悄换掉 |
| **`check-page-flow-cdp.mjs`** | **抓得到**（对修复前的捕获它报 5 条失败） | 页面在 `state.job` 上的任何控制流回归：不再轮询、把任务丢掉、动画没关、结果不渲染、页面抛异常 |

因此"契约不会再漂"由前四条负责，"用户看到永远转圈"这一类由第五条负责——两者互补，
本轮两个缺陷各自都有对应的闸（第二个缺陷由 Rust 的页面源码断言直接钉住）。

### 8. 验证（全部 exit 0；`target/flow-gate/run-gates.ps1` → `gates.log`）

| # | 命令 | 结果 |
| --- | --- | --- |
| 1 | `cargo fmt --all -- --check` | 0 |
| 2 | `cargo clippy --all-targets -- -D warnings` | 0 |
| 3 | `cargo clippy --features serve --all-targets -- -D warnings` | **0**（本轮先失败过一次：`flowlog::job_terminal` 8 个参数触发 `clippy::too_many_arguments`；修法是把任务身份收进 `JobIdentity`，不是 `#[allow]`） |
| 4 | `cargo test --all-targets` | **401 passed**（lib）+ 2 + 4 + 14 + 0，**0 failed** |
| 5 | `cargo test --features serve --all-targets` | **401 passed**（lib）+ 2 + 4 + 14 + **271 passed**（bin）+ **2 passed**（`serve_startup`），**0 failed** |
| 6 | `cargo build --release --bins` | 0 |
| 7 | `cargo build --release --features serve --bins` | 0 |

第 5 项比上一轮的 257 条多 **14** 条，全部是本轮新增：`serve::flowlog` **6** 条单元用例
+ `serve::tests` **8** 条端到端用例（4 条流动日志：默认不写 / 一次上传可按请求 id 追到尾 /
准入拒绝带原因 / 失败终态带 code+detail；4 条契约：服务端产物 / 页面源码 / 真实捕获字节 /
`?queue=` 契约）。

**依赖隔离（三份依赖树与上一轮快照逐行 0 差异）**：

| 命令 | 结果 |
| --- | --- |
| `cargo tree -e normal -p rapid-ocr-rs` | **606 行**，与 `target/plan-gate/tree-default.txt` **逐行相同**；`tiny_http` **0** 次 |
| `cargo tree -e normal --no-default-features` | **605 行**，逐行相同；`tiny_http` **0** 次 |
| `cargo tree -e normal -p rapid-ocr-rs --features serve` | **611 行**，逐行相同；`tiny_http` **1** 次 |

`Cargo.toml` 的 `[dependencies]`/`[features]` 一个字未改（`--log-level` 是 clap 选项，
不引入任何新依赖）。`tiny_http` 仍然只在 `src/bin/serve/http.rs`：本轮新增的
`flowlog.rs` 不含它（`serve::dependency_boundary` 的两条边界断言同时通过）。

**页面自身语法**：`node tools/extract-page-scripts.mjs`（两个 nonce script 块）→
`node --check` ×2，均通过。

**12 图硬门槛**（`target/flow-gate/`，**没有**覆盖 `tests/baseline/`）：

| 门槛 | 要求 | 本次实测（release） | 比较方式 | 结论 |
| --- | --- | --- | --- | --- |
| 12 图区域数均值 | `34.833333333333336` | **`34.833333333333336`**（36 样本） | 原始 JSON 的**数字字面量**精确比较 | True |
| 12 图 mean CER | `0.44765135645866394` | **`0.44765135645866394`**（12 例） | 同上 | True |

`tests/baseline` 未被改动（脚本内 `git status --porcelain -- tests/baseline` 为空；
`bench-cpu-2000.json` 的 `regions.avg` 与本次字面量相同 = True）。

**12 图 HTTP 与 CLI 逐张一致（12/12）**：

```text
01基础多位置文本 serve= 42 cli= 42 count=True texts=True      07小字号与密集排版 serve= 37 cli= 37 count=True texts=True
02多语言与RTL混排 serve= 21 cli= 21 count=True texts=True      08数字公式与符号   serve= 51 cli= 51 count=True texts=True
03旋转与倾斜     serve= 13 cli= 13 count=True texts=True      09竖排文本         serve= 14 cli= 14 count=True texts=True
04表格与键值对   serve= 61 cli= 61 count=True texts=True      10长段落与分栏     serve= 37 cli= 37 count=True texts=True
05代码与等宽字体 serve= 38 cli= 38 count=True texts=True      11文字样式与特效   serve= 22 cli= 22 count=True texts=True
06低对比度与深色背景 serve= 21 cli= 21 count=True texts=True  12综合压力测试     serve= 61 cli= 61 count=True texts=True
TOTAL serve=418 cli=418 images=12
ALL_12_MATCH=True
POLLING_DOES_NOT_REHASH=True
```

**环境变量门控的 `formula_integration_tests`（真实模型，0 skipped）**：

```powershell
$env:RAPID_OCR_MODEL_ROOT='D:\100_Projects\110_Daily\SnapClip\OCR-Model'
$env:RAPID_OCR_FORMULA_TEST_ROOT='D:\100_Projects\110_Daily\SnapClip\Formula-TestSet'
cargo test --lib formula_integration_tests -- --test-threads=1
```

**12 passed, 0 failed, 0 ignored**（95.29 s），`skipping test` 出现 **0** 次。

**第 17 项（本轮新增的浏览器闸）**：

```text
serve ready: http://127.0.0.1:8763 (pid …)
capture: http://127.0.0.1:8763
  POST /api/ocr           -> 202
  GET /api/jobs/{id}      -> 4 poll(s) 200,200,200,200
  GET /api/jobs/{id}/result -> 1 fetch(es) 200
  .stage class            -> stage
  rendered regions        -> 42
  first region            -> "01 文本 VISUAL TEXT BENCHMARK"
page flow check PASSED: one upload polls its job, fetches its result, clears the animation and renders regions
```

**没有重跑 im2latex-100 smoke**：本轮**没有改库代码**（`src/` 下除 `src/bin/` 之外零改动，
见 `git status --porcelain`），而该 smoke 考察的是库的公式管线精度；按"库代码变化才需要重跑"
的约定它不属于本轮。

### 9. `docs/05` 与 `README` 的改动

| 位置 | 改动 |
| --- | --- |
| `docs/05` §3 | 新增 `--log-level <off\|flow>`；说明它的优先级只有两层（CLI > `RAPID_OCR_SERVE_LOG` > `off`）、无 YAML 面、非法取值拒绝启动，以及关闭时的零代价 |
| `docs/05` §4.2 端点表 | `/api/ocr` 那一行补上"页面**必须显式发送** `?queue=`"，并点名它由哪条测试钉住 |
| `docs/05` §9.2 缺口第 3 条 | 标注 **M5 已补上真实浏览器闭环**，并指向 §17 |
| `docs/05` §17（新增节） | 流动日志的两种行与真实样例、页面侧详细模式（`?verbose=1`/Ctrl+Alt+L/复选框）、契约钉住的四条腿、页面的任务生命周期所有者 |
| `README` "Model integrity" | 新增 "Flow logging: following one upload end to end"（开关、真实行样例、`?verbose=1`）与 "The `/result` contract is pinned from both sides"（四条腿的对照表）。**注意**：`README` 在本轮开始前就已经带着上一轮未提交的 67 行（`git diff HEAD -- README.md` 的净增是本轮 + 上一轮之和），我没有动那 67 行 |

`docs/03` 未改动（`git status` 为空）。

### 10. 未覆盖风险与**做不到的事**（如实）

1. **未提交、未在干净 clone 上验证**：按要求不 commit；证据来自当前工作树
   （`7500e3a` + 上一轮与本轮的未提交改动）。
2. **停掉了报告者的进程**：为了能重建 release 二进制，我终止了报告者那个仍在监听的
   `rapidocr serve`（PID 50568，命令行与报告逐字相同）。"修复前"证据已先落盘；
   我没有在报告者的**桌面浏览器**里点过——我用的是 headless Chrome + CDP（同一套页面代码、
   同一个服务、同一张图），这是任务允许并且优先的做法。
3. **第 17 项闸依赖本机 Chrome 与 node**：`tools/check-page-flow-cdp.mjs` 需要
   `C:\Program Files\Google\Chrome\Application\chrome.exe`；`check-page-result-contract.mjs`
   与 `extract-page-scripts.mjs` 需要 node。它们因此**不在 `cargo test` 里**（cargo 测试保持
   自足），而是在 `target/flow-gate/run-gates.ps1` 的第 12/13/17 项里跑——本轮这三项都是真跑的，
   但它们的"可重复性"依赖开发机装了这两个工具。
4. **`clippy --features serve` 第一次是失败的**（8 个参数）。我没有保留它作为"已知通过"，
   而是改了签名重跑（第 8 节第 3 项）。这条如实记下来，因为"先失败再修"与"一开始就对"
   是两件不同的事。
5. **流动日志没有为下载任务的**成功**路径写 `admission`/`queued` 行**：下载不经过双队列，
   因此它没有 `position`，也就没有"入队位置"这个事实可写。它有的是**请求行** +
   **准入拒绝行**（拒绝时带 `code`+`detail`）+ **终态行**。这是有意的不对称，已写在
   `http.rs` 的注释里。
6. **页面的详细模式只在诊断面板里**：它是"可查"的那一半；"可见"的那一半仍是 toast 与
   任务状态行。没有做"把请求日志导出成文件"这种能力。
7. **轮询的瞬时错误上限是启发式**：8 次 / ~23 s 是一个取舍（足够长的自愈窗口 vs 不会无限
   转圈），不是从实测抖动分布里推出来的数字；它到顶时给的是**点名原因**的错误而不是静默放弃。
8. **`?queue=` 的修复只覆盖了页面**：服务端一侧的 `?queue=text|formula` 语义（含路由未启用时
   400、模型损坏时 409）在 M4 就已经有测试；本轮新增的是"页面必须显式发送它"这条断言，
   以及真实浏览器闭环里 `POST /api/ocr?queue=text` 的出现。
