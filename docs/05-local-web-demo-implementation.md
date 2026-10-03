# 本地 Web 评估界面（`rapidocr serve`）实施文档

> 状态：**M0 待冻结，未进入实现**
> 已定决策：**仅本机（loopback）** · **模型下载默认关闭、显式触发** · **实现为 `rapidocr serve` 子命令**
> 关联文档：`docs/03-windows-only-optimization-tasks.md`、`docs/04-windows-phase-reports.md`
> 参考规范：MDN（Fetch / XMLHttpRequest.upload / AbortController）、`tiny_http::Server`、OWASP SSRF Prevention、WCAG 2.2

---

## 0. 目标与非目标

### 0.1 目标

让任何人**不写代码、不装 Rust 工具链**（用预编译 zip）就能在本机评估 OCR 效果：

1. 左侧上传/拖入/粘贴图片，右侧显示识别结果；
2. 结果可交互：区域高亮 ↔ 文本、置信度、坐标；
3. 暴露诊断信息（逐阶段耗时、provider、ORT 指纹、峰值内存），让用户判断"慢/错"的原因；
4. 模型缺失时明确提示缺哪些文件、多大、来源，并允许用户**点击下载**；
5. 图片与模型**永不离开本机**。

### 0.2 非目标

| 非目标 | 原因 |
| --- | --- |
| **远程 / 局域网访问** | **永久非目标**：安全模型只对 loopback 成立；不提供任何可绑定其他地址的参数，也不提供远程模式 |
| 公网托管站点 | 库在非 Windows 目标上 `compile_error!`（Linux 服务器无法构建），且服务器无 DirectML/CUDA |
| 多用户 / 账号 / cookie 会话 | 单用户本地工具；**不引入 cookie 认证** |
| 模型后台自动下载 | 读请求不应产生 600+ MB 网络与磁盘副作用 |
| 把 HTTP/UI 依赖引入库 | 库边界仍是 `ImageInput` → `OcrOutput`（§2.1） |
| 前端工程化 | 单文件内联 HTML/CSS/JS，零构建 |
| 运行中中断推理（M1） | ONNX Runtime `Run` 在当前 API 下不可中断；取消语义见 §4.3 |

---

## 1. 现状盘点

### 1.1 必须复用（已存在）

| 能力 | 位置 | 复用方式 |
| --- | --- | --- |
| 检测框叠加 | `output::visualize::draw_output(img: &RecImage, output: &OcrOutput) -> RgbImage` | `annotated.png` |
| 哈希计算 | `model_store::sha256_file` | 状态判定与下载校验 |
| 默认模型目录 | `model_store::default_model_store_dir()` | 默认 `--model-dir` |
| 模型来源表 | `assets/default_models.yaml` | 构造 `ModelSet`（§5） |
| 结果序列化 | `to_output_json` / `to_output_items` / `plain_text(TextOrder)` | API 响应 |
| 报告渲染 | `render_output_report`（**注意：当前内联 `<style>` 与 `<script>`**，见 §9.5） | 导出（需新增静态模式） |
| 诊断数据 | `OcrOutput.timings` / `.stages`、`ort_runtime_fingerprint()`、`runtime::memory` | 诊断面板，**不重新测量** |
| 输入限制 | 编码字节上限、header 像素探测、24 Mpx、`max_side_len` | 请求体与像素上限 |

### 1.2 实现前必须先补齐（M0 范围，全部在库里）

| 缺口 | 现状（已核对代码） | 处置 |
| --- | --- | --- |
| 无模型集抽象 | `ModelRegistry` 只有 `resolve_det/rec/cls` | 新增 `ModelSet`/`ModelFileSpec`/`ModelRole`（§5） |
| **双权威来源** | `ModelManifest`（detector/recognizer/dictionary/classifier 固定字段）与 `default_models.yaml` 并存 | 改为**通用 manifest + 单一来源选择规则**（§5.3） |
| 清单无法表达公式/tokenizer | `ModelManifest` 只有四类固定字段 | 同上，改为 `files: Vec<ManifestFile>` + `role` |
| 逐文件状态不可得 | `validate_files()` **遇到第一个错误即返回** | 抽出共享的逐文件校验函数（§5.2） |
| 字典无哈希 | `ResolvedRecModel { model_url, sha256: Option<String>, dict_url: Option<String> }` | 字典补 SHA-256，且作为 `ModelFileSpec` |
| 哈希可选 | `ensure_downloaded(.., expected_sha256: Option<&str>, ..)` | 模型/字典文件**不允许** `None` |
| 下载跟随重定向 | 默认 `reqwest::blocking::Client` | 禁用重定向（§6） |
| 无体积上限 / 无长度预检 | 直接流式写文件 | 预检 + `take(max+1)`（§6） |
| `.part` 固定名 | `target_path.with_extension("part")` | 唯一临时名 + 原子替换（§6） |
| 无并发下载锁 | 无 | 同文件单飞（§6） |
| 无 connect/read 超时区分 | 只有整体 `timeout(60s)` | 分项超时（§6） |
| **Windows 原子替换不明** | 代码中无 `MoveFileExW` 用法 | 用 `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`（§6.7） |
| provider 可能静默回退 | `fail_if_provider_unavailable` 默认 **`false`** | serve 侧默认强制失败（§7.5） |

---

## 2. 架构与边界

### 2.1 分层与依赖方向

```
rapidocr (bin)                        ← 唯一引入 HTTP 依赖的地方
  └── serve 子命令（feature = "serve"，非 default）
        ├── http 层：路由 / 准入顺序 / ServeError → 状态码 / 安全头
        ├── 静态页：include_str!("web/index.html")，启动时注入 nonce 与 token
        ├── job 层：双队列 + 固定 worker + 有界结果存储 + tombstone + TTL 清理
        ├── download 层：独立 worker + 单飞 + 加固下载器
        └── rapid-ocr-rs（库，零 HTTP 依赖）
              ModelSet / model_store（加固） / ImageInput → OcrRequest → OcrOutput
```

**硬性约束**：库不新增网络/UI 依赖；`tiny_http` 只能 optional 且**不进 default**；`serve` 不改变库既有默认行为；前端不加载任何外部资源。

### 2.2 依赖选择

| 需求 | 选择 | 理由 |
| --- | --- | --- |
| HTTP | `tiny_http`（blocking，optional） | OCR 本身阻塞；不引入 async runtime |
| 上传体 | 原始 body（`application/octet-stream`） | 避免 multipart 解析依赖 |
| 原子替换 | `MoveFileExW`（raw Win32，与 `memory.rs` 同风格） | Windows 下 `fs::rename` 覆盖语义不可靠（§6.7） |
| 静态页 | `include_str!` + 注入 nonce/token | 无运行时文件依赖 |
| 打开浏览器 | `cmd /C start <url>` | 不新增依赖 |
| 磁盘空间 | `GetDiskFreeSpaceExW` | 与 Windows-only 定位一致 |
| 并发 | `std::sync::{mpsc, Mutex, Condvar}` + `AtomicU64` | 不引入 async runtime |

---

## 3. 命令行接口

```
rapidocr serve [OPTIONS]

  --port <PORT>              默认 8760；占用时报可定位错误，不静默换端口
                             （**没有 --host**：监听地址硬编码 127.0.0.1，见 §7.1）
  --model-dir <DIR>          默认 model_store::default_model_store_dir()
  --config <FILE>            EngineConfig YAML（与 run/evaluate 一致）
  --provider <PROV>          cpu | directml | cuda（默认 cpu）；非 cpu 时默认强制不回退（§7.5）
  --allow-provider-fallback  允许 provider 不可用时回退 CPU（默认不允许）
  --max-side <N>             覆盖 max_side_len
  --allow-download           允许下载模型（仍需 token，§7）
  --allow-download-host <H>  追加一个允许下载的 host（可重复；默认仅编译期白名单，§6.1）
  --open                     启动后打开系统默认浏览器

  # 资源上限
  --max-body-mb <N>          请求体上限，默认 32
  --max-result-mb <N>        单个结果序列化上限，默认 8（§4.6）
  --max-export-mb <N>        单个导出文档上限（含内嵌图片），默认 32（§9.5）
  --max-download-mb <N>      单文件下载上限，默认 1024（必须 > 566 MB 公式模型，§6.2）
  --max-queue-text <N>       普通 OCR 队列上限，默认 4
  --max-queue-formula <N>    公式 OCR 队列上限，默认 2
  --max-consecutive-text <N>    普通任务连续处理上限，默认 4（保公式不被饿死，§8.3）
  --max-consecutive-formula <N> 公式任务连续处理上限，默认 1（保普通 OCR 不被饿死，§8.3）
  --max-retained <N>         终态任务保留数上限，默认 32
  --max-retained-mb <N>      终态任务占用字节上限，默认 64
  --max-tombstones <N>       已淘汰任务 ID 记录上限，默认 256（§4.5）
  --job-ttl-secs <N>         终态任务与 tombstone 保留时长，默认 600
```

**参数优先级（必须一致并记录）**：**CLI flag > `--config` YAML > 内建默认**。
`serve` 会用 CLI 值覆盖配置中的 `max_side_len` 与 `provider_preference`，并在启动日志中打印"哪个值生效、被覆盖的值是什么"。未启用 `serve` feature 时子命令仍可解析，但返回可定位错误（提示 `cargo build --features serve`）。

---

## 4. 协议：统一的异步任务生命周期

### 4.1 设计原则

OCR（尤其公式路径）单图可达数秒至数十秒，**不得长期占用 HTTP 请求**。所有识别都是异步任务，不存在"同步返回结果"的第二套语义。

### 4.2 端点

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/` | 内联单页（注入 nonce + token） |
| `GET` | `/api/status` | 引擎/provider/ORT 指纹/队列与内存概况（路径脱敏） |
| `GET` | `/api/models` | 模型集状态（§5.4） |
| `POST` | `/api/models/download` | 启动下载任务（需 `--allow-download` **且** token）；请求体 `{"set_id": "<id>"}`，未知 id → **404 `model_set_not_found`**（绝不回落 `sets[0]`） |
| `POST` | `/api/ocr` | 提交识别 → **202** `{job_id, queue, position, state:"queued"}` |
| `GET` | `/api/jobs/{id}` | `{id, kind, queue, state, position, queued_ms, started_ms, elapsed_ms, error}` + M2 追加的 `{failure, download, cancel_requested}`（见 §4.3） |
| `GET` | `/api/jobs/{id}/result` | 结果（未完成 409 `job_not_finished`；已淘汰 410 `job_evicted`） |
| `GET` | `/api/jobs/{id}/annotated.png` | 叠加检测框 PNG（原图淘汰 → 410 `original_evicted`） |
| `GET` | `/api/jobs/{id}/export?format=json\|md\|html` | 导出（HTML 走静态模式 + 独立 CSP，§9.5） |
| `POST` | `/api/jobs/{id}/cancel` | 取消（§4.3） |
| `POST` | `/api/engine/reload` | 显式创建/重建引擎（无请求体；响应 `{outcome, engine, missing, corrupt, source, model_dir, load_ms}`，§7.6） |

### 4.3 状态机与取消语义

```
queued ──► running ──► succeeded
   │          │     └► failed
   └──────────┴────► cancelled
```

- `queued` → 立即 `cancelled`（可靠）；
- `running` → **409 `not_cancellable`**（M1 不中断推理；不得假装取消成功）；
- 终态 → 409 `job_finished`。
- 前端必须区分"**取消上传**"（XHR abort）与"**取消推理**"（job cancel），按钮与文案分开。

**M2 的补充（下载任务，不改变上面三条对 OCR 任务的语义）**：

- `running` 的**下载**任务：`POST /cancel` → **200**，响应里 `state` 仍是 `running`、
  `cancel_requested: true`；worker 在**文件边界**（§6.6）兑现取消，随后 `state` 才是
  `cancelled`（已通过校验的文件保留，临时文件不残留）。**进行中的那个文件不会被打断**
  （阻塞式 HTTP 读取没有安全的中断语义），这一点必须如实呈现，不得假装立即停止；
- `failure`：失败任务的结构化分类 `{status, code, message, detail}`（`detail` 与
  `/result` 上重放的错误体同源），`error` 仍是同一份人类可读文本；
- `download`（仅 `kind = "model_download"` 非 `null`）：
  `{files_done, files_total, bytes_done, bytes_total, current_file}`；`files_total`/`bytes_total`
  只统计**需要下载**的文件（缺失 ∪ 损坏），`current_file` 在失败时指向出错的那个文件。


### 4.4 准入顺序（**防止在拒绝请求前读入大 body**）

按顺序执行，任一步失败立即返回并**不读取 body**：

1. 方法与路径匹配 → 否则 405/404；
2. token 校验 → 401；
3. `Host` 校验 → 421；`Origin` 校验（仅 `POST/PUT/DELETE`）→ 403；
4. **队列容量预检** → 满则 **503**（此时**尚未读取请求体**，直接回 `Request` 让连接关闭）；
5. `Content-Length` 预检（> `--max-body-mb` → **413**）；
6. 有界流式读取：总字节上限 + **读取超时**（chunked 请求同样受这两条约束）；
7. 仅在以上全部通过后才解码与建任务。

### 4.5 结果存储、淘汰与 tombstone

| 项 | 默认 | 行为 |
| --- | --- | --- |
| 普通队列 / 公式队列 | 4 / 2 | 满 → 立即 503（不阻塞） |
| 终态任务保留 | 32 个 / 64 MB | 超限按"最旧终态优先"淘汰 |
| 任务 TTL | 600 s | 后台清理线程，不依赖访问触发 |
| **tombstone** | 256 个 / 同 TTL | 淘汰时把 `job_id → evicted_at` 写入有界 FIFO tombstone 表；**这是 410 能成立的前提** |
| 查询语义 | — | 活跃 → 正常；**在 tombstone 内 → 410 `job_evicted`**；两者都无 → 404 `job_not_found` |
| 原图保留 | 仅编码字节，计入字节预算 | `annotated.png` **按需**从编码字节重新解码，**不长期保留 `RecImage`** |
| 淘汰后访问 | — | 404 与 410 必须可区分，且有测试覆盖 tombstone 的容量与 TTL 淘汰 |

### 4.6 结果序列化上限

结果存储的字节上限**不等于**响应安全：公式区域多时，JSON/Markdown/HTML 序列化会再产生一份大内存。

- 新增 `--max-result-mb`（默认 8）；
- 序列化使用**有界写入器**（累计字节超限即中止），超限 → **413 `result_too_large`**；
- 该上限对 `/result` 与 `/export` **同时**生效；
- 不得先序列化成巨大 `String` 再判断长度。

---

## 5. `ModelSet`：模型清单的唯一抽象（M0 前置）

### 5.1 结构（库内新增并导出）

```rust
pub enum ModelRole {
    Detector, Classifier, Recognizer, Dictionary, Tokenizer,
    FormulaDetector, FormulaRecognizer,
}

pub struct ModelFileSpec {
    pub name: String,            // 相对文件名；禁止路径分隔符与 ..
    pub role: ModelRole,
    pub size_bytes: Option<u64>, // 用于下载前空间核算；未知则跳过预算检查但仍受限流上限
    pub sha256: String,          // **必填**
    pub source_url: String,      // https，且 host 在允许列表内
}

pub struct ModelSet {
    pub id: String, pub family: String, pub version: String,
    pub files: Vec<ModelFileSpec>,
}
```

### 5.2 逐文件校验（**唯一实现**）

```rust
pub enum ModelFileState { Missing, Present, Corrupt { expected: String, actual: String } }

/// 共享实现：一次返回**每个**文件的状态，不在第一个错误处提前返回。
pub fn validate_model_files(files: &[ModelFileSpec], root: &Path) -> Vec<(ModelFileSpec, ModelFileState)>;

pub struct ModelSetStatus {
    pub set_id: String,
    pub files: Vec<(ModelFileSpec, ModelFileState)>,
    pub complete: bool,                    // 全部 Present 且每个文件都有哈希
    pub download_bytes_total: Option<u64>, // 缺失文件大小之和；有未知大小则为 None
}
```

规则：

- `Missing` / `Corrupt`（存在但哈希不匹配 → 前端提示"损坏，建议重新下载"）；
- **无哈希的文件不参与 `complete` 判定**，且该集合不得报 `complete = true`；
- CLI 侧的"完整性检查"改为调用同一函数后判断 `all(Present)`，**不再保留第二套逐文件逻辑**；
- 路径安全沿用现有约束：拒绝绝对路径与 `..`（当前 `ModelManifest::validate_files` 已有的规则必须保留在这个共享函数里）。

### 5.3 单一权威来源（**取消"可选覆盖"语义**）

当前 `ModelManifest` 与 `default_models.yaml` 是两套权威，文档上一版写的"manifest 可选覆盖"仍然是双来源。**从根上统一**：

1. **`ModelManifest` 改为通用结构**（破坏性修改，符合开发期规则）：

```rust
pub struct ModelManifest {
    pub schema_version: u32,          // 显式版本，未知版本直接报错
    pub id: String, pub family: String, pub version: String,
    pub languages: Vec<String>,
    pub files: Vec<ManifestFile>,     // 覆盖所有 ModelRole，含公式与 tokenizer
}
pub struct ManifestFile { pub name: String, pub role: ModelRole, pub sha256: String, pub size_bytes: Option<u64>, pub source_url: Option<String> }
```

2. **运行时来源选择规则（无合并、无优先级冲突）**：
   - 若 `<model-dir>/manifest.json` 存在 → **它是该目录的唯一来源**，`default_models.yaml` **完全不参与**；
   - 否则 → `default_models.yaml` 是唯一来源；
   - 若所选来源**缺少当前管线所需的 role**（例如启用了公式但 manifest 没有 `FormulaRecognizer`）→ **报错并列出缺失 role**，不静默降级；
   - 旧格式清单（固定四字段）→ 视为 `schema_version` 不支持，给出可定位错误与迁移提示。
3. `default_models.yaml` 仍是"可下载来源表"的载体（URL + SHA256），`ModelSet` 由它或 manifest 构造；**HTTP 层永不直接解析 YAML**。

### 5.4 `GET /api/models` 响应

```json
{
  "model_dir": "<redacted>",
  "source": "default_table | local_manifest",
  "downloads_allowed": false,
  "sets": [
    { "id": "PP-OCRv6", "complete": false, "download_bytes_total": 42106880,
      "files": [ { "name": "PP-OCRv6_det_medium.onnx", "role": "detector",
                   "state": "missing", "size_bytes": 4712345, "sha256": "…", "source_url": "https://…" } ] }
  ]
}
```

---

## 6. 加固下载器（M0 前置，改在库里）

```rust
pub struct DownloadRequest<'a> {
    pub url: &'a str,
    pub expected_sha256: &'a str,   // 必填
    pub save_dir: &'a Path,
    pub max_bytes: u64,             // 来自 --max-download-mb（§6.2）
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub allowed_hosts: &'a [&'a str],
}
pub fn download_verified(req: &DownloadRequest<'_>) -> Result<PathBuf>;
```

### 6.1 硬性要求

1. **仅 HTTPS**；否则 `SchemeRejected`；
2. **禁止自动重定向**（`redirect(Policy::none())`），收到 3xx → `RedirectRejected`（未来若要支持必须逐跳校验 host/path）；
3. **host 允许列表必须来自可信配置，不得来自模型清单**：默认是**编译期固定**白名单（当前为 `www.modelscope.cn`）；本地 `manifest.json` 可以提供 URL，但**不能扩大**该白名单，越界即 `HostRejected`；确需其他来源时必须显式传 `--allow-download-host <HOST>` 并打印高风险警告（OWASP：allowlist 必须来自可信配置，而不是资源描述自身）；
4. **`Content-Length` 预检**：超过 `max_bytes` 在写入前拒绝；
5. **流式上限**：无长度时 `take(max_bytes + 1)`，超限失败并删除临时文件；
6. **唯一临时文件名**（`.part-<pid>-<seq>`）；
7. **原子替换**（§6.7）；
8. **同文件单飞**：进程内按目标文件名加锁，重复请求复用同一任务；
9. **哈希失败删除临时文件**；
10. **磁盘空间预检**（`GetDiskFreeSpaceExW`）→ `InsufficientSpace`（507）；
11. **错误分类**：`Network` / `ConnectTimeout` / `ReadTimeout` / `TooLarge` / `RedirectRejected` / `SchemeRejected` / `HostRejected` / `InsufficientSpace` / `HashMismatch` / `Cancelled`。

### 6.2 `max_bytes` 的来源

- 新增 CLI `--max-download-mb`（**默认 1024**，必须大于 566 MB 公式模型；启动时校验该默认值不为 0）；
- 对**每个文件**生效，并作为**整个下载任务的总量上限**（多文件集合按剩余额度递减）；
- `size_bytes` 为 `None` 时**仍然**受该固定流式上限保护，只是跳过"提前预检"这一步；
- 若 `size_bytes` 已知且 `> max_bytes` → 在发请求前就拒绝，并说明两者数值。

### 6.3 目标已存在且损坏

下载前若目标文件存在但哈希不匹配 → 允许覆盖（这是"用户点重新下载"的正常路径），但必须：

- 先确认临时文件校验通过，**再**执行替换；
- 替换失败时保留原文件并报错（不得留下"既没有旧文件也没有新文件"的状态）。

### 6.4 迁移

现有 CLI 下载路径迁移到 `download_verified`；**删除"可传 `None` 哈希"的入口**，不保留兼容分支。

### 6.5 磁盘空间核算

需求 = 缺失文件大小之和（`download_bytes_total`，未知项按 `max_bytes` 计入）；可用空间不足 → 507 并回报所需/可用字节。

### 6.6 取消

下载任务的取消只在**文件边界**生效（当前文件下载完成后停止后续文件）；取消后删除临时文件，已完成并校验通过的文件保留。

### 6.7 Windows 原子替换（必须明确实现）

`fs::rename` 在 Windows 上对"目标已存在"的覆盖语义不可靠，因此：

- 使用 **`MoveFileExW(src, dst, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`**（raw Win32 绑定，与 `runtime/memory.rs` 现有风格一致，不新增依赖）；
- 若该调用失败：保留原目标文件，报可定位错误（含 Win32 错误码）；
- **不采用**"先删除再 rename"作为主路径（那会引入崩溃窗口）；只有明确记录为降级路径并在错误信息中说明时才允许；
- 回归测试：**目标已存在且哈希错误时，重新下载后文件被正确替换且哈希通过**。

---

## 7. 安全模型

### 7.1 仅本机：监听地址**硬编码**（无参数可改）

**本项目仅支持本地**：监听地址硬编码为 `127.0.0.1`，**不提供任何**可绑定其他地址的参数、配置项或环境变量；**也不提供"远程模式"**。

上一版的 `--host` 与 Host/Origin 校验自相矛盾（绑定 `192.168.x.x` 后，正常浏览器请求会被自己的校验拒掉），因此整条路径删除，**而不是**加一个警告了事。

- **只监听 IPv4**：绑定 `127.0.0.1`，**不监听 IPv6**；因此 Host/Origin 允许集合 = `{127.0.0.1, localhost}` 与**实际绑定端口**，**不含 `[::1]`**（没有监听就不放行）；
- 启动时**断言**实际监听地址属于 loopback 集合，否则立即退出并报可定位错误（防御将来被误改）；
- 启动日志打印实际监听地址与允许的 Host/Origin 集合，便于排查；
- 该限制是**永久设计**，不是本轮临时收敛。

### 7.2 校验与令牌

| 措施 | 要求 |
| --- | --- |
| `Host` 校验 | 不在允许集合 → **421 `bad_host`**（防 DNS rebinding） |
| `Origin` 校验 | **所有** `POST/PUT/DELETE` 必须带 `Origin` 且等于当前服务 origin；缺失、`null`、不匹配 → **403 `bad_origin`** |
| 令牌 | 启动时随机生成，注入页面下发；客户端用 `X-RapidOCR-Token`；**所有 `/api/*` 都需要**（`GET /` 除外） |
| CORS | **不发送任何** `Access-Control-Allow-Origin`；绝不 `*` |
| Cookie | **不使用** |
| 下载 | 同时要求 `--allow-download` **与** token；请求体**不得**携带 URL（防 SSRF） |
| 路径 | 一律经 `ModelSet` / 共享校验函数 / `model_store`，禁止用请求内容拼路径 |

### 7.3 响应头

全部响应：`X-Content-Type-Options: nosniff`、`Referrer-Policy: no-referrer`、`Cache-Control: no-store`。

### 7.4 Provider 与模型的路径脱敏

`/api/status` 不返回完整本机绝对路径：模型目录只给状态或脱敏形式；ORT 指纹可保留文件名 + 体积 + SHA-256。

### 7.5 Provider 回退语义（**必须冻结**）

现状：`RuntimeConfig::fail_if_provider_unavailable` 默认 **`false`**（`config.rs:195`），因此"启动时固定 provider"并不等于"真的用了该 provider"。

规则：

- `serve --provider directml|cuda` → **默认强制 `fail_if_provider_unavailable = true`**；
- 需要回退时显式加 `--allow-provider-fallback`（此时才沿用 `false`）；
- **配置错误在启动期失败**：provider 名称非法、或对应 feature 未编译进来（如 `--provider directml` 但没启用 `directml-provider`）→ 立即退出并给可定位错误，**不进入服务**；
- **provider 可用性在引擎创建期判定**：EP 是否真的可用只能通过建立会话得知，因此在 `Loading → Ready|Failed` 转换处暴露（§7.6），**不会**在首次 OCR 时才悄悄失败；
- `/api/status` **始终**同时给出 `requested` / `selected_ep` / `fallback_to_cpu`；
- 前端展示 provider 时必须显示实测耗时，**不得**用"已选择 DirectML"暗示性能结论（已知：DirectML 对公式模型反而慢约 2.3×）。

### 7.6 服务状态与引擎状态（解决启动期建引擎与模型缺失仍可访问的矛盾）

服务可用性**不依赖模型**，只有引擎依赖模型与 provider：

```rust
pub enum ServiceState { Starting, Ready }          // 监听成功即为 Ready

pub enum EngineState {
    BlockedModelsMissing { missing: Vec<String> }, // 模型缺失，尚未创建
    Loading,                                       // 正在创建会话
    Ready { requested: String, selected_ep: String, fallback_to_cpu: bool },
    Failed { reason: String },                     // 创建失败（含 provider 不可用）
    Rebuilding,                                    // 运行期切换 provider（M3）
}
```

启动顺序（**取代上一版启动期创建 engine 的表述**）：

1. 绑定硬编码的 `127.0.0.1`（§7.1）→ 失败即退出；
2. **校验运行配置**：provider 名称、对应 feature 是否编译进来、各资源上限取值合法 → 任一非法立即退出；
3. 检查模型集状态（§5）：
   - **不齐备** → 服务**正常启动**，`EngineState::BlockedModelsMissing`；`/api/models`、`/api/status` 可用；`POST /api/ocr` 返回 **409 `models_missing`**（字段与 `/api/models` 一致）；
   - **齐备** → **预加载**：`Loading` → `Ready`（provider 不可用则 `Failed`，原因写入 `/api/status`）。

运行期转换：

- 模型下载完成（M2）后**不自动**重建引擎；在下一次 `POST /api/ocr` 或显式 `POST /api/engine/reload` 时创建（避免后台突然占用数百 MB）；
- `POST /api/engine/reload`：`Ready → Loading → Ready|Failed`；`Loading` 期间新 OCR 请求排队（不拒绝）；`Failed` 时 OCR 返回 **503 `engine_unavailable`** 并附 `reason`；
- 引擎 `Failed` 必须让 `/api/status` 明确显示原因，**不得**退化成模型不可用这种模糊状态。

---

## 8. 线程、队列与准入

### 8.1 线程模型

```
主线程：accept loop（tiny_http::Server::recv_timeout(200ms) + 关闭标志；退出用 Server::unblock()）
   ├── 静态/校验类请求：就地处理（廉价）
   ├── 普通 OCR ──► 有界队列 A（--max-queue-text）
   ├── 公式 OCR ──► 有界队列 B（--max-queue-formula）
   │                    └── OCR worker（M1 固定 1 个；引擎 &mut self）
   └── 下载请求 ──► 独立有界 channel
                        └── 独立下载 worker（不得阻塞 OCR）
```

### 8.2 worker 数

- M1 **固定 1 个 OCR worker**（引擎 `&mut self`，多 worker 需要引擎池，属后续工作）；因此 **M1 不暴露 `--ocr-workers`**——不提供设了不生效的选项，等引擎池落地后再引入；
- 队列满 → **立即 503**，不阻塞等待；
- 推理**绝不在 accept 线程**执行。

### 8.3 公平调度（对应"分队列"的可验收定义）

上一版只限制连续公式任务数，普通任务持续到达时**公式队列会被永久饿死**。改为**双向配额**：

- 两个**独立容量**（`--max-queue-text` / `--max-queue-formula`）；公式洪水无法占用普通队列槽位；
- 调度按轮进行：每轮先取最多 `--max-consecutive-text`（默认 **4**）个普通任务，再取最多 `--max-consecutive-formula`（默认 **1**）个公式任务；
- **保底规则**：只要公式队列非空，每轮**至少**执行 1 个公式任务（即使普通队列一直非空）；同理只要普通队列非空，每轮**至少**执行 1 个普通任务；
- 某一队列为空时，另一队列可自由连续处理（不浪费空闲额度）；
- 验收测试**两个方向都要**：
  1. 持续灌入公式任务 → 普通 OCR 等待时间有上界；
  2. 持续灌入普通任务 → 公式任务等待时间有上界（**新增**，上一版会失败）。

### 8.4 结果存储

TTL + 数量 + 字节三重上限 + tombstone（§4.5），**不允许**无界 `HashMap`。

---

## 9. 前端（单文件内联）

> **原型已选定（2026-10-03）**：Temp/demo3-v2.html（1658 行）作为 include_str! 内联页的起点。
> 它由 Temp/demo3.html 派生，包含三处移植与一处统一：
> 1. 移植 demo2.html 的缩放组（zoomOut/zoomLbl/zoomIn/zoomFit + 1:1）与剪贴板粘贴按钮；
> 2. 移植 demo1.html 的三字段 EP 展示（pRequested/pSelected/pFallback，§7.5 要求三者始终同时给出，未知态不得伪装成 alse）；
> 3. 占位符统一为 __CSP_NONCE__（×4）/ __SRV_TOKEN__（×3），服务端只做这两处替换，替换后必须重新扫描并在残留时**失败退出**。
>
> 已知缺陷（**不要**直接把 demo3.html 当作内联页）：demo3.html 的 IIFE 提前闭合
> （(function(){ 1 处、})(); 2 处），整段脚本 
ode --check 报 Unexpected token '}'，
> 页面 JS 完全不执行。demo3-v2.html 已修正为 1:1。

### 9.1 上传与进度

- 上传使用 **`XMLHttpRequest`**：`xhr.upload.onprogress` 显示进度，`xhr.abort()` 取消上传（`fetch` 没有稳定的上传进度事件）；
- 提交后轮询 `GET /api/jobs/{id}`（M1 轮询；SSE 留待后续评估）；
- "取消上传"与"取消识别"分开（§4.3）。

### 9.2 布局

| 区域 | 内容 |
| --- | --- |
| 左 | 拖放区 + 文件选择按钮 + `<img>` + 覆盖层 `<canvas>`；粘贴；缩放/适应；"下载标注图" |
| 右上 | 引擎状态（`loading`/`ready`/`rebuilding`/`failed`）、`requested`/`selected_ep`/`fallback_to_cpu`、ORT 版本、峰值内存、进度 |
| 右中 | 区域列表（阅读顺序、文本、置信度、坐标）；点击 ↔ 高亮；"复制全文" |
| 右下 | 导出 JSON/Markdown/HTML；折叠的诊断面板（逐阶段耗时、账本口径说明、provider 实测耗时） |
| 横幅 | 模型缺失/损坏：文件、总大小、来源、是否允许下载、下载按钮与进度 |

### 9.3 Canvas 与大图内存

- `URL.createObjectURL(file)` 预览，替换或离开页面时 **`revokeObjectURL()`**；
- Canvas 像素尺寸按 `devicePixelRatio`；
- polygon 以**原图坐标**返回，前端按显示矩形换算，不做几何推断；
- 不复制多份大图；标注图按需生成；结果存储不长期保留解码后的原图。

### 9.4 可访问性（WCAG 2.2）

拖放必须同时提供文件选择按钮；列表项用真实 `<button>` 且可聚焦、有 `:focus-visible`；状态变化用 `aria-live="polite"`；选中/公式/普通文本**不只靠颜色**区分；`Esc` 关闭高亮/提示；固定横幅不得遮挡聚焦元素。

### 9.5 CSP 与 HTML 导出（**解决冲突**）

现状：`output/html.rs` 内联输出 `<style>`（约 106 行）与 `<script>`（约 149 行）。若直接返回给我们自己的 origin，会被 nonce CSP 拦掉。

方案（两者都要做）：

1. **给 `render_output_report` 增加渲染模式**：`ReportMode::{Full, Static}`。
   - `Full`：保持现状（内联 script/style），供 CLI `--output-dir` 使用；
   - `Static`：**不含任何 `<script>`**，样式内联保留；Web 导出使用它。
2. **Web 导出以附件形式返回**，并带**独立的导出 CSP**：

```
Content-Disposition: attachment; filename="ocr-<id>.html"
Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; img-src data:; script-src 'none'; sandbox
```

   即：导出文档**不允许脚本**；`style-src 'unsafe-inline'` 只在这条导出响应上出现，主页面的 CSP 仍是 nonce。
3. 主页面 CSP（`GET /`）：

```
default-src 'none'; script-src 'nonce-<n>'; style-src 'nonce-<n>';
img-src 'self' blob: data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'
```

4. **M3 自动化测试**：`export?format=html` 的响应含 `Content-Disposition: attachment`、含上述导出 CSP、且**正文不含 `<script`**；主页面的 CSP 含 nonce 且**不含 `unsafe-inline`**。
5. **导出的 HTML 必须是真正可离线使用的单文件**：`render_report` 的 `image_href` 是**相对路径**（注释即写明依赖与 HTML 同目录故可离线，这只在 CLI 场景成立），而导出 CSP 又是 `img-src data:`——因此当前设计**并不成立**。
   - **决策：Web 导出把图片以 `data:image/png;base64,…` 内嵌**（`image_href` 传 data URL），导出后脱离服务仍可查看；
   - 新增 `--max-export-mb`（默认 **32**）：3200×2000 的标注 PNG 可能远超 `--max-result-mb`（默认 8），二者是**不同预算**；
   - 超过上限 → **413 `export_too_large`**，错误信息指向可直接下载的 `annotated.png`（**不得**返回一个图片链接失效的 HTML）；
   - **不选** HTML 加图片打 zip：本 crate **当前没有 zip 依赖**，为此新增依赖不划算；**也不选**只提供在线查看：那就失去导出的意义。

### 9.6 CSP 兼容约束

不使用内联 `onclick=`；事件在 nonce 脚本中绑定；不引用外部 CDN；页面由服务端注入 nonce 与 token（每次启动随机）。

---

## 10. 性能与资源规则

1. 普通/公式**分队列 + 公平调度**（§8.3）；
2. 引擎**启动时**创建（provider 失败要早暴露），首次加载显示独立 `loading` 与耗时；
3. 不在 HTTP 线程执行推理；
4. 结果缓存 TTL + 数量 + 字节三上限（§4.5），序列化另有上限（§4.6）；
5. 下载与推理分离线程；
6. 诊断数据复用 `timings`/`stages`/ORT 指纹/`memory`，不重新测量；
7. 不把 provider 名称当性能结论；
8. 公式路由**默认关闭**（避免意外加载 566 MB 模型）；
9. `/api/status` 路径脱敏（§7.4）。

---

## 11. 里程碑

### M0：冻结协议与安全（先决条件）

- [ ] 删除 `--host`；监听地址**硬编码** `127.0.0.1`（无参数/配置/环境变量可改）；启动断言监听地址属 loopback；Host/Origin 允许集合含实际端口（§7.1）
- [ ] tombstone 表（容量 + TTL）+ 404/410 区分（§4.5）
- [ ] `ModelSet`/`ModelFileSpec`/`ModelRole`/`ModelSetStatus` + **共享逐文件校验函数**（§5.1/5.2）
- [ ] `ModelManifest` 通用化（`schema_version` + `files: Vec<ManifestFile>`）+ **单一来源选择规则**（§5.3）
- [ ] 字典补 SHA-256；无哈希不得 `complete`（§1.2、§5.2）
- [ ] 加固下载器 + `--max-download-mb` + `MoveFileExW` 原子替换 + 迁移 CLI 调用方（§6）
- [ ] provider 回退语义 + **启动期配置校验** + 引擎创建期可用性判定 + `/api/status` 三字段（§7.5）
- [ ] `ServiceState` / `EngineState` 状态机与全部转换（§7.6）
- [ ] `ServeError` 与状态码映射（§11.1，含 `engine_unavailable` / `export_too_large`）
- [ ] 准入顺序（§4.4）与 `--max-result-mb`（§4.6）
- [ ] 双队列容量与**双向**公平调度参数（`--max-consecutive-text` / `--max-consecutive-formula`，§8.3）

**M0 验收**：以上每项都有单元测试；`cargo test` 全绿；文档与实现一致。

#### 11.1 `ServeError`（独立类型，禁止字符串匹配）

```rust
pub enum ServeError {
    BadRequest, PayloadTooLarge, ResultTooLarge, BadHost, BadOrigin, Unauthorized,
    Busy, JobNotFound, JobEvicted, JobNotFinished, NotCancellable,
    ModelsMissing, ModelsCorrupt, DownloadsDisabled, InsufficientDiskSpace,
    UnsupportedInput, Download(DownloadError), Ocr(RapidOcrError), Internal,
}
```

| 情况 | 状态码 | `code` |
| --- | --- | --- |
| 缺/错 token | 401 | `unauthorized` |
| 缺/错 `Origin` | 403 | `bad_origin` |
| 缺/错 `Host` | 421 | `bad_host` |
| 请求体超限 | 413 | `payload_too_large` |
| 结果序列化超限 | 413 | `result_too_large` |
| 队列满 | 503 | `busy` |
| 模型缺失 / 损坏 | 409 | `models_missing` / `models_corrupt` |
| 未开 `--allow-download` | 403 | `downloads_disabled` |
| 磁盘不足 | 507 | `insufficient_disk_space` |
| 图片无法解码/超限 | 422 | `unsupported_input` |
| 未完成 / 已淘汰 / 不可取消 | 409 / **410** / 409 | `job_not_finished` / `job_evicted` / `not_cancellable` |
| 下载失败 / 超时 | 502 / 504 | `download_failed` / `download_timeout` |
| 其他 | 500 | `internal` |

### M1：最小闭环

- [ ] `serve` 子命令 + feature 隔离 + 未启用 feature 的可定位错误
- [ ] provider 启动期解析（§7.5）；`GET /`、`/api/status`、`POST /api/ocr`、`/api/jobs/{id}`、`/result`
- [ ] 单图上传（XHR + 进度 + 取消上传）、异步任务与轮询
- [ ] 预览与 polygon 叠框、区域列表、复制全文、JSON 导出
- [ ] 模型缺失提示（消费 `ModelSetStatus`；**不含下载动作**）
- [ ] 测试：状态机、tombstone（容量+TTL）、双队列 503、公平调度、准入顺序、Host/Origin/token、安全头与 nonce CSP

**M1 验收**：真实 12 图经 HTTP 的 `regions` 数量与文本与 `rapidocr run --json` **逐张一致**；空 `--model-dir` 下 `/api/models` 与 `/api/ocr` 的缺失字段一致；公式洪水下普通 OCR 不被饿死、普通洪水下公式也不被饿死；空模型目录下服务仍为 `Ready` 且 `EngineState::BlockedModelsMissing`（OCR 409，字段与 `/api/models` 一致）；CLI 中不存在任何可改变监听地址的选项，也不存在 `--ocr-workers`。

### M2：模型管理

- [ ] `GET /api/models`、`POST /api/models/download`、下载任务进度
- [ ] 单飞、空间检查、强制 SHA-256、失败清理、`MoveFileExW` 原子替换、重定向拒绝
- [ ] 下载取消（文件边界）与"目标已损坏时重新下载"（§6.3）
- [ ] 模型齐备后惰性创建 engine 并显示耗时

### M3：诊断与导出

- [ ] 时间账本、ORT/provider 指纹、内存信息进入诊断面板（含口径说明）
- [ ] `annotated.png`、Markdown/HTML 导出（HTML 走 `ReportMode::Static` + 独立 CSP，§9.5）
- [ ] provider 运行期切换：暂停新任务 → 排空 → 销毁旧 engine → 创建新 engine → `rebuilding`；失败恢复旧 engine 或明确 `failed`
- [ ] 测试：导出 HTML 可用且不含 `<script>`、CSP 头正确

### M4：公式与评估

- [ ] 公式模型下载（566 MB，显式点击 + 体积提示）
- [ ] 公式 OCR 走独立队列（§8.3）
- [ ] 公式区域展示与诊断
- [ ] 上传标注样本 → CER / 精确匹配（复用 `evaluation`，不另写指标）

---

## 12. 验证计划

| 类别 | 内容 |
| --- | --- |
| 静态检查与 **feature 矩阵** | `cargo fmt --all -- --check`；`cargo test --all-targets`（默认 feature，**不覆盖 serve 代码**）；**`cargo test --features serve --all-targets`**；**`cargo clippy --features serve --all-targets -- -D warnings`**；`cargo clippy --all-targets -- -D warnings` |
| 依赖隔离 | 默认构建不含 `tiny_http`（`cargo tree -e normal --no-default-features` + `cargo package --list` 对比） |
| 协议 | 202→queued→running→succeeded；未完成 `/result` 409；**tombstone 命中 410 且 TTL/容量淘汰后回 404** |
| 准入参数边界 | `--max-body-mb=0`；超大整数；字节换算**溢出**；`Content-Length` 与实际不符 |
| 请求体 | `Content-Type` 非 `application/octet-stream`；**chunked 超限**；读取超时 |
| 队列 | 双队列各自 503；**公式洪水不饿死普通 OCR**；`--max-consecutive-formula` 生效 |
| 结果 | `--max-result-mb` 超限 → 413 `result_too_large`（序列化即中止，不先建大 String） |
| 安全 | 缺 token→401；错 `Origin`→403；错 `Host`→421；安全头齐备；主页面 CSP 含 nonce 且无 `unsafe-inline` |
| 仅本机 | 监听地址硬编码 `127.0.0.1`：对 CLI 参数做**枚举断言**（不存在任何地址类选项）；启动断言监听地址属 loopback；绑定非 loopback 的路径在代码中不存在 |
| IPv6 | 服务不监听 IPv6（`[::1]` 不被接受为 Host/Origin，返回 421/403）；只监听 `127.0.0.1` |
| 引擎状态 | 空模型目录 → 服务 `Ready` + `EngineState::BlockedModelsMissing`，OCR 409 且字段与 `/api/models` 一致；模型齐备 → 预加载 `Ready`；`POST /api/engine/reload` → `Loading`→`Ready`/`Failed`；`Failed` 时 OCR 503 `engine_unavailable` 且带 `reason` |
| 公平性（双向） | 公式洪水下普通 OCR 等待有上界；**普通洪水下公式任务等待有上界** |
| 导出可用性 | 导出 HTML **不含任何外部引用**（无相对 `src`、无 `/api` 链接），图片为 `data:`；超 `--max-export-mb` → 413 `export_too_large` |
| 下载白名单来源 | manifest 声明白名单外的 host → `HostRejected`；加 `--allow-download-host` 后才放行并打印警告 |
| 下载 | 重定向拒绝、非 https 拒绝、host 白名单拒绝、`Content-Length` 超限拒绝、无长度流式超限拒绝、哈希失败删临时文件、单飞只下一份、磁盘不足 507、**目标已损坏时原子替换成功** |
| Provider | 请求 directml/cuda 但实际回退 → 默认启动即失败；加 `--allow-provider-fallback` 时 `/api/status` 三字段如实反映 |
| 模型集 | 字典缺哈希 → `complete=false`；损坏 → `Corrupt`；manifest 缺 role → 报错列出缺失 role；旧 schema manifest → 可定位错误 |
| 导出 | HTML 导出含 `Content-Disposition: attachment`、导出 CSP、正文无 `<script>` |
| 参数优先级 | CLI `--config` / `--provider` / `--max-side` 三者优先级与启动日志一致 |
| 真实资产 | 12 图 HTTP 与 CLI 逐张一致；公式路径单独验证 |
| 手工 | 浏览器闭环、粘贴、上传进度、标注图、键盘与焦点可用 |
| 性能 | 诊断面板数据与 CLI 报告同值（不重新测量） |

**证据要求**：每个里程碑在 `docs/06-local-web-demo-reports.md`（新建）记录命令、关键输出、与验收标准对照、未覆盖风险。**不得**以"界面看起来正常"作为验证通过。

---

## 13. 参考命令（实施后填实测值）

```powershell
cargo run --features serve -- serve --port 8760 --open

# 主页面 CSP 与 nonce
curl.exe -s -D - -o NUL http://127.0.0.1:8760/

# 模型状态（含 source 字段）
curl.exe -s -i http://127.0.0.1:8760/api/models -H "X-RapidOCR-Token: <token>"

# 提交任务（202 + job_id）
# 必须显式声明 Content-Type：curl 的 --data-binary 会把类型设成
# application/x-www-form-urlencoded，而准入顺序里的媒体类型白名单只接受
# application/octet-stream（§4.4、src/bin/serve/admit.rs）。
curl.exe -s -X POST --data-binary "@D:\100_Projects\110_Daily\SnapClip\OCR-test-image\01基础多位置文本.png" `
  "http://127.0.0.1:8760/api/ocr?max_side=2000" `
  -H "Content-Type: application/octet-stream" `
  -H "X-RapidOCR-Token: <token>" -H "Origin: http://127.0.0.1:8760"

# 轮询 / 取结果 / 导出
curl.exe -s http://127.0.0.1:8760/api/jobs/<id> -H "X-RapidOCR-Token: <token>"
curl.exe -s http://127.0.0.1:8760/api/jobs/<id>/result -H "X-RapidOCR-Token: <token>" -o target\serve-ocr.json
curl.exe -s -D - "http://127.0.0.1:8760/api/jobs/<id>/export?format=html" -H "X-RapidOCR-Token: <token>" -o target\serve-report.html

# 模型下载（M2；需要 --allow-download，请求体**只有** set_id，body 不得携带 URL）
# set_id 必须来自 GET /api/models 的 sets[].id：未知 id 是 404 model_set_not_found，
# 不会被猜成"第一个集合"。
curl.exe -s -X POST "http://127.0.0.1:8760/api/models/download" `
  -H "Content-Type: application/json" `
  -H "X-RapidOCR-Token: <token>" -H "Origin: http://127.0.0.1:8760" `
  -d '{"set_id":"PP-OCRv6-small-ch"}'
# 进度与取消：同一个 job 生命周期（download{files_done,files_total,bytes_done,bytes_total,current_file}）
curl.exe -s http://127.0.0.1:8760/api/jobs/<download-job-id> -H "X-RapidOCR-Token: <token>"
curl.exe -s -X POST http://127.0.0.1:8760/api/jobs/<download-job-id>/cancel `
  -H "X-RapidOCR-Token: <token>" -H "Origin: http://127.0.0.1:8760"
# 运行中的下载在**文件边界**生效：响应里 state 仍是 running + cancel_requested=true，
# 轮询到 cancelled 才是真的停下（§6.6）。

# 显式创建/重建引擎（M2；无请求体）。响应里的 engine 与 /api/status 的同一字段同形：
# {"outcome":"ready|blocked_models_missing|failed","engine":{…},"missing":[…],"load_ms":…}
curl.exe -s -X POST http://127.0.0.1:8760/api/engine/reload `
  -H "X-RapidOCR-Token: <token>" -H "Origin: http://127.0.0.1:8760"

# 与 CLI 对照（区域数应逐张一致）
.\target\release\rapidocr.exe run --img-path "D:\100_Projects\110_Daily\SnapClip\OCR-test-image\01基础多位置文本.png" --config "D:\100_Projects\110_Daily\SnapClip\OCR-Model\test-config-small.yaml" --json
```

---

## 14. 风险与对策

| 风险 | 影响 | 对策 |
| --- | --- | --- |
| 契约反复变更 | 破坏性重写 | **M0 先冻结**并配套测试 |
| 恶意网页调用本机服务 | 耗尽算力/触发下载 | Host + Origin + token；下载还需显式开关（§7） |
| 拒绝前读入大 body | 内存/带宽被消耗 | 准入顺序：**先查队列与长度，再读**（§4.4） |
| tombstone 无界增长 | 内存泄漏 | 容量 + TTL 双上限（§4.5） |
| 结果序列化爆内存 | OOM | `--max-result-mb` + 有界写入器（§4.6） |
| Windows 覆盖语义 | 文件损坏/丢失 | `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` + 回归测试（§6.7） |
| provider 静默回退 | 用户误判性能 | 默认强制失败 + 三字段展示（§7.5） |
| 双权威模型来源 | 行为不一致 | 单一来源规则 + 缺 role 报错（§5.3） |
| 公式任务饿死普通 OCR | 体验退化 | 双队列 + 连续上限（§8.3） |
| `serve` 依赖泄漏进默认构建 | 影响发布 | feature 隔离 + `cargo tree`/`package` 验证 |
| HTTP 逻辑渗入库 | 破坏库边界 | §2.1 硬性约束 + review checklist |

---

## 15. 待确认问题（M0 冻结前）

1. 默认端口 `8760` 是否可接受？
2. 默认模型集：只提供 `PP-OCRv6`，还是同时提供 `PP-OCRv4/v5` 切换？
3. `--max-download-mb` 默认 1024 MB 是否合适（公式模型 566 MB + 普通模型约 40 MB + 余量）？
4. 结果上限默认 8 MB / 保留 32 个 64 MB 是否合适？
5. 队列默认（text 4 / formula 2）与配额（text 连续 4 / formula 连续 1）是否合适？
6. `--max-export-mb` 默认 32 MB 是否够（3200×2000 标注 PNG base64 后约为原文件的 1.33 倍）？

---

## 16. 参考资料

- MDN：[Using Fetch](https://developer.mozilla.org/en-US/docs/Web/API/Fetch_API/Using_Fetch) · [XMLHttpRequest.upload](https://developer.mozilla.org/en-US/docs/Web/API/XMLHttpRequest/upload) · [AbortController](https://developer.mozilla.org/en-US/docs/Web/API/AbortController)
- [`tiny_http::Server`](https://docs.rs/tiny_http/latest/tiny_http/struct.Server.html)（`recv_timeout`、`unblock`）
- OWASP：[SSRF Prevention Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Server_Side_Request_Forgery_Prevention_Cheat_Sheet.html)
- WCAG 2.2：[Dragging Movements](https://www.w3.org/WAI/WCAG22/Understanding/dragging-movements) · [Focus Visible](https://www.w3.org/WAI/WCAG22/Understanding/focus-visible) · [Focus Not Obscured](https://www.w3.org/WAI/WCAG22/Understanding/focus-not-obscured-enhanced.html)
