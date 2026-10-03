# 本地 Web 评估界面（`rapidocr serve`）实施文档

> 状态：待实施
> 决策已定：**仅本地** · **模型下载默认关闭、由用户显式触发** · **实现为 `rapidocr serve` 子命令**
> 关联文档：`docs/03-windows-only-optimization-tasks.md`（平台与性能边界）、`docs/04-windows-phase-reports.md`（实测证据）

---

## 0. 目标与非目标

### 0.1 目标

让任何人**不写代码、不装 Rust 工具链**（用预编译 zip）就能在本机评估 OCR 效果：

1. 左侧上传/拖入图片（或粘贴剪贴板图片），右侧显示识别结果；
2. 结果可交互：区域高亮 ↔ 文本、置信度、坐标；
3. 暴露**诊断信息**（逐阶段耗时、provider、ORT 版本、峰值内存），让用户能判断"慢/错"的原因；
4. 模型缺失时**明确提示**缺哪些文件、多大、从哪里下载，并允许用户**点击下载**；
5. 图片与模型**永不离开本机**。

### 0.2 非目标（明确不做）

| 非目标 | 原因 |
| --- | --- |
| 公网托管站点 | crate 在非 Windows 目标上 `compile_error!`（Linux 服务器无法构建），且服务器没有 DirectML/CUDA |
| 多用户 / 鉴权 / 会话 | 只绑 `127.0.0.1`，单用户本地工具 |
| 模型自动后台下载 | 读请求不应产生 600+ MB 网络与磁盘副作用（决策 2） |
| 把 HTTP/UI 依赖引入库 | 库的边界仍是 `ImageInput` → `OcrOutput`（见 §2.1） |
| 前端工程化（npm/打包器） | 单文件内联 HTML/CSS/JS，零构建步骤 |

---

## 1. 现状盘点（可复用的既有能力）

实施前已存在、**必须复用而不是重写**的部分：

| 能力 | 位置 | 复用方式 |
| --- | --- | --- |
| 检测框叠加渲染 | `output::visualize::draw_output(img: &RecImage, output: &OcrOutput) -> RgbImage` | `GET /api/annotated` 直接调用 |
| 模型下载（含哈希校验） | `model_store::ensure_downloaded(file_url, expected_sha256, save_dir)` | 下载任务的唯一入口 |
| 哈希计算 / 既有文件校验 | `model_store::sha256_file`、`model_store::verify_existing_file` | 状态检查与下载后校验 |
| 默认存储目录 | `model_store::default_model_store_dir()` | 默认模型根 |
| 模型表（URL + SHA256） | `assets/default_models.yaml` + `ModelRegistry::{from_default_yaml, resolve_det, resolve_rec, resolve_cls}` | 解析"该下哪些文件、多大来源" |
| 清单校验 | `ModelManifest::validate_files(root)`：**校验 SHA-256** 且**拒绝绝对路径与 `..` 逃逸** | 判定"模型完整"，并作为路径安全边界 |
| 结果序列化 | `to_output_json` / `to_output_items` / `render_output_report` | API 响应与"导出 JSON/MD/HTML" |
| 逐阶段耗时与 provider | `OcrOutput.timings` / `.stages`、`RuntimeProfile`、`ort_runtime_fingerprint()` | 诊断面板 |
| 输入限制 | 编码字节上限、header 像素探测、24 Mpx 输入上限、`max_side_len` | 请求体与像素上限 |
| 峰值内存 | `runtime::memory`（PSAPI 口径） | 诊断面板 |

**结论**：新增代码集中在"HTTP 层 + 静态页 + 模型生命周期状态机"，不含推断逻辑。

---

## 2. 架构与边界

### 2.1 分层与依赖方向

```
rapidocr (bin)                      ← 只在这里引入 HTTP 依赖
  └── serve 子命令（feature = "serve"）
        ├── http 层：路由 / 请求解析 / 错误→状态码
        ├── 静态页：include_str!("web/index.html") 内联
        ├── 任务层：OCR 队列（有界）+ 下载任务状态机
        └── rapid-ocr-rs（库，零 HTTP 依赖）
              ImageInput → OcrRequest → OcrOutput
```

硬性约束：

- **库不得新增网络/UI 依赖**；`tiny_http` 只能是 `[dependencies]` 中的 **optional**，由 `serve` feature 启用；
- `serve` **不进** `default`，保证发布到 crates.io 的默认依赖图不变；
- 前端只调用本机 API，**不加载任何外部 CDN 资源**（离线可用）。

### 2.2 依赖选择

| 需求 | 选择 | 理由 |
| --- | --- | --- |
| HTTP 服务 | `tiny_http`（blocking，optional） | OCR 本身是阻塞的 CPU/GPU 重活，引入 `tokio`/`axum` 属于为异步而异步；单文件、依赖极少 |
| 上传体解析 | **原始 body**（`application/octet-stream`），不使用 multipart | 避免 multipart 依赖；JS 侧 `fetch(url,{method:'POST',body:file})` 即可 |
| 静态页 | `include_str!` 内联单文件 | 无运行时文件依赖，`cargo install` 后即可用 |
| 打开浏览器 | `cmd /C start <url>`（`std::process::Command`） | 不新增依赖；仅 `--open` 时执行 |
| 磁盘空间检查 | 复用既有 raw Win32 风格调用 `GetDiskFreeSpaceExW` | 与 Windows-only 定位一致，不新增依赖 |
| 任务进度 | 进程内 `Mutex<HashMap<JobId, JobState>>` + 轮询 | M1 不需要 SSE；如后续需要再评估 |

> 备选：若 M3 需要服务端推送进度，再评估 `axum + tokio` 或 `tiny_http` 上的 SSE 手写实现；**M1/M2 不引入**。

---

## 3. 命令行接口

```
rapidocr serve [OPTIONS]

OPTIONS:
  --host <HOST>          默认 127.0.0.1（仅当显式指定才可绑定其他地址，并打印警告）
  --port <PORT>          默认 8760；被占用时报可定位错误，不静默换端口
  --model-dir <DIR>      默认 model_store::default_model_store_dir()
  --config <FILE>        EngineConfig YAML（与 run/evaluate 一致）
  --provider <PROV>      cpu | directml | cuda（默认 cpu）※会话级，启动时决定
  --max-side <N>         覆盖 max_side_len（默认取配置值）
  --allow-download       允许本进程下载模型（未指定时下载接口返回 403）
  --open                 启动后调用系统默认浏览器打开
  --max-body-mb <N>      请求体上限，默认 32
```

约定：

- 与既有子命令一致使用 clap derive，新增 `Command::Serve { .. }`；
- **未启用 `serve` feature 时**，`Serve` 变体仍存在，但执行时返回可定位错误：
  `this binary was built without the 'serve' feature; rebuild with: cargo build --features serve`
  （比 `unknown subcommand` 更容易自查）；
- `--provider directml` 需 `directml-provider` feature，否则返回既有 `feature_disabled` 错误文本。

---

## 4. HTTP API

统一前缀 `/api`。所有响应 `application/json; charset=utf-8`，错误体统一：

```json
{ "code": "models_missing", "message": "…", "detail": { } }
```

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/` | 返回内联单页 |
| `GET` | `/api/status` | 服务与运行时信息：provider 解析结果、ORT 指纹、线程计划、峰值内存、引擎是否就绪 |
| `GET` | `/api/models` | 模型清单与就绪状态（见 4.1） |
| `POST` | `/api/models/download` | 启动下载任务（需 `--allow-download`） |
| `GET` | `/api/jobs/{id}` | 任务状态与进度 |
| `POST` | `/api/ocr` | 执行识别（body = 图片原始字节） |
| `GET` | `/api/ocr/{id}/annotated.png` | 返回叠加检测框的图片（`draw_output`） |
| `GET` | `/api/ocr/{id}/export?format=json\|md\|html` | 导出结果 |

### 4.1 `GET /api/models`

```json
{
  "model_dir": "C:\\Users\\me\\AppData\\Local\\rapid-ocr-rs\\models",
  "downloads_allowed": false,
  "sets": [
    {
      "id": "PP-OCRv6",
      "complete": false,
      "missing": ["PP-OCRv6_det_medium.onnx"],
      "corrupt": [],
      "files": [
        { "name": "PP-OCRv6_det_medium.onnx", "present": false, "size_bytes": 0,
          "expected_sha256": "…", "sha256_ok": null, "source_url": "https://…" }
      ],
      "download_bytes_total": 42106880
    }
  ]
}
```

判定规则（**必须**复用既有实现，不得自写）：

- `present`：文件存在；
- `sha256_ok`：`sha256_file` 与 `ModelManifest`/`default_models.yaml` 记录一致；
- `corrupt`：存在但哈希不匹配 → 前端必须显示"损坏，建议重新下载"；
- `complete`：清单存在时用 `ModelManifest::validate_files(model_dir).is_ok()`；无清单时"全部文件存在且哈希匹配"。

### 4.2 `POST /api/models/download`

请求：

```json
{ "set_id": "PP-OCRv6" }
```

行为与安全约束：

1. `--allow-download` 未启用 → **403** `downloads_disabled`（默认行为，见决策 2）；
2. **只允许下载 `default_models.yaml` 中声明的 URL**，请求体**不得**携带任意 URL（防 SSRF）；
3. 下载前检查可用磁盘空间（`GetDiskFreeSpaceExW`），不足 → **507** `insufficient_disk_space`，并回报所需/可用字节；
4. 逐文件调用 `model_store::ensure_downloaded(url, Some(expected_sha256), model_dir)`，**哈希不匹配即失败**（不静默使用），失败的文件删除后报错；
5. **同一文件单飞**：进程内按文件名加锁，重复请求返回同一 `job_id`，绝不并发下载两份 566 MB；
6. 返回 `202 { "job_id": "…", "files": N }`。

### 4.3 `GET /api/jobs/{id}`

```json
{ "id": "…", "kind": "model_download", "state": "running|done|failed",
  "files_done": 1, "files_total": 2, "bytes_done": 12000000, "bytes_total": 42106880,
  "current": "PP-OCRv6_rec_medium.onnx", "error": null }
```

### 4.4 `POST /api/ocr`

- 请求体：图片原始字节（`Content-Type: application/octet-stream`）；
- 查询参数：`formula=0|1`（默认 0）、`words=0|1`、`chars=0|1`、`max_side=<N>`、`order=reading|top_left`；
- 上限：请求体 ≤ `--max-body-mb`；像素与编码字节上限沿用库内既有检查；
- 响应：`{ "id": "…", "image": {"width":…, "height":…}, "text": "…", "regions": [ … ], "timings": { … }, "stages": { … } }`
  —— `regions`/`text` 直接来自 `to_output_items` 与 `plain_text(TextOrder)`，**不另建一套序列化**。

**错误映射（按 `RapidOcrError` 变体，禁止字符串匹配）**：

| 情况 | 状态码 | `code` |
| --- | --- | --- |
| 模型缺失 | 409 | `models_missing`（带 `missing`/`download_bytes_total`/`downloads_allowed`） |
| 模型哈希不匹配 | 409 | `models_corrupt` |
| 配置非法 | 400 | `bad_config` |
| 图片无法解码 / 超限 | 422 | `unsupported_input` |
| 请求体超限 | 413 | `payload_too_large` |
| 队列已满 | 503 | `busy`（带 `retry_after_ms`） |
| 非允许的 `Host` 头 | 421 | `bad_host` |
| 其他内部错误 | 500 | `internal` |

---

## 5. 前端（单文件内联）

布局：左图 / 右栏。

| 区域 | 内容 |
| --- | --- |
| 左侧 | 拖放区 + `<img>` + 覆盖层 `<canvas>`；粘贴（Ctrl+V）与文件选择；缩放/适应窗口；"下载标注图"按钮 |
| 右侧顶部 | 状态条：provider、ORT 版本、引擎就绪、峰值内存；`max_side_len` 与"公式路由"开关；"重新识别" |
| 右侧主体 | 区域列表：序号（阅读顺序）、文本、置信度、坐标；点击 ↔ 图中框高亮联动；"复制全文" |
| 右侧底部 | 导出 JSON / Markdown / HTML；折叠的**诊断面板**：逐阶段耗时（det/rec preprocess、ORT infer、postprocess）、页面总耗时、未归属余量及其口径说明 |
| 顶部横幅 | 模型缺失/损坏时的提示条：缺失文件、总大小、来源、`允许下载` 状态、下载按钮与进度条 |

实现约束：

- 纯原生 JS，无框架、无构建、无 CDN；
- 框坐标由后端返回的 polygon 直接绘制，**不在前端做几何推断**；
- 诊断面板必须显示时间账本的口径说明文本（"不是严格划分、残差含义"），避免用户误读占比；
- 公式模型提示：provider 说明中标注实测结论——**DirectML 在普通 OCR 上约 2× 加速，但在公式模型上反而慢约 2.3×**（阶段 0 实测），并说明 CUDA 在本机未验证。

---

## 6. 并发与资源模型

- **单引擎 + 串行执行**：`recognize` 需要 `&mut self`，用 `Mutex<Option<RapidOcrEngine>>` 保护；
- **有界队列**：默认 4 个待处理请求，超出返回 503；队列深度可配置；
- HTTP accept 线程与 OCR 工作线程分离，**绝不在 accept 线程里跑推断**；
- 引擎在首次请求或启动时按需构建（惰性），构建时间通过 `/api/status` 暴露；
- 不新增线程池自调优：继续使用 `RuntimeProfile` 的统一线程策略；
- 关闭：`Ctrl+C` 立即退出（M1 不做优雅停机；如有下载任务进行中，提示用户）。

---

## 7. 安全与限制清单

| 项 | 要求 |
| --- | --- |
| 绑定地址 | 默认 `127.0.0.1`；显式绑定其他地址时打印警告 |
| `Host` 头校验 | 仅接受 `127.0.0.1`、`localhost`、`[::1]`（含端口），否则 421 —— 防 DNS rebinding |
| 路径 | 一律经 `ModelManifest::validate_files` / `model_store`，**禁止**用请求内容拼路径 |
| URL | 只允许清单声明来源，**禁止**请求体提供 URL（防 SSRF） |
| 请求体 | 默认 ≤ 32 MB；像素/编码字节上限沿用库内检查 |
| 响应体 | 不返回本机绝对路径以外的敏感信息；`/api/status` 只返回 ORT 指纹与模型目录 |
| 下载 | 默认关闭；开启需显式 `--allow-download`；哈希校验失败必须失败 |
| 日志 | 只打印到 stdout/stderr；不落盘用户图片；临时产物放 `%TEMP%` 并在请求结束后清理 |

---

## 8. 实施里程碑

### M1：可用闭环（本文件核心交付）

- [ ] `Cargo.toml`：`tiny_http` 作为 optional 依赖；新增 `serve` feature（**不进 default**）；确认 `default-features` 构建不拉入 HTTP 依赖
- [ ] `Command::Serve`（clap）+ `--host/--port/--model-dir/--config/--provider/--max-side/--allow-download/--open/--max-body-mb`
- [ ] 未启用 feature 时给出可定位错误
- [ ] `GET /` 内联单页；`GET /api/status`
- [ ] `GET /api/models`（复用 `ModelRegistry` + `ModelManifest::validate_files` + `sha256_file`）
- [ ] `POST /api/models/download` + `GET /api/jobs/{id}`（含 403/507/单飞/哈希校验）
- [ ] `POST /api/ocr` + 错误映射表 + 有界队列
- [ ] 前端：上传/粘贴、左图叠框、右栏结果、复制全文
- [ ] 模型缺失横幅（409 驱动）+ 下载按钮与进度
- [ ] 单元测试：路由与错误映射（含 409/403/413/421/503）
- [ ] 真实资产验证：12 张测试图经 HTTP 的 `regions` 数量与 `rapidocr run --json` **逐张一致**
- [ ] 手工验证：浏览器完成一次"上传 → 结果 → 下载标注图"闭环

**验收标准**：`cargo run --features serve -- serve --open` 后，浏览器上传任一真实截图，右侧结果与 CLI `rapidocr run --json` 的区域数、文本一致；把 `--model-dir` 指向空目录时，界面显示缺失文件与大小，点击下载（在 `--allow-download` 下）后自动恢复可用。

### M2：诊断与 provider 切换

- [ ] `GET/PUT /api/settings`：运行期切换 provider（重建引擎并暴露 `rebuilding` 状态）
- [ ] 诊断面板：逐阶段耗时、`timing_split`/账本口径说明、峰值内存、ORT 指纹
- [ ] 公式路由开关 + 公式区域单独分区展示
- [ ] provider 按钮上直接展示实测结论（DirectML 对公式模型更慢）

### M3：评估模式

- [ ] 上传"图片 + 参考文本"（或 zip）→ 服务端算 CER / 精确匹配，复用 `evaluation` 模块
- [ ] 结果表格可按 CER 排序，导出失败样本清单
- [ ] 与 `rapidocr evaluate` 的报告字段保持一致，避免两套指标实现

---

## 9. 验证计划

| 类别 | 内容 | 命令/方式 |
| --- | --- | --- |
| 静态检查 | fmt / clippy `-D warnings` | `cargo fmt --all -- --check`；`cargo clippy --all-targets --all-features -- -D warnings` |
| 单元测试 | 路由、参数校验、错误映射、任务状态机、单飞、Host 校验 | `cargo test --features serve` |
| 依赖隔离 | 默认构建不得包含 `tiny_http` | `cargo tree -e normal --no-default-features`；`cargo package --list` 对比 |
| 打包 | 包内不含前端源码以外的构建产物；`serve` 为可选 | `cargo package --allow-dirty --no-verify` |
| 集成（无模型） | 空 `--model-dir` → `/api/ocr` 返回 409 且字段完整 | 临时目录 + 真实图片字节 |
| 集成（真实模型） | 12 张图经 HTTP 与 CLI 结果一致 | 见 §10 命令 |
| 手工 | 浏览器闭环、粘贴、标注图下载、损坏模型提示 | 人工 + 截图存档 |
| 未覆盖风险 | 网络下载真实来源（ModelScope）在 CI 不可用 → 用本地 HTTP 服务器提供 fixture 文件做端到端下载测试，并在报告中标注"真实来源未验证" |

**证据要求**：每个里程碑结束时，在 `docs/06-local-web-demo-reports.md`（新建）记录：执行命令、关键输出、与验收标准的对照、未覆盖风险。**不得**以"界面看起来正常"作为验证通过。

---

## 10. 参考命令（实施后填实测值）

```powershell
# 构建并启动（开发）
cargo run --features serve -- serve --port 8760 --open

# 允许下载 + 指定模型目录
cargo run --features serve -- serve --allow-download --model-dir "$env:LOCALAPPDATA\rapid-ocr-rs\models"

# 模型状态
curl.exe -s http://127.0.0.1:8760/api/models | python -m json.tool

# 触发下载
curl.exe -s -X POST http://127.0.0.1:8760/api/models/download -H "Content-Type: application/json" -d '{\"set_id\":\"PP-OCRv6\"}'

# 识别一张真实图片
curl.exe -s -X POST --data-binary "@D:\100_Projects\110_Daily\SnapClip\OCR-test-image\01基础多位置文本.png" "http://127.0.0.1:8760/api/ocr?max_side=2000" -o target\serve-ocr.json

# 与 CLI 对照（区域数应一致）
.\target\release\rapidocr.exe run --img-path "D:\100_Projects\110_Daily\SnapClip\OCR-test-image\01基础多位置文本.png" --config "D:\100_Projects\110_Daily\SnapClip\OCR-Model\test-config-small.yaml" --json
```

---

## 11. 风险与对策

| 风险 | 影响 | 对策 |
| --- | --- | --- |
| 请求体/像素过大导致 OOM | 进程被杀 | 双层上限（body + 库内像素/字节检查），超限 413/422 |
| 公式识别单图可达数秒~数十秒 | 请求超时、界面"卡住" | 有界队列 + 前端显示"处理中/耗时"；`/api/status` 暴露引擎状态；必要时 M3 再做取消 |
| provider 切换需重建会话 | 切换后首个请求很慢 | 明确返回 `rebuilding` 状态与耗时，不假装即时生效 |
| 下载 566 MB 公式模型 | 磁盘与时间成本高 | 默认关闭、显式点击、显示体积、下载前查空间、单飞、完成后哈希校验 |
| 端口占用 | 启动失败 | 报可定位错误（含端口号与占用提示），不静默换端口 |
| `serve` 依赖泄漏进默认构建 | 影响 crates.io 发布体积与依赖图 | feature 隔离 + `cargo tree`/`cargo package` 验证（§9） |
| 把 HTTP 逻辑写进库 | 破坏库边界 | §2.1 的硬性约束 + review checklist |

---

## 12. 待确认问题（实施前）

1. 默认端口取 `8760` 是否可接受？
2. 默认模型集：`PP-OCRv6`（det+rec+dict）是否需要再提供 `PP-OCRv4/v5` 的切换入口？
3. M1 是否需要"粘贴剪贴板图片"（实现成本很低，浏览器 `paste` 事件即可，建议纳入）？
4. 诊断面板是否默认展开（建议默认折叠，避免干扰首次体验）？
