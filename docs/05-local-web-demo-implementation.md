# 本地 Web 评估界面（`rapidocr serve`）实施文档

> 状态：**M0 待冻结，未进入实现**
> 已定决策：**仅本地** · **模型下载默认关闭、由用户显式触发** · **实现为 `rapidocr serve` 子命令**
> 关联文档：`docs/03-windows-only-optimization-tasks.md`、`docs/04-windows-phase-reports.md`
> 参考规范：MDN（Fetch / XMLHttpRequest.upload / AbortController）、`tiny_http::Server`、OWASP SSRF Prevention、WCAG 2.2（Dragging Movements / Focus Visible / Focus Not Obscured）

---

## 0. 目标与非目标

### 0.1 目标

让任何人**不写代码、不装 Rust 工具链**（用预编译 zip）就能在本机评估 OCR 效果：

1. 左侧上传/拖入/粘贴图片，右侧显示识别结果；
2. 结果可交互：区域高亮 ↔ 文本、置信度、坐标；
3. 暴露**诊断信息**（逐阶段耗时、provider、ORT 指纹、峰值内存），让用户能判断"慢/错"的原因；
4. 模型缺失时明确提示缺哪些文件、多大、来源，并允许用户**点击下载**；
5. 图片与模型**永不离开本机**。

### 0.2 非目标

| 非目标 | 原因 |
| --- | --- |
| 公网托管站点 | 库在非 Windows 目标上 `compile_error!`（Linux 服务器无法构建），且服务器无 DirectML/CUDA |
| 多用户 / 账号 / cookie 会话 | 单用户本地工具；**不引入 cookie 认证** |
| 模型后台自动下载 | 读请求不应产生 600+ MB 网络与磁盘副作用（既定决策） |
| 把 HTTP/UI 依赖引入库 | 库边界仍是 `ImageInput` → `OcrOutput`（§2.1） |
| 前端工程化（npm/打包器） | 单文件内联 HTML/CSS/JS，零构建 |
| 运行中中断推理（M1） | ONNX Runtime `Run` 在当前 API 下不可中断；取消语义见 §4.3 |

---

## 1. 现状盘点

### 1.1 必须复用（已存在）

| 能力 | 位置 | 复用方式 |
| --- | --- | --- |
| 检测框叠加 | `output::visualize::draw_output(img: &RecImage, output: &OcrOutput) -> RgbImage` | `GET /api/jobs/{id}/annotated.png` |
| 哈希计算 | `model_store::sha256_file` | 状态判定与下载校验 |
| 默认模型目录 | `model_store::default_model_store_dir()` | 默认 `--model-dir` |
| 模型来源表 | `assets/default_models.yaml`（`model_dir` + `SHA256` + `dict_url`） | 构造 `ModelSet`（§5） |
| 清单校验 | `ModelManifest::validate_files(root)`：**校验 SHA-256** 且**拒绝绝对路径与 `..` 逃逸** | 完整性判定与路径安全边界 |
| 结果序列化 | `to_output_json` / `to_output_items` / `render_output_report` / `plain_text(TextOrder)` | API 响应与导出 |
| 诊断数据 | `OcrOutput.timings` / `.stages`、`ort_runtime_fingerprint()`、`runtime::memory` | 诊断面板，**不重新测量** |
| 输入限制 | 编码字节上限、header 像素探测、24 Mpx、`max_side_len` | 请求体与像素上限 |

### 1.2 实现前必须先补齐（**M0 范围，全部在库里**）

以下问题已核对代码确认，**不能靠 HTTP 层绕过**：

| 缺口 | 现状（已核对） | 处置 |
| --- | --- | --- |
| 无"模型集"抽象 | `ModelRegistry` 只有 `resolve_det/rec/cls`，HTTP 层若直接用就必须自己拼 det/cls/rec/dict 文件名 | 新增 `ModelSet`/`ModelFileSpec`/`ModelRole`（§5） |
| 字典无哈希 | `ResolvedRecModel { model_url, sha256: Option<String>, dict_url: Option<String> }` —— **字典只有 URL，没有 SHA-256** | `default_models.yaml` 补字典 SHA256；字典作为 `ModelFileSpec` |
| 哈希可选 | `ensure_downloaded(file_url, expected_sha256: Option<&str>, save_dir)` 允许传 `None` | 模型/字典文件**不允许** `None`；无哈希的文件不得报 `complete=true` |
| 下载跟随重定向 | `reqwest::blocking::Client`（默认最多 10 跳重定向） | 禁用重定向（§6） |
| 无响应体上限 / 无 `Content-Length` 预检 | 直接流式写文件 | 预检 + `take(max+1)` 流式上限（§6） |
| `.part` 固定名 | `target_path.with_extension("part")` | 唯一临时名 + 原子 rename（§6） |
| 无并发下载锁 | 无 | 同一文件单飞（§6） |
| 只有整体 timeout | `Client::builder().timeout(60s)`，无 connect/read 区分 | 分别设置 connect/read/整体超时（§6） |
| 清单与默认表未统一 | `ModelManifest`（校验用）与 `default_models.yaml`（来源用）是两套 | 统一到 `ModelSet`，清单作为可选覆盖（§5.3） |

---

## 2. 架构与边界

### 2.1 分层与依赖方向

```
rapidocr (bin)                        ← 唯一引入 HTTP 依赖的地方
  └── serve 子命令（feature = "serve"，非 default）
        ├── http 层：路由 / 请求解析 / ServeError → 状态码 / 安全头
        ├── 静态页：include_str!("web/index.html")，启动时注入 nonce 与 token
        ├── job 层：有界队列 + 固定 worker + 有界结果存储 + TTL 清理
        ├── download 层：独立 worker + 单飞 + 加固后的下载器
        └── rapid-ocr-rs（库，零 HTTP 依赖）
              ModelSet / model_store（加固） / ImageInput → OcrRequest → OcrOutput
```

**硬性约束**

- 库**不得**新增网络/UI 依赖；`tiny_http` 只能是 optional，由 `serve` feature 启用，**不进** default；
- `serve` 不得改变库既有默认行为（下载默认关闭、`allow_download: false` 语义不变）；
- 前端**不加载任何外部资源**（无 CDN），离线可用。

### 2.2 依赖选择

| 需求 | 选择 | 理由 |
| --- | --- | --- |
| HTTP | `tiny_http`（blocking，optional） | OCR 本身阻塞；引入 `tokio`/`axum` 属为异步而异步 |
| 上传体 | **原始 body**（`application/octet-stream`） | 避免 multipart 解析依赖 |
| 静态页 | `include_str!` + 运行时注入 nonce/token | 无运行时文件依赖；CSP 可用 nonce 而非 `unsafe-inline` |
| 打开浏览器 | `cmd /C start <url>` | 不新增依赖，仅 `--open` |
| 磁盘空间 | 复用既有 raw Win32 风格调用 `GetDiskFreeSpaceExW` | 与 Windows-only 定位一致 |
| 并发原语 | `std::sync::{mpsc, Mutex, Condvar}` + `AtomicU64` | 不引入 async runtime |

---

## 3. 命令行接口

```
rapidocr serve [OPTIONS]

  --host <HOST>            默认 127.0.0.1（显式绑定其他地址时打印警告；仍启用 Host/Origin 校验）
  --port <PORT>            默认 8760；占用时报可定位错误，不静默换端口
  --model-dir <DIR>        默认 model_store::default_model_store_dir()
  --config <FILE>          EngineConfig YAML（与 run/evaluate 一致）
  --provider <PROV>        cpu | directml | cuda（默认 cpu；**会话级，启动时固定**）
  --max-side <N>           覆盖 max_side_len
  --allow-download         允许下载模型；未指定时下载接口 403（与 token 同时要求，§7）
  --open                   启动后打开系统默认浏览器
  --max-body-mb <N>        请求体上限，默认 32
  --max-queue <N>          OCR 等待队列上限，默认 4（满则立即 503）
  --max-retained <N>       保留的已完成任务数上限，默认 32
  --max-retained-mb <N>    保留任务占用的字节上限（原图 + 结果），默认 64
  --job-ttl-secs <N>       终态任务保留时长，默认 600
```

约定：与既有子命令一致用 clap derive；**未启用 `serve` feature 时**该子命令仍可解析，但返回可定位错误：
`this binary was built without the 'serve' feature; rebuild with: cargo build --features serve`。

---

## 4. 协议：统一的异步任务生命周期

### 4.1 设计原则

OCR（尤其公式路径）单图可达数秒至数十秒，**不得长期占用 HTTP 请求**。因此**所有识别都是异步任务**，不再存在"同步返回结果"与"job id"两套语义。

### 4.2 端点

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/` | 内联单页（注入 nonce + token） |
| `GET` | `/api/status` | 引擎/provider/ORT 指纹/队列与内存概况（路径脱敏，§10.9） |
| `GET` | `/api/models` | 模型集状态（§5.4） |
| `POST` | `/api/models/download` | 启动下载任务（需 `--allow-download` **且** token） |
| `POST` | `/api/ocr` | 提交识别任务 → **202** `{job_id, position, state:"queued"}` |
| `GET` | `/api/jobs/{id}` | 状态：`{id, kind, state, position, queued_ms, started_ms, elapsed_ms, error}` |
| `GET` | `/api/jobs/{id}/result` | 结果（`state != succeeded` → 409 `job_not_finished`；已淘汰 → 410 `job_evicted`） |
| `GET` | `/api/jobs/{id}/annotated.png` | 叠加检测框的 PNG（原图已淘汰 → 410 `original_evicted`） |
| `GET` | `/api/jobs/{id}/export?format=json\|md\|html` | 导出（`render_output_report`） |
| `POST` | `/api/jobs/{id}/cancel` | 取消（§4.3） |
| `GET` | `/api/jobs/{id}`（下载任务同构） | 模型下载任务复用同一 job 端点，`kind = "model_download"` |

### 4.3 状态机与取消语义

```
queued ──► running ──► succeeded
   │          │     └► failed
   └──────────┴────► cancelled
```

- `POST /api/jobs/{id}/cancel`：
  - `queued` → 立即 `cancelled`（可靠）；
  - `running` → **409 `not_cancellable`**（M1 不支持中断推理；ONNX Runtime `Run` 在当前 API 下不可中断，**不得**假装取消成功）；
  - 终态 → 409 `job_finished`。
- 前端必须区分"**取消上传**"（XHR abort，§9.1）与"**取消推理**"（job cancel），文案与按钮分开。

### 4.4 资源上限与淘汰策略

| 项 | 默认 | 行为 |
| --- | --- | --- |
| 等待队列 | 4 | 满 → **立即 503 `busy`**（含 `retry_after_ms`），**不阻塞等待** |
| 保留的终态任务 | 32 个 / 64 MB | 超限时按"最旧的终态任务优先"淘汰；`succeeded` 与 `failed` 同等对待 |
| 任务 TTL | 600 s | 到期即淘汰（后台清理线程，不依赖访问触发） |
| 单请求体 | 32 MB | 超限 413，**在写盘/解码前拒绝**（`Content-Length` 预检 + 流式上限双保险） |
| 原图保留 | 仅保留**编码字节**，且计入 §4.4 字节预算 | `annotated.png` 需要时**按需**从编码字节重新解码，**不长期保留 `RecImage`** |
| 结果内存 | 计入字节预算 | 淘汰时同时释放原图与结果 |
| 淘汰后访问 | — | 404 `job_not_found`（从未存在）或 410 `job_evicted`（曾存在已淘汰），必须可区分 |

---

## 5. `ModelSet`：模型清单的唯一抽象（M0 前置）

### 5.1 结构（库内新增，导出到公开 API）

```rust
pub enum ModelRole {
    Detector, Classifier, Recognizer, Dictionary, Tokenizer,
    FormulaDetector, FormulaRecognizer,
}

pub struct ModelFileSpec {
    pub name: String,            // 相对文件名，禁止路径分隔符与 ..
    pub role: ModelRole,
    pub size_bytes: Option<u64>, // 用于下载前空间核算；未知则跳过预算检查但保留流式上限
    pub sha256: String,          // **必填**：无哈希的文件不得进入 ModelSet
    pub source_url: String,      // 必须 https，且 host 在允许列表内
}

pub struct ModelSet {
    pub id: String,              // 如 "PP-OCRv6"、"FormulaNet-Plus-M"
    pub family: String,
    pub version: String,
    pub files: Vec<ModelFileSpec>,
}
```

覆盖范围必须同时支持：普通 OCR（det/cls/rec/dict）、公式（detector/recognizer）、tokenizer 与字典，且**不允许** HTTP 层理解 YAML 内部布局。

### 5.2 状态与判定

```rust
pub enum ModelFileState { Missing, Present, Corrupt { expected: String, actual: String } }

pub struct ModelSetStatus {
    pub set_id: String,
    pub files: Vec<(ModelFileSpec, ModelFileState)>,
    pub complete: bool,                       // 全部 Present
    pub download_bytes_total: Option<u64>,    // 缺失文件大小之和（未知则为 None）
}
```

判定规则（**必须复用 `sha256_file` 与 `ModelManifest::validate_files`**，不得自写）：

- `Missing`：文件不存在；
- `Corrupt`：存在但哈希不匹配 → 前端必须提示"损坏，建议重新下载"；
- **无哈希的文件不参与 `complete` 判定**，且该模型集**不得**报告 `complete = true`；
- 有清单时以 `ModelManifest::validate_files(model_dir)` 为准（它已拒绝绝对路径与 `..`）。

### 5.3 与既有清单统一

`ModelManifest`（校验）与 `default_models.yaml`（来源）目前是两套。统一方向：**`ModelSet` 是运行时唯一来源**，`ModelManifest` 作为可选覆盖（存在时其哈希优先，且必须能映射到同一 `name`）；两者冲突时**报错**而不是静默选择。`ModelRegistry::resolve_*` 保留供 CLI 使用，但 Web 层只消费 `ModelSet`。

### 5.4 `GET /api/models` 响应

```json
{
  "model_dir": "<redacted>",
  "downloads_allowed": false,
  "sets": [
    { "id": "PP-OCRv6", "family": "PP-OCRv6", "version": "v6",
      "complete": false,
      "download_bytes_total": 42106880,
      "files": [
        { "name": "PP-OCRv6_det_medium.onnx", "role": "detector", "state": "missing",
          "size_bytes": 4712345, "sha256": "…", "source_url": "https://…" }
      ] }
  ]
}
```

---

## 6. 加固下载器（M0 前置，改在库里）

`ensure_downloaded` 的现有语义不足以保证"下载即可信"，替换为显式策略版本：

```rust
pub struct DownloadRequest<'a> {
    pub url: &'a str,
    pub expected_sha256: &'a str,   // 必填，不再接受 None
    pub save_dir: &'a Path,
    pub max_bytes: u64,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub allowed_hosts: &'a [&'a str],
}
pub fn download_verified(req: &DownloadRequest<'_>) -> Result<PathBuf>;
```

必须满足（逐条对应审核意见）：

1. **仅 HTTPS**；非 https → `SchemeRejected`；
2. **禁止自动重定向**（`redirect(Policy::none())`）；收到 3xx → `RedirectRejected`（若未来要支持，必须**逐跳**校验 host/path 后再放行）；
3. **host 允许列表**（来自 `ModelSet` 声明，如 `www.modelscope.cn`）→ 否则 `HostRejected`；
4. **`Content-Length` 预检**：超过 `max_bytes` 在写入前拒绝；
5. **流式上限**：无长度时用 `take(max_bytes + 1)`，超限即失败并删除临时文件；
6. **唯一临时文件名**（如 `.part-<pid>-<seq>`），避免并发/陈旧残留冲突；
7. **原子提交**：下载并校验通过后 `fs::rename` 到目标路径；
8. **同文件单飞**：进程内按目标文件名加锁，重复请求复用同一任务；
9. **哈希失败删除临时文件**，绝不留下可疑文件；
10. **磁盘空间预检**（`GetDiskFreeSpaceExW`），不足 → `InsufficientSpace`（映射 507）；
11. **错误分类**（供 HTTP 层映射，不做字符串匹配）：`Network` / `ConnectTimeout` / `ReadTimeout` / `TooLarge` / `RedirectRejected` / `SchemeRejected` / `HostRejected` / `InsufficientSpace` / `HashMismatch` / `Cancelled`。

已有调用方（CLI 下载路径）同步迁移到新 API；不允许保留"可传 `None` 哈希"的旧入口。

---

## 7. 本地服务的安全边界

**`127.0.0.1` 不是安全边界**：恶意网页仍可向本机端口发简单 POST，消耗算力、触发下载或占满队列。

| 措施 | 要求 |
| --- | --- |
| 绑定 | 默认 `127.0.0.1`；绑定其他地址时打印警告 |
| `Host` 校验 | 仅接受 `127.0.0.1` / `localhost` / `[::1]`（含端口），否则 **421 `bad_host`**（防 DNS rebinding） |
| `Origin` 校验 | **所有** `POST/PUT/DELETE` 必须带 `Origin`，且等于当前服务实际 origin；缺失、`null` 或不匹配 → **403 `bad_origin`** |
| 令牌 | 启动时生成随机 token，通过注入页面下发；客户端用自定义头 `X-RapidOCR-Token` 携带 |
| 令牌范围 | **所有 `/api/*` 都需要 token**（`GET /` 与静态资源除外） |
| CORS | **不发送任何** `Access-Control-Allow-Origin`（同源专用），绝不 `*` |
| 认证方式 | **不使用 cookie**（避免被跨站自动携带） |
| 下载接口 | 同时要求 `--allow-download` **与** token；请求体**不得**携带 URL（防 SSRF） |
| 路径 | 一律经 `ModelSet` / `ModelManifest::validate_files` / `model_store`，禁止用请求内容拼路径 |
| 响应头 | 全部响应加 `X-Content-Type-Options: nosniff`、`Referrer-Policy: no-referrer`、`Cache-Control: no-store` |
| CSP | `default-src 'none'; script-src 'nonce-<n>'; style-src 'nonce-<n>'; img-src 'self' blob: data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'` —— 用 **nonce** 放行内联脚本，**禁止** `unsafe-inline` |
| 日志 | 只输出到 stdout/stderr；不落盘用户图片；临时产物放 `%TEMP%` 并随请求清理 |

---

## 8. 线程与并发模型

```
主线程：accept loop（tiny_http::Server::recv_timeout(200ms) + 关闭标志；退出用 Server::unblock()）
   ├── 静态/校验类请求：就地处理（廉价）
   ├── OCR 请求 ──► 有界 channel(容量 = --max-queue)
   │                    └── 固定 1 个 OCR worker（引擎 &mut self ⇒ 单 worker 是合理默认）
   │                           └── 写回有界结果存储
   └── 下载请求 ──► 独立有界 channel
                        └── 独立 1 个下载 worker（模型下载不得阻塞 OCR）
```

规则：

- **不要每个请求一个线程**；
- 队列满 → **立即 503**，不无限等待；
- OCR worker 与 download worker 分离（§10.5）；
- 结果存储有 **TTL + 数量 + 字节**三重上限（§4.4），**不允许**无界 `HashMap`；
- 推理**绝不在 accept 线程**执行；
- 引擎惰性初始化，首次加载要有独立 `loading` 状态与实测耗时。

---

## 9. 前端（单文件内联）

### 9.1 上传与进度

- 上传使用 **`XMLHttpRequest`**：`xhr.upload.onprogress` 显示上传进度，`xhr.abort()` 取消上传（MDN：`fetch` 没有稳定的上传进度事件）；
- 提交后轮询 `GET /api/jobs/{id}`（M1 用轮询；SSE 留待后续评估）；
- "取消上传"与"取消识别"是两个独立操作与两套文案（§4.3）。

### 9.2 布局

| 区域 | 内容 |
| --- | --- |
| 左 | 拖放区 + 文件选择按钮 + `<img>` + 覆盖层 `<canvas>`；粘贴（Ctrl+V）；缩放/适应窗口；"下载标注图" |
| 右上 | 状态条：引擎状态（`loading`/`ready`/`rebuilding`/`failed`）、provider、ORT 版本、峰值内存、上传/识别进度 |
| 右中 | 区域列表：阅读顺序序号、文本、置信度、坐标；点击 ↔ 图中框高亮；"复制全文" |
| 右下 | 导出 JSON / Markdown / HTML；折叠的**诊断面板**（逐阶段耗时、时间账本口径说明、provider 实测耗时） |
| 横幅 | 模型缺失/损坏提示：缺失文件、总大小、来源、是否允许下载、下载按钮与进度 |

### 9.3 Canvas 与大图内存

- 预览用 `URL.createObjectURL(file)`，**替换图片或离开页面时 `URL.revokeObjectURL()`**；
- Canvas 实际像素尺寸按 `devicePixelRatio` 设置（`canvas.width = clientWidth * dpr`）；
- polygon 始终以**原图坐标**返回，由前端按显示矩形换算；**不在前端做几何推断**；
- 不复制多份原始大图；标注图**按需**生成（`annotated.png`），结果存储不长期保留原图解码结果。

### 9.4 可访问性（WCAG 2.2）

- 拖放**必须**同时提供文件选择按钮（Dragging Movements 的替代操作）；
- 区域列表项使用真实 `<button>` 或可聚焦元素，支持键盘操作与 `:focus-visible`；
- 状态变化用 `aria-live="polite"` 播报；
- 选中/公式/普通文本**不仅靠颜色**区分（边框 + 文本标签）；
- `Esc` 关闭高亮/提示；
- 固定横幅**不得遮挡**聚焦元素（Focus Not Obscured）。

### 9.5 CSP 兼容约束

- 不使用内联 `onclick=` 等属性处理器，事件一律在 nonce 脚本中绑定；
- 不引用外部 CDN；
- 页面由服务端注入 nonce 与 token，二者都是每次启动随机。

---

## 10. 性能与资源规则

1. **普通 OCR 与公式 OCR 分队列**（或至少分别限流），避免公式任务阻塞普通任务；
2. 引擎惰性初始化，首次加载显示独立 `loading` 与耗时；
3. 不在 HTTP 线程执行推理；
4. 结果缓存 **TTL + 数量 + 字节**三上限；
5. 下载与推理**分离线程**；
6. 诊断数据直接复用 `timings` / `stages` / ORT 指纹 / `memory`，**不重新测量**；
7. **不把 provider 名称当性能结论**：必须展示实测耗时，并标注已知实测事实（DirectML 在普通 OCR 约 2× 加速，但**在公式模型上反而慢约 2.3×**；CUDA 在本机未验证）；
8. 公式路由**默认关闭**，避免用户意外加载约 566 MB 模型；
9. `/api/status` **不返回完整本机绝对路径**：改为模型目录状态或脱敏形式（ORT 指纹可保留哈希与体积，路径只给文件名）。

---

## 11. 里程碑

### M0：冻结协议与安全（**先决条件，本文件通过后立即执行**）

- [ ] 确定并写入代码：异步 job 生命周期与状态机（§4）
- [ ] 确定 TTL / 数量 / 字节上限与淘汰策略（§4.4）
- [ ] 新增 `ModelSet` / `ModelFileSpec` / `ModelRole` / `ModelSetStatus` 并导出（§5）
- [ ] `default_models.yaml` 为**字典**补齐 SHA-256；无哈希文件不得报 `complete`（§1.2、§5.2）
- [ ] 统一 `ModelManifest` 与 `ModelSet` 的冲突处理（§5.3）
- [ ] 加固下载器并迁移既有调用方（§6）
- [ ] Host + Origin + token 三项校验与安全头/CSP（§7）
- [ ] 定义 `ServeError` 枚举与状态码映射（§4.2、下 §11.1）
- [ ] 明确取消语义（queued 可取消、running 返回 409）（§4.3）

**M0 验收**：以上每一项都有对应单元测试；`cargo test` 全绿；文档与实现一致。

#### 11.1 `ServeError`（独立类型，禁止字符串匹配）

```rust
pub enum ServeError {
    BadRequest, PayloadTooLarge, BadHost, BadOrigin, Unauthorized, Forbidden,
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
| 队列满 | 503 | `busy` |
| 模型缺失 / 损坏 | 409 | `models_missing` / `models_corrupt` |
| 未开 `--allow-download` | 403 | `downloads_disabled` |
| 磁盘不足 | 507 | `insufficient_disk_space` |
| 图片无法解码/超限 | 422 | `unsupported_input` |
| 任务未完成 / 已淘汰 / 不可取消 | 409 / 410 / 409 | `job_not_finished` / `job_evicted` / `not_cancellable` |
| 下载失败 | 502 / 504 | `download_failed` / `download_timeout` |
| 其他 | 500 | `internal` |

### M1：最小闭环

- [ ] `serve` 子命令 + feature 隔离 + 未启用 feature 的可定位错误
- [ ] **provider 启动时固定**（不做运行期切换）
- [ ] `GET /`、`GET /api/status`、`POST /api/ocr`、`GET /api/jobs/{id}`、`/result`
- [ ] 单图上传（XHR + 上传进度 + 取消上传）、异步任务与状态轮询
- [ ] 图片预览与 polygon 叠框、区域列表、复制全文、JSON 导出
- [ ] 模型缺失提示（消费 `ModelSetStatus`，不含下载动作）
- [ ] 单元测试：状态机、队列上限 503、TTL/淘汰、Origin/Host/token、响应头与 CSP

**M1 验收**：真实 12 图经 HTTP 的 `regions` 数量与文本与 `rapidocr run --json` **逐张一致**；`--model-dir` 指向空目录时界面正确显示缺失文件与大小。

### M2：模型管理

- [ ] `GET /api/models`（`ModelSetStatus`）、`POST /api/models/download`、下载任务进度
- [ ] 单飞、磁盘空间检查、强制 SHA-256、失败清理、原子 rename、重定向拒绝
- [ ] 下载取消与失败恢复语义
- [ ] 模型齐备后**惰性**创建 engine，并显示创建耗时

### M3：诊断与导出

- [ ] 时间账本、ORT/provider 指纹、内存信息进入诊断面板（含口径说明文案）
- [ ] `annotated.png`、Markdown/HTML 导出
- [ ] provider 运行期切换：暂停新任务 → 等当前任务结束 → 销毁旧 engine → 创建新 engine → `rebuilding` 状态；失败则恢复旧 engine 或明确 `failed`（**不做即时下拉框**）

### M4：公式与评估

- [ ] 公式模型下载（566 MB，显式点击 + 体积提示）
- [ ] 公式 OCR **独立队列**
- [ ] 公式区域展示与诊断
- [ ] 上传标注样本 → CER / 精确匹配（复用 `evaluation` 模块，不另写指标实现）
- [ ] 评估报告导出

---

## 12. 验证计划

| 类别 | 内容 | 方式 |
| --- | --- | --- |
| 静态检查 | fmt / clippy `-D warnings` | `cargo fmt --all -- --check`；`cargo clippy --all-targets --all-features -- -D warnings` |
| 依赖隔离 | 默认构建不含 `tiny_http` | `cargo tree -e normal --no-default-features`；对比 `cargo package --list` |
| 协议 | 202→queued→running→succeeded；`result` 在未完成时 409；淘汰后 410 | 单元/集成测试 |
| 安全 | 缺 token→401；错 `Origin`→403；错 `Host`→421；响应含三项安全头与 nonce CSP | 集成测试直连 socket |
| 下载 | 重定向拒绝、非 https 拒绝、host 不在白名单拒绝、`Content-Length` 超限拒绝、无长度时流式超限拒绝、哈希失败删除临时文件、单飞只下一份、磁盘不足 507 | 用**本地 HTTP 服务器**提供 fixture；真实 ModelScope 来源标注为未验证 |
| 资源 | 队列满 503、TTL 清理、数量/字节淘汰、原图淘汰后 `annotated.png` 返回 410 | 集成测试 |
| 取消 | queued 可取消；running 返回 409 | 集成测试 |
| 模型集 | 字典缺哈希时 `complete=false`；损坏文件报 `Corrupt` | 单元测试 + 临时目录 |
| 真实资产 | 12 图 HTTP 与 CLI 逐张一致；公式路径单独验证 | §13 命令 |
| 手工 | 浏览器闭环、粘贴、上传进度、标注图下载、键盘与焦点可用 | 人工 + 截图存档 |
| 性能 | 诊断面板数据与 CLI 报告同值（不重新测量） | 同图对比 |

**证据要求**：每个里程碑结束在 `docs/06-local-web-demo-reports.md`（新建）记录：命令、关键输出、与验收标准对照、未覆盖风险。**不得**以"界面看起来正常"作为验证通过。

---

## 13. 参考命令（实施后填实测值）

```powershell
# 构建并启动（开发）
cargo run --features serve -- serve --port 8760 --open

# 模型状态（状态码与字段都要看）
curl.exe -s -i http://127.0.0.1:8760/api/models -H "X-RapidOCR-Token: <token>"

# 提交识别任务（202 + job_id）
curl.exe -s -X POST --data-binary "@D:\100_Projects\110_Daily\SnapClip\OCR-test-image\01基础多位置文本.png" `
  "http://127.0.0.1:8760/api/ocr?max_side=2000" `
  -H "X-RapidOCR-Token: <token>" -H "Origin: http://127.0.0.1:8760"

# 轮询与取结果
curl.exe -s http://127.0.0.1:8760/api/jobs/<id> -H "X-RapidOCR-Token: <token>"
curl.exe -s http://127.0.0.1:8760/api/jobs/<id>/result -H "X-RapidOCR-Token: <token>" -o target\serve-ocr.json

# 与 CLI 对照（区域数应逐张一致）
.\target\release\rapidocr.exe run --img-path "D:\100_Projects\110_Daily\SnapClip\OCR-test-image\01基础多位置文本.png" --config "D:\100_Projects\110_Daily\SnapClip\OCR-Model\test-config-small.yaml" --json
```

---

## 14. 风险与对策

| 风险 | 影响 | 对策 |
| --- | --- | --- |
| 任务状态/队列语义反复 | 破坏性重写 | **M0 先冻结**（§11 M0），先把契约和测试立起来 |
| 恶意网页调用本机服务 | 耗尽算力/触发下载 | Host + Origin + token 三重校验（§7），下载还需显式开关 |
| 下载过程被中间人/重定向利用 | 供应链风险 | 禁重定向、仅 HTTPS、host 白名单、强制 SHA-256（§6） |
| 公式单图数十秒 | 界面"卡住"、队列堆积 | 有界队列 + 503 + 进度可见；公式独立队列（§10.1）；取消语义诚实（§4.3） |
| 大图多开导致内存膨胀 | OOM | 三层上限 + TTL 淘汰 + 不保留 `RecImage`（§4.4、§9.3） |
| provider 切换期间资源翻倍 | 内存峰值 | 暂停→排空→销毁→创建的显式序列，失败回滚（M3） |
| `serve` 依赖泄漏进默认构建 | 影响 crates.io 依赖图 | feature 隔离 + `cargo tree`/`cargo package` 验证（§12） |
| HTTP 逻辑渗入库 | 破坏库边界 | §2.1 硬性约束 + review checklist |

---

## 15. 待确认问题（M0 冻结前）

1. 默认端口 `8760` 是否可接受？
2. 默认模型集：只提供 `PP-OCRv6`，还是同时提供 `PP-OCRv4/v5` 的切换入口？
3. 未完成任务的保留策略是否需要"用户手动置顶不淘汰"（建议不需要）？
4. 结果存储上限默认 32 个 / 64 MB 是否合适（公式结果 JSON 可能较大）？
5. 是否需要在 M1 就提供 `--no-token`（仅调试用，默认不提供；建议**不提供**）？

---

## 16. 参考资料

- MDN：[Using Fetch](https://developer.mozilla.org/en-US/docs/Web/API/Fetch_API/Using_Fetch) · [XMLHttpRequest.upload](https://developer.mozilla.org/en-US/docs/Web/API/XMLHttpRequest/upload) · [AbortController](https://developer.mozilla.org/en-US/docs/Web/API/AbortController)
- [`tiny_http::Server`](https://docs.rs/tiny_http/latest/tiny_http/struct.Server.html)（`recv_timeout`、`unblock`）
- OWASP：[SSRF Prevention Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Server_Side_Request_Forgery_Prevention_Cheat_Sheet.html)
- WCAG 2.2：[Dragging Movements](https://www.w3.org/WAI/WCAG22/Understanding/dragging-movements) · [Focus Visible](https://www.w3.org/WAI/WCAG22/Understanding/focus-visible) · [Focus Not Obscured](https://www.w3.org/WAI/WCAG22/Understanding/focus-not-obscured-enhanced.html)
