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

- 加固下载器（§6）：`DownloadRequest` / `download_verified`、禁止重定向、host 白名单、
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
| 2 | 禁止自动重定向 | `ClientBuilder::redirect(Policy::none())`；3xx → `RedirectRejected{location}` | `a_redirect_is_rejected_and_nothing_is_written`（302 + Location，目录为空，请求数 1） |
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
| 3xx → `RedirectRejected`（带 Location） | ✅ `a_redirect_is_rejected_and_nothing_is_written` |
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
| 重定向 | 默认 client **自动跟随**（可被跳到任意 host） | `Policy::none()`；3xx → `RedirectRejected` | §6.1 第 2 条 |
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
