# 本地 Web 评估界面（`rapidocr serve`）实施文档

> 状态：**M0–M4 已实现并通过验收**（逐里程碑证据见 `docs/06-local-web-demo-reports.md`；协议已冻结，冻结结论见下）
> 已定决策：**仅本机（loopback）** · **模型下载默认关闭、显式触发** · **实现为 `rapidocr serve` 子命令**
> 关联文档：`docs/03-windows-only-optimization-tasks.md`、`docs/04-windows-phase-reports.md`
> 参考规范：MDN（Fetch / XMLHttpRequest.upload / AbortController）、`tiny_http::Server`、OWASP SSRF Prevention、WCAG 2.2
>
> **本行曾在实现完成后仍写着"M0 待冻结，未进入实现"**——那是 M4 交付时遗留的陈旧状态声明，已按
> 事实改正（同一文件末尾的"实施完成记录（M0-M4）"当时已经勾选了全部里程碑）。本轮还纠正了
> §4.2.1/§4.4/§5.4/§7.2/§13/§15 里与实现不一致的表述，逐处见
> `docs/06-local-web-demo-reports.md` 的"M1 评审修复轮"记录。

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
| 哈希计算 | `model_store::sha256_file` | 状态判定与下载校验。**M1 评审 P1-2 之后的实际形态**：状态判定与两条公式加载路径改走 `model_verify` 的**身份键控缓存**（`path + size + mtime`），`sha256_file` 仍是唯一的底层实现与下载校验入口 |
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
| 下载跟随重定向 | 默认 `reqwest::blocking::Client` | **禁止盲从**；改为手工逐跳校验（§6.1 第 2 条） |
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
  --reverify-models          **启动期冷验证**这次运行会真正加载的每个模型文件（忽略校验缓存），
                             任一缺失/损坏即**拒绝启动**并点名那些文件。范围 = 文本管线的
                             detector/recognizer/dictionary（`use_cls` 时再加 classifier）+
                             公式管线的 formula_recognizer（`--formula-detector` 声明的检测模型
                             不属于模型集，因此由加载路径自己校验）——**不是**默认表里的每一个
                             文件。它与"清缓存"无关（新进程的缓存本来就是空的）：价值在于把验证
                             从**首次使用**移到**启动期**。见 A1 一节
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

  # M4：公式队列与评估
  --formula-detector <ONNX>  页面公式检测模型（pix2text-mfd-1.5.onnx）；给出即启用公式队列
                             （§4.2、§10.8）。缺省时 `queue=formula` 仍是 400，
                             理由进 `/api/models` 的 `formula.disabled_reason`
  --max-eval-cases <N>       `POST /api/evaluate` 一张清单最多评估多少张图，默认 32
  --eval-root <DIR>          `POST /api/evaluate` 的沙箱根（**默认不配置**）。端点是唯一
                             接收**本机路径**的端点，因此读取范围必须显式开启：
                             给出后清单与它引用的每张图都必须规范化到该目录内
                             （拒绝 `..`、绝对路径逃逸与符号链接逃逸）；**不给时整个
                             端点拒绝**并给出可定位理由（§4.2、M1 评审 P2-3）
```

**参数优先级（必须一致并记录）**：**CLI flag > `--config` YAML > 内建默认**。
`serve` 会用 CLI 值覆盖配置中的 `max_side_len` 与 `provider_preference`，并在启动日志中打印"哪个值生效、被覆盖的值是什么"。未启用 `serve` feature 时子命令仍可解析，但返回可定位错误（提示 `cargo build --features serve`）。

**M4 的两处实现结论（改的是文档没写清的地方）**：

1. 公式**识别**模型（`pp_formulanet_plus_m.onnx`，566 MB）来自模型集（`formula_recognizer` role，
   默认表可下载）；公式**检测**模型在 `FormulaPolicy` 里是可选的、也没有可信的公开下载来源
   （§5.1 的 role 枚举里有 `FormulaDetector`，但 `assets/default_models.yaml` 刻意不登记它），
   因此 `serve` 只能通过 `--formula-detector` 显式给出。本地清单若声明了 `formula_detector`
   role，`serve` 也认它（CLI > 模型集）；两者都没有时**公式路由不可用**——理由是可读文字，
   见 §5.4 的 `formula.disabled_reason`。
2. 没有检测模型时**不**打开路由，而不是"接了但永远产不出公式区域"：`FormulaPolicy` 在没有
   `detector_path` 时只处理调用方显式声明的区域（`input_regions`），而 HTTP 请求里没有这种
   区域。那样公式任务会稳定地"成功且零公式区域"，是一个看起来能用、实际什么都没做的路径。

---

## 4. 协议：统一的异步任务生命周期

### 4.1 设计原则

OCR（尤其公式路径）单图可达数秒至数十秒，**不得长期占用 HTTP 请求**。所有识别都是异步任务，不存在"同步返回结果"的第二套语义。

**M4 的唯一例外（显式记录，不是悄悄放宽）**：`POST /api/evaluate` 是**批量、无中途交互**的
行政动作——它的结果是一份完整报告，没有"部分报告"这样可轮询的中间状态，也没有取消语义
（推理不可中断，§4.3）。因此它**同步返回报告**，但必须满足三条：请求在**独立线程**里执行、
由那个线程写响应（accept 线程立刻回到循环，服务全程可观测、可提交任务）；同时只允许一个
评估（第二个请求 503 `busy`，不排队）；用例数受 `--max-eval-cases` 约束，一次请求的时间有上界。
推理仍然走与 OCR 任务**同一条**引擎锁路径，并遵守"会话绝不在 accept 线程上建立"。

### 4.2 端点

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/` | 内联单页（注入 nonce + token） |
| `GET` | `/api/status` | 引擎/provider/ORT 指纹/队列与内存概况（路径脱敏）+ M4 的 `formula` 块（与 `/api/models` 同源同值） |
| `GET` | `/api/models` | 模型集状态（§5.4；M4 起顶层字段是**文本管线**作用域，公式管线在 `formula` 块里） |
| `POST` | `/api/models/download` | 启动下载任务（需 `--allow-download` **且** token）；请求体 `{"set_id": "<id>"}`，未知 id → **404 `model_set_not_found`**（绝不回落 `sets[0]`）。公式集合（566 MB）与文本集合同一条路径、同一个按钮语义 |
| `POST` | `/api/models/reverify` | **A2 新增**：一次动作完成"重新读盘"——①清掉校验缓存，②**冷验证**这次运行会真正加载的每个文件，③**引擎此前就绪则重建会话**（否则"清缓存"只是一个按下去什么都不会变的按钮：流水线的会话缓存可能仍在服务一个磁盘上已经不是这个文件的模型）。请求体**必须为空**（有 body → 400）；单飞（第二个并发调用 **503 `busy`**，与 `POST /api/engine/reload` **同一把**资格），且整个序列在**独立线程**里执行（accept 线程绝不建会话）。响应见 §5.5 |
| `POST` | `/api/ocr` | 提交识别 → **202** `{job_id, queue, position, state:"queued"}`。队列由 `?queue=text\|formula` 选择，**队列类别就是管线选择**（M4，见下） |
| `POST` | `/api/evaluate` | **M4 新增**：批量评估一份已标注清单 → 200 + 库的评估报告。**必须先配置 `--eval-root <DIR>`**（M1 评审 P2-3）：未配置时整个端点拒绝（400 `bad_request`，`detail.reason` 点名 `--eval-root`）；配置后清单与清单引用的**每张图**都必须规范化到该目录内，越界（`..`、绝对路径、符号链接）是可定位的 400。请求体**恰好**一个键 `{"manifest": "<本机清单路径>"}`（与 `rapidocr evaluate --manifest` 同格式：`[{image, text, boxes}]`，`image` 相对清单目录解析）。报告字段与 CLI 的 `rapidocr evaluate` **同一份实现**（`cases[]` + `mean_cer` + `exact_match_rate` + `mean_detection_*` + `peak_working_set_bytes` + `memory_source` + `ort_runtime` + `ort_runtime_version`），另加 `iou_threshold` 与 `manifest_file`（只给文件名）。可定位的拒绝是 **400 `bad_request`** + `detail.reason`（没有 `--eval-root`/清单越界/格式不对/用例数超过 `--max-eval-cases`/某张图越界或读不出来）；模型缺失与引擎不可用分别是 409 `models_missing` / 503 `engine_unavailable`（与 `/api/ocr` 同一份错误体） |
| `GET` | `/api/jobs/{id}` | `{id, kind, queue, state, position, queued_ms, started_ms, elapsed_ms, error}` + M2 追加的 `{failure, download, cancel_requested}`（见 §4.3） |
| `GET` | `/api/jobs/{id}/result` | 结果（未完成 409 `job_not_finished`；已淘汰 410 `job_evicted`） |
| `GET` | `/api/jobs/{id}/annotated.png` | 叠加检测框 PNG（原图淘汰 → 410 `original_evicted`） |
| `GET` | `/api/jobs/{id}/export?format=json\|md\|html` | 导出（HTML 走静态模式 + 独立 CSP，§9.5） |
| `POST` | `/api/jobs/{id}/cancel` | 取消（§4.3） |
| `POST` | `/api/engine/reload` | 显式创建/重建引擎。请求体**可省略**：省略 = "按磁盘上的当前文件重建会话"；带 `{"provider":"cpu\|directml\|cuda"}` = **显式应用 provider 设置**（§7.6 的 `Rebuilding` 序列，运行期切换，M3）。响应 `{outcome, engine, provider, requested, selected_ep, fallback_to_cpu, missing, corrupt, source, model_dir, load_ms, rollback_ms, error}`（§7.6） |

#### 4.2.1 公式队列的请求协议（M4 定案）

页面上的"公式识别路由"开关**只**通过 `?queue=formula` 表达，没有第二个开关：

```text
POST /api/ocr?queue=text                → 文本管线，进文本队列（默认）
POST /api/ocr?queue=formula             → 公式管线，进公式队列
```

**为什么不做 `formula=1`**：队列类别与管线是一一对应的（§8.3 的双队列正是"普通 OCR / 公式 OCR"
两条管线），再加一个布尔开关就会多出一种自相矛盾的组合（`queue=text&formula=1` 该按哪个跑？），
而页面本来就已经按开关发送 `queue=formula`。因此这里选择"一个含义一个字段"，
而不是"两个可能冲突的字段相加"。

三种结论必须可区分：

| 情况 | 结论 |
| --- | --- |
| 路由可用、公式模型齐备 | 202 `{queue:"formula"}`，进公式队列，由公式管线执行 |
| 路由可用、公式模型**缺失/损坏** | **409** `models_missing` / `models_corrupt`，`detail.scope="formula"`、`missing`/`corrupt`/`blocked` 列出公式 role 的文件，`detail.detector` 给出检测模型的 `{configured, file, sha256, state}`。判定在**读 body 之前**完成，用的是与 `/api/models` **同一份哈希状态**（库的身份键控校验缓存：键 = 路径 + 体积 + mtime，冷验证真的读盘、命中只花一次 `stat`）；`state` 取 `present`/`corrupt`/`missing`，`sha256` 为 `null` 表示没有可信摘要（只能证明"存在"） |
| 路由**不可用**（没配 `--formula-detector`） | **400** `bad_request`（M1 起不变），理由在 `/api/models` 的 `formula.disabled_reason` 里 |

**M1 评审 P1-1/P1-2 的修正（本节上一版的说法已经被实现否掉，如实记录）**：上一版写的是
"存在性检查在**读 body 之前**完成（只 `stat`，不哈希 566 MB）；权威哈希判定由库在加载识别器时
按集合声明的 SHA-256 执行"。那条规则有两个缺口，现在已经从根上修掉：

1. **检测模型从不校验**：`formula_detector` 的路径被单独传递、集合声明的 SHA-256 被丢掉，
   `FormulaDetector::from_model` 不校验任何东西——一个"文件在、内容错"的检测模型因此能一路
   加载成功。现在检测模型的声明摘要与识别模型走**同一条**规则、**同一个**入口形状
   （`FormulaPolicy.expected_detector_sha256` ↔ `expected_model_sha256`），不匹配是
   `HashMismatch`（路径 + 期望 + 实际）；
2. **请求路径只看存在性**：损坏但存在的公式文件会溜过准入、读出整个 body、建出任务，最后才在
   worker 里失败。现在准入与 `/api/models` 共用同一份哈希状态（缓存命中是 `stat` 级），
   损坏文件在**读 body 之前**就是 409；
3. 同时修掉的是**缓存失效**：识别器/检测器的会话缓存从"按路径"改为"按文件身份"，文件被替换或
   损坏后不会被内存里那份旧会话静默继续使用。

残留盲区（**B 轮已收窄，如实陈述**）：身份 = `(path, size, mtime)` **加上首尾各 64 KiB 的
局部摘要**。因此"体积不变且 mtime 不变"的内容替换**只有在首尾 64 KiB 逐字节相同时**才不被
识别（把改动放在中段、或用 `SetFileTime` 把时间戳写回原值的组合，现在会被抓住：
`verification.partial_mismatches` 计数、`POST /api/models/reverify` 的 `content_changed`
以及启动日志都会说明"这次为什么重新哈希了"）。

**局部摘要是启发式，不是安全边界**：能写这个文件的人同样能保留首尾、只改中段，因此它
降低的是"误把改过的文件当成没改"的概率，**不是**"防住能写文件的人"。`mtime` 不可得
（文件系统不提供）时身份里是 `None`，同类替换同样落在盲区里。**确定性地**排除缓存影响只有
两个入口：启动期的 `--reverify-models`（缺失/损坏即拒绝启动）与运行期的
`POST /api/models/reverify`（清缓存 → 冷验证 → 重建引擎）。这一点同时写在
`src/model_verify.rs` 的模块文档与 `/api/models.verification.residual_blind_spot` 里
（响应里就能读到，不必翻文档）。

**小于 128 KiB 的文件（规则明确）**：首块 = 前 `min(size, 64 KiB)` 字节，尾块 = 后
`min(size, 64 KiB)` 字节。因此 `size ≤ 128 KiB` 时两个切片重叠、局部摘要实际覆盖**整个
文件内容**——"首尾相同"就等于"内容相同"，盲区对这类文件不存在（默认表里的
dictionary/tokenizer 就是这种量级）。`size == 0` 时两块都退化为同一个域分隔编码，规则仍然
唯一确定。以上三条规则都有单元测试钉住（含"中段改动**不**被发现"这条**限制本身**）。

**普通 OCR 永不因公式缺口而失败**：`/api/ocr` 的 409 只报告**文本管线**的缺失文件，
`EngineState::BlockedModelsMissing` 的清单同样是文本作用域（§5.4、§7.6）。

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
4. **队列容量预留** → 满则 **503**（此时**尚未读取请求体**，直接回 `Request` 让连接关闭）。
   M1 评审 P2-1：这一步是**判定与占位同一个动作**——预留凭据随请求一路传到入队，入队成功即提交、
   任何失败/中止即归还容量。上一版的"先读一个 `queue_full` 布尔值、通过后再入队"是**两个临界区**，
   "拒绝时不读 body"在并发下不成立；
5. `Content-Length` 预检（> `--max-body-mb` → **413**）；
6. 有界流式读取：总字节上限 + **读取超时**（chunked 请求同样受这两条约束）；
7. 仅在以上全部通过后才解码与建任务（OCR 的第 4 步凭据在这里被提交）。
   `queue=formula` 的模型可用性（哈希状态）检查发生在第 4 步之后、第 6 步之前（§4.2.1）。

### 4.5 结果存储、淘汰与 tombstone

| 项 | 默认 | 行为 |
| --- | --- | --- |
| 普通队列 / 公式队列 | 4 / 2 | 满 → 立即 503（不阻塞） |
| 终态任务保留 | 32 个 / 64 MB | 超限按"最旧终态优先"淘汰 |
| 任务 TTL | 600 s | 后台清理线程，不依赖访问触发 |
| **tombstone** | 256 个 / 同 TTL | 淘汰时把 `job_id → evicted_at` 写入有界 FIFO tombstone 表；**这是 410 能成立的前提** |
| 查询语义 | — | 活跃 → 正常；**在 tombstone 内 → 410 `job_evicted`**；两者都无 → 404 `job_not_found` |
| 原图保留 | 仅编码字节，计入字节预算 | `annotated.png` **按需**从编码字节重新解码，**不长期保留 `RecImage`** |
| 原图释放顺序 | 字节超限时**先释放最旧终态任务的原图**（任务记录与结果都保留 → 该端点是 410 `original_evicted`），全部释放完仍超限才淘汰整个终态任务 | M3 澄清：§4.2 冻结的 `original_evicted` 只有在"任务还在、原图已被预算收回"时才可达；若字节压力总是整任务淘汰，那个 `code` 永远不可达（M0c 的整任务淘汰因此是**第二步**，不是唯一一步）。另：失败/取消的任务**永不**会有注释图，它们的原图在进入终态时立即释放 |
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
   - **M4 的收口**：`serve` 的清单请求是 `ModelRequest::text_and_formula`（两条管线的 role 并集），
     因为页面必须能报告公式集合的体积/哈希并让用户按集合下载。因此**本地清单也要声明
     `formula_recognizer`**；缺它时启动期就会列出缺失 role（错误里同时点名两条管线缺什么），
     而不是等到第一次公式请求。这一条是 §5.3 原文的直接推论，M1 时还看不出来（当时只请求文本管线）。
3. `default_models.yaml` 仍是"可下载来源表"的载体（URL + SHA256），`ModelSet` 由它或 manifest 构造；**HTTP 层永不直接解析 YAML**。

### 5.4 `GET /api/models` 响应

```json
{
  "model_dir": "<redacted>",
  "source": "default_table | local_manifest",
  "downloads_allowed": false,
  "complete": false,
  "missing": ["PP-OCRv6_det_small.onnx"],
  "corrupt": [],
  "blocked": ["PP-OCRv6_det_small.onnx"],
  "formula": {
    "complete": false,
    "missing": ["pp_formulanet_plus_m.onnx"],
    "corrupt": [],
    "blocked": ["pp_formulanet_plus_m.onnx"],
    "routing": true,
    "disabled_reason": null,
    "required_roles": ["formula_recognizer"],
    "detector": {
      "configured": true,
      "file": "pix2text-mfd-1.5.onnx",
      "sha256": "…|null",
      "state": "present | corrupt | missing"
    }
  },
  "verification": {
    "identity": "path + size + mtime + SHA-256 of the first and last 64 KiB",
    "partial_window_bytes": 65536,
    "cold_this_call": 0,
    "cold_verifications": 4,
    "cache_hits": 37,
    "partial_reads": 41,
    "partial_mismatches": 0,
    "entries": 4,
    "last_cold_ms": 1180.4,
    "last_cold_bytes": 593915961,
    "guarantee": "content is verified at first use and re-verified whenever the file's identity changes (path, size, mtime, or the first/last 64 KiB); the service does not claim that the on-disk content is trusted at all times",
    "force_check": "POST /api/models/reverify (or --reverify-models at startup) is the deterministic way to force a fresh full check",
    "residual_blind_spot": "a same-size, same-mtime edit that also keeps the first and last 64 KiB byte-identical is still not detected; the partial digest is a heuristic that narrows the window, not a security boundary, because an attacker who can write the file can also preserve its head and tail"
  },
  "sets": [
    { "id": "PP-OCRv6", "complete": false, "download_bytes_total": 42106880,
      "files": [ { "name": "PP-OCRv6_det_medium.onnx", "role": "detector",
                   "state": "missing", "size_bytes": 4712345, "sha256": "…", "source_url": "https://…" } ] }
  ]
}
```

**M1 评审 P1-1/P1-2 对形状的两处补充（本文件上一版没有这两块）**：

- `formula.detector` 增加 `sha256` 与 `state`：检测模型与识别模型**同一条**完整性规则。
  `sha256` 为 `null` 表示没有可信摘要（`--formula-detector` 指向一个模型集没有声明过的文件），
  此时 `state` 只能是 `present`（"文件在"）或 `missing`，绝不假装校验过；
- `verification` 是**校验成本账 + 保证强度**：`cold_this_call` 是这一份报告里真的重算了摘要的
  文件数（页面每 8 s 轮询时必须是 0），`cold_verifications`/`cache_hits`/`partial_reads`/
  `partial_mismatches`/`entries` 是进程累计，`last_cold_ms`/`last_cold_bytes` 是最近一次冷验证的
  实测耗时与被读字节数，`guarantee`/`force_check`/`residual_blind_spot` 是**如实**的保证陈述
  （内容在首次使用时验证、在身份变化时重新验证；服务不声称磁盘上的内容在任何时刻都可信；
  需要确定性重查就用 `--reverify-models` 或 `POST /api/models/reverify`）。
  它存在的意义是让"不再重新哈希 566 MB"这句话**可被验证**，而不是一句承诺。

**M4 的作用域修正（本文件上一版把四个顶层字段写成"所有集合"的并集，那是错的）**：

- 顶层的 `complete`/`missing`/`corrupt`/`blocked` = **文本管线**（detector/classifier/recognizer/
  dictionary/tokenizer），也就是引擎真正要加载的那些文件；`POST /api/ocr` 的 409 `detail`
  与 `EngineState::BlockedModelsMissing` 用它。公式模型（566 MB，默认不下载）缺失**绝不会**
  让普通 OCR 变成 409——这正是 M4 要修掉的根因；
- `formula` 块 = **公式管线**（`formula_recognizer`）的同一组字段，外加
  `routing`（服务端是否启用了公式队列，§4.2）与 `disabled_reason`（不可用时的**文字**理由，
  页面据此禁用开关并显示原因，§9.4 要求不只靠颜色）；`detector` 只给**文件名**（§7.4 路径脱敏）；
- 分组依据是 `files[].role`（`Pipeline::of`），**不是集合 id 或集合顺序**：默认表把两条管线
  放在两个集合里，本地清单把两条管线放在**一个**集合里，两种来源下结论必须一致；
- 页面同一套判据：`QUEUE_ROLES.text = [detector, recognizer, dictionary]`、
  `QUEUE_ROLES.formula = [formula_recognizer]`（= 库的 `ModelRequest::text_roles/formula_roles`），
  再叠加 `formula.routing`。
- `complete` 仍然要求"每个文件都 `Present` **且**声明了哈希"（§5.2：没有哈希的文件不得让集合
  报 `complete`）；`missing`/`corrupt` 只列缺失与损坏。

### 5.5 `POST /api/models/reverify` 响应（A2）

```json
{
  "outcome": "ready | rolled_back | blocked_models_missing | failed",
  "engine": { "state": "ready", "requested": "cpu", "selected_ep": "CPUExecutionProvider",
              "fallback_to_cpu": false },
  "provider": { "requested": "cpu", "selected_ep": "CPUExecutionProvider", "fallback_to_cpu": false },
  "requested": "cpu",
  "selected_ep": "CPUExecutionProvider",
  "fallback_to_cpu": false,
  "missing": [], "corrupt": [], "source": "default_table", "model_dir": "<redacted>",
  "load_ms": 412, "rollback_ms": null, "error": null,
  "computed": 5,
  "content_changed": ["fx.onnx"],
  "files": [
    { "name": "PP-OCRv6_det_small.onnx", "role": "detector", "pipeline": "text",
      "state": "present", "declared_sha256": "…", "sha256": "…",
      "cause": "first_sight | stat_changed | content_changed | cache_hit",
      "digest_computed_this_call": true }
  ],
  "verification": { "…与 §5.4 同源…", "digests_computed": 5 }
}
```

字段语义（**逐条都是可断言的**）：

- `computed` = 这一轮真的重算了几个**完整摘要**（`POST /api/models/reverify` 从不查缓存，
  因此它等于本次运行会用到的文件数，读不出来的文件除外）；`verification.digests_computed`
  是同一个数字的另一种读法（放在成本账里，免得读者在两处之间猜口径）；
- `content_changed` = **stat 身份相同、首尾 64 KiB 不同**的文件名。这是"为什么又读了一遍
  566 MB"的唯一原因来源，也是 B 轮收窄盲区的可见证据；
- `files[].cause` 是逐文件的原因；`files[].sha256` 是**这一轮算出来的**实际摘要
  （不是声明值——声明值在 `declared_sha256` 里）；
- `state` 只用 §5.2 的三个取值；`missing` 与 `corrupt` 的区分是"文件不在"与"文件在但读不出来
  或内容不对"（前者建议下载，后者建议重新下载并以原子方式替换）；
- `engine`/`outcome`/`load_ms`/`rollback_ms`/`error` 与 `POST /api/engine/reload`
  **同一套语义**（`outcome` 只有四种取值；`error` 非空且 `engine` 仍 ready 就是
  `rolled_back`），因此客户端不必学第二套状态词汇；
- **请求体必须为空**：有 body → 400 `bad_request`（无参数动作）。

> **为什么必须有第 3 步（重建引擎）**：只清缓存不重建会话，就等于一个按下去什么都不会变的
> 按钮——`RapidOcrEngine` 的会话缓存按文件身份失效，而"同体积 + 同 mtime（+首尾相同）"的
> 替换可能不被身份察觉，于是内存里那份旧会话会继续服务一个磁盘上已经不是这个文件的模型。
> 端点的测试里有一条正是断言这一点：损坏的模型让端点报 `corrupt` 并且**引擎状态是可定位的
> 错误态**（而不是"旧引擎继续 ready"），还原后同一个端点重建会话并回到 `ready`。

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

1. **仅 HTTPS**；初始 URL 与**每一跳**都适用，否则 `SchemeRejected`（`https` → `http` 的降级也是 `SchemeRejected`）；
2. **禁止盲从重定向；只允许手工逐跳跟随**（`redirect(Policy::none())` 关掉客户端自动跟随，由下载器自己实现跟随循环）。规则逐条冻结如下：
   - **每一跳都重新校验** scheme 与 host：scheme 必须是 `https`；host 必须属于**生效白名单**（编译期常量 ∪ 显式 `--allow-download-host`，见第 3 条）。越界即 `HostRejected`，错误里给出**那个** host；
   - **跳数上界 N = 5**（`MAX_REDIRECT_HOPS`）。超过上界仍是 3xx → `RedirectRejected`（带那次的 `Location`）；3xx 缺少可用的 `Location` 同样是 `RedirectRejected`；
   - **相对 `Location` 按当前 URL 解析**（`Url::join`，RFC 9110 允许相对引用）；
   - **不携带任何凭据跨跳**：每个 hop 都用同一个请求形状（自己的 `User-Agent` + `Referer`，无 `Authorization`、无 cookie、无自定义令牌头）；
   - **路径也要一致**：跳转目标的 URL 末段必须与初始 URL 的末段相同（否则落盘文件名会与模型表声明的名字错位）→ `RedirectRejected`；
   - **不跟随非 3xx**：2xx 才进入长度预检/流式上限/哈希校验，其它状态码仍按"非 2xx"报 `Network`；
   - 跟随**不改变**其它任何保证：`Content-Length` 预检、`take(max_bytes + 1)` 流式上限、唯一临时名、`MoveFileExW` 原子替换、强制 SHA-256、单飞、磁盘预检、分项超时全部作用在**最终**响应体上（超时按跳计）。
   理由（OWASP SSRF Prevention）：重定向是绕过白名单的经典路径——盲从等于把"下载哪个地址"的决定权交给上游；只有逐跳校验才能在保留白名单语义的前提下接通真实来源（ModelScope 对 ONNX 权重应答 **302 → `cdn-lfs-cn-1.modelscope.cn`**，M2/M2b 实测）；
3. **host 允许列表必须来自可信配置，不得来自模型清单**：默认是**编译期固定**白名单（当前为 `www.modelscope.cn` 与 ModelScope LFS CDN `cdn-lfs-cn-1.modelscope.cn`，后者只作为重定向目标出现）；本地 `manifest.json` 可以提供 URL，但**不能扩大**该白名单，越界即 `HostRejected`；确需其他来源时必须显式传 `--allow-download-host <HOST>` 并打印高风险警告（OWASP：allowlist 必须来自可信配置，而不是资源描述自身）；host 比较是**整串精确**匹配（大小写不敏感），不做后缀/子域放宽；
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
| 令牌 | 启动时随机生成，注入页面下发；客户端用 `X-RapidOCR-Token`；**所有 `/api/*` 都需要**（`GET /` 除外）。熵**只**来自操作系统 CSPRNG（Windows `BCryptGenRandom`，`BCRYPT_USE_SYSTEM_PREFERRED_RNG`；非密码学的时间/PID/栈地址派生已删除），比较保持常量时间；**fail-closed**：CSPRNG 不可用时服务**拒绝启动**，绝不退回弱熵（M1 评审 P2-4）。CSP nonce 用同一个 CSPRNG |
| CORS | **不发送任何** `Access-Control-Allow-Origin`；绝不 `*` |
| Cookie | **不使用** |
| 下载 | 同时要求 `--allow-download` **与** token；请求体**不得**携带 URL（防 SSRF） |
| 评估的本机路径 | `/api/evaluate` 是唯一按**路径**读取本机文件的端点：必须显式给 `--eval-root <DIR>`，清单与每张图都要规范化到该目录内；未配置时整个端点拒绝（M1 评审 P2-3） |
| 下载的跳转（出站 SSRF） | 重定向**不得盲从**：逐跳校验 scheme（仅 `https`）与 host（编译期白名单 ∪ 显式 `--allow-download-host`），上限 5 跳，相对 `Location` 按当前 URL 解析，凭据不跨跳，越界/超限是可定位错误（§6.1 第 2 条） |
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

**M3 的运行期 provider 切换（显式设置应用，不是即时下拉）**：

请求 = `POST /api/engine/reload` + `{"provider": "cpu|directml|cuda"}`；**请求只在序列结束或失败后才返回**，
而在它执行期间服务必须继续可观测、可提交任务，因此完整顺序是：

1. **先校验**（与启动期**同一套**规则：provider 名称、对应 feature 是否编译进来、§7.5 的回退语义）：
   非法 → **400 `bad_request`**（`detail.reason` 是库侧原文），**任何状态都不动**；
2. **暂停新任务**：`Ready → Rebuilding`（§7.6 的合法边）。此刻 `POST /api/ocr` 是
   `OcrAdmission::Queue`——**入队（202）而不是拒绝**（状态机里那条规则就是为它冻结的）；
   与此同时 `/api/status` 报告**新**的 `requested`，而 `selected_ep`/`fallback_to_cpu` 是 `null`
   （未知态不得伪装成 `false`，§7.5）；
3. **排空**：拿到"引擎会话"那把锁即"正在进行的那次推理已经结束"；在跑的推理**不**被中断（§4.3）；
4. **销毁旧 engine → 创建新 engine**（顺序固定，避免失败时留下一个与 `/api/status` 不一致的可用引擎）；
5. `Rebuilding → Ready`（成功）或 `Rebuilding → Failed`（失败）；
6. **失败恢复旧 engine**（用旧配置重建会话）：成功 → 配置回到旧值、`outcome = "rolled_back"` 并带 `error`；
   连旧引擎也起不来 → 明确 `Failed`，`reason` 里**两个原因都写**。

执行方式：该序列在**独立线程**里跑、由那个线程写响应（accept 线程立刻回到循环），否则整个服务
（包括 `/api/status` 与 `/api/ocr`）会在它结束前停摆，而"`Rebuilding` 可见""新任务入队"这两条本身就
无法被观测。同时只允许一个切换在跑：第二个请求得到 **503 `busy`**（不排队）。

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

**M4：验收用的是真实工作，而不是脚本化后端**。M1 的公式队列在生产路径上不可达（当时
`queue=formula` 是 400），只能用测试专用慢速后端驱动；M4 接上真实公式管线后，两个方向都由
**真实推理**驱动：真实的文本 OCR（真实照片）与真实的公式 OCR（`pp_formulanet_plus_m.onnx`
+ `pix2text-mfd-1.5.onnx`），上界仍取自 `/api/status` 的 `wait_bound`，并把"洪水队列在窗口内
确实被服务过"（完成的任务数 + 观测到的队列占用）作为断言的一部分。见 M4 记录的验证 3。
调度策略本身（`queue.rs`）**一行未改**：M0c 冻结的取法与可证明上界是这条验收的基础。

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
| 右上（引擎面板下方） | **"重新校验"按钮 + 结论区**（A2）：一次动作触发 `POST /api/models/reverify`（清缓存 → 冷验证 → 重建引擎），结论显示 `computed`/`content_changed`/`outcome`/逐文件状态。放在常驻位置而不是模型横幅里：横幅在模型齐备时是隐藏的，而这个动作在齐备时同样有意义 |
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
8. 公式路由**默认关闭**（避免意外加载 566 MB 模型）。M4 的落地方式：页面上的开关默认关；
   服务端侧"路由是否可用"由 `--formula-detector` 唯一决定（§4.2.1），而**加载**是惰性的——
   只有真的提交了 `queue=formula` 的任务，库才会去建那个 566 MB 的会话；
9. `/api/status` 路径脱敏（§7.4）；
10. **模型文件的校验结论按文件身份缓存**（键 = 路径 + 体积 + mtime + 首尾各 64 KiB 的局部
    摘要）：冷验证真的读盘并如实记账，命中只花一次 `stat` + 一次 128 KiB 局部读（与文件大小
    无关）。`/api/models` 的 `verification` 块把成本报出来，页面 8 s 轮询因此不再反复读
    566 MB（M1 评审 P1-2 / 性能一节；局部摘要是 B 轮加的，实测前后成本见 `docs/06`）。
    **需要确定性地排除缓存影响时**只有两个入口：启动期的 `--reverify-models`（冷验证 +
    缺失/损坏即拒绝启动）与运行期的 `POST /api/models/reverify`（清缓存 → 冷验证 → 重建引擎）。

---

## 11. 里程碑

### M0：冻结协议与安全（先决条件）

- [x] 删除 `--host`；监听地址**硬编码** `127.0.0.1`（无参数/配置/环境变量可改）；启动断言监听地址属 loopback；Host/Origin 允许集合含实际端口（§7.1）
- [x] tombstone 表（容量 + TTL）+ 404/410 区分（§4.5）
- [x] `ModelSet`/`ModelFileSpec`/`ModelRole`/`ModelSetStatus` + **共享逐文件校验函数**（§5.1/5.2）
- [x] `ModelManifest` 通用化（`schema_version` + `files: Vec<ManifestFile>`）+ **单一来源选择规则**（§5.3）
- [x] 字典补 SHA-256；无哈希不得 `complete`（§1.2、§5.2）
- [x] 加固下载器 + `--max-download-mb` + `MoveFileExW` 原子替换 + 迁移 CLI 调用方（§6）
- [x] provider 回退语义 + **启动期配置校验** + 引擎创建期可用性判定 + `/api/status` 三字段（§7.5）
- [x] `ServiceState` / `EngineState` 状态机与全部转换（§7.6）
- [x] `ServeError` 与状态码映射（§11.1，含 `engine_unavailable` / `export_too_large`）
- [x] 准入顺序（§4.4）与 `--max-result-mb`（§4.6）
- [x] 双队列容量与**双向**公平调度参数（`--max-consecutive-text` / `--max-consecutive-formula`，§8.3）

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
| 评估请求不合法（M4：清单读不出来/格式不对/超过 `--max-eval-cases`/图读不出来） | 400 | `bad_request`（`detail.reason` 可定位） |
| 其他 | 500 | `internal` |

### M1：最小闭环

- [x] `serve` 子命令 + feature 隔离 + 未启用 feature 的可定位错误
- [x] provider 启动期解析（§7.5）；`GET /`、`/api/status`、`POST /api/ocr`、`/api/jobs/{id}`、`/result`
- [x] 单图上传（XHR + 进度 + 取消上传）、异步任务与轮询
- [x] 预览与 polygon 叠框、区域列表、复制全文、JSON 导出
- [x] 模型缺失提示（消费 `ModelSetStatus`；**不含下载动作**）
- [x] 测试：状态机、tombstone（容量+TTL）、双队列 503、公平调度、准入顺序、Host/Origin/token、安全头与 nonce CSP

**M1 验收**：真实 12 图经 HTTP 的 `regions` 数量与文本与 `rapidocr run --json` **逐张一致**；空 `--model-dir` 下 `/api/models` 与 `/api/ocr` 的缺失字段一致；公式洪水下普通 OCR 不被饿死、普通洪水下公式也不被饿死；空模型目录下服务仍为 `Ready` 且 `EngineState::BlockedModelsMissing`（OCR 409，字段与 `/api/models` 一致）；CLI 中不存在任何可改变监听地址的选项，也不存在 `--ocr-workers`。

### M2：模型管理

- [x] `GET /api/models`、`POST /api/models/download`、下载任务进度
- [x] 单飞、空间检查、强制 SHA-256、失败清理、`MoveFileExW` 原子替换、重定向逐跳校验（§6.1 第 2 条）
- [x] 下载取消（文件边界）与"目标已损坏时重新下载"（§6.3）
- [x] 模型齐备后惰性创建 engine 并显示耗时

### M3：诊断与导出

- [x] 时间账本、ORT/provider 指纹、内存信息进入诊断面板（含口径说明）
- [x] `annotated.png`、Markdown/HTML 导出（HTML 走 `ReportMode::Static` + 独立 CSP，§9.5）
- [x] provider 运行期切换：暂停新任务 → 排空 → 销毁旧 engine → 创建新 engine → `rebuilding`；失败恢复旧 engine 或明确 `failed`
- [x] 测试：导出 HTML 可用且不含 `<script>`、CSP 头正确

### M4：公式与评估

- [x] 公式模型下载（566 MB，显式点击 + 体积提示）
- [x] 公式 OCR 走独立队列（§8.3）
- [x] 公式区域展示与诊断
- [x] 上传标注样本 → CER / 精确匹配（复用 `evaluation`，不另写指标）

### M1 评审修复轮（P1/P2/P3，逐条证据见 `docs/06`）

M4 交付后的独立评审发现的问题；每条都在本文档的对应章节改成了实现证明的表述（"改哪里"逐条列出）：

- [x] **P1-1 检测模型的完整性**：`formula_detector` 的声明 SHA-256 端到端传递并**在加载路径上校验**
      （与识别模型同一条规则、同一个入口形状）。改动：§4.2.1、§5.2 的实现说明、§5.4 的 `detector` 块
- [x] **P1-2 准入按哈希状态 + 会话缓存按文件身份**：新增库内的身份键控校验缓存
      （`path + size + mtime`），`/api/models`、公式准入与两条公式加载路径共用它；
      识别器/检测器的会话缓存改为按文件身份失效。改动：§3（性能）、§4.2.1、§4.4 第 4 步、§5.4、§10
- [x] **P2-1 队列容量预留是原子的**：判定与占位合成同一个临界区，凭据提交或归还。改动：§4.4 第 4 步
- [x] **P2-2 无 body 的 `POST /api/engine/reload` 走独立线程**：建会话不再发生在 accept 线程上
      （两种形态同一条线程路径）。改动：§4.2 的 `POST /api/engine/reload` 行
- [x] **P2-3 `--eval-root` 沙箱**：不给即拒绝整个评估端点；清单与图片都必须规范化到根内。改动：§3、§4.2、§7.2
- [x] **P2-4 令牌熵为 CSPRNG 且 fail-closed**：`BCryptGenRandom`，失败拒绝启动。改动：§7.2
- [x] **P3 文档状态**：本文件第 3 行的陈旧状态声明与 §13/§15 的陈旧标题已改正（状态行、§13、§15）

**这一轮改动的是实现，也是文档**：上面每一条都不是"实现没做"，而是"M4 的实现/表述在这几点上不完整"，
因此 `docs/05` 相应章节按实现改正；`docs/03` 未改动。

### A1/A2/B：启动期冷验证、运行期"重新校验"、身份加局部摘要（逐条证据见 `docs/06`）

M1 评审修复轮之后的一轮，针对"校验缓存的保证强度"这三件事（同一份记录在 `docs/06` 的
"A1/A2/B"一节）：

- **A1 `--reverify-models`**：启动期**冷验证**这次运行会真正加载的每个文件（忽略缓存），
  逐文件打印一行结论，任一缺失/损坏即**拒绝启动**并点名那些文件。范围是
  `ModelPlan::required_files`，**不是**整张默认表。改动：§3、§5.5、§10 第 10 条、§12
- **A2 `POST /api/models/reverify`**：①清校验缓存 ②冷验证 ③**引擎此前就绪则重建会话**
  （缺第 3 步就是"按下去什么都不会变的按钮"）。响应给逐文件状态/原因、`computed`、
  `content_changed` 与引擎结论（含 `load_ms`/`rolled_back`/`failed`，与 reload 同一套语义）。
  单飞 + 独立线程（与 `POST /api/engine/reload` **同一把**资格，第二个并发调用 503 `busy`）。
  页面新增常驻"重新校验"按钮与结论区。改动：§4.2、§5.5、§9.2、§10、§12
- **B 身份加"首尾各 64 KiB 的局部摘要"**：命中现在要求 stat 身份**与**局部摘要都相同；
  stat 相同而局部摘要不同 = 内容变了 → 作废并重新完整哈希，并如实报告
  `content_changed`/`partial_mismatches`。`residual_blind_spot` 收窄为"同体积 + 同 mtime +
  首尾 64 KiB 逐字节相同"，并明确写出**局部摘要是启发式，不是安全边界**。
  小于 128 KiB 的文件首尾重叠 ⇒ 局部摘要覆盖整个内容（规则与测试见 §4.2.1）。
  改动：§4.2.1、§5.4、§10 第 10 条、§12

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
| 下载 | **重定向逐跳校验（允许白名单内、拒绝越界/超限/降级/改名，最终体仍受上限约束）**、非 https 拒绝、host 白名单拒绝、`Content-Length` 超限拒绝、无长度流式超限拒绝、哈希失败删临时文件、单飞只下一份、磁盘不足 507、**目标已损坏时原子替换成功** |
| Provider | 请求 directml/cuda 但实际回退 → 默认启动即失败；加 `--allow-provider-fallback` 时 `/api/status` 三字段如实反映 |
| 模型集 | 字典缺哈希 → `complete=false`；损坏 → `Corrupt`；manifest 缺 role → 报错列出缺失 role；旧 schema manifest → 可定位错误 |
| 导出 | HTML 导出含 `Content-Disposition: attachment`、导出 CSP、正文无 `<script>` |
| 参数优先级 | CLI `--config` / `--provider` / `--max-side` 三者优先级与启动日志一致 |
| 真实资产 | 12 图 HTTP 与 CLI 逐张一致；公式路径单独验证 |
| 公平性（双向，M4） | 公式洪水下普通 OCR 等待有上界；**普通洪水下公式任务等待有上界**——两个方向都用**真实推理**（真实照片 + 真实公式模型），上界取 `/api/status` 的 `wait_bound` 并证明洪水队列在窗口内确实被服务 |
| 评估（M4） | `POST /api/evaluate` 的报告与 `rapidocr evaluate` **逐字段同值**（逐例 CER + 均值 + 精确匹配率）；超限/坏清单是可定位的 400；同一时刻只允许一个评估（503 `busy`） |
| 公式模型（M4） | `/api/models` 报告公式集合的 role/体积/SHA-256；下载按钮按集合（566 MB 显式点击，绝不自动下载）；剔除公式文件后路由变 409 且普通 OCR 不受影响 |
| 手工 | 浏览器闭环、粘贴、上传进度、标注图、键盘与焦点可用 |
| 性能 | 诊断面板数据与 CLI 报告同值（不重新测量） |
| 完整性（M1 评审 P1-1/P1-2） | 损坏但**存在**的公式检测模型 → 加载是 `HashMismatch`（路径 + 期望 + 实际）；`/api/models` 报 `corrupt`；`queue=formula` 在读 body 前 409 `models_corrupt` 且 `detail.scope="formula"`；有效的检测模型 + 声明摘要仍加载成功 |
| 校验缓存（M1 评审 P1-2 / 性能） | 第二份 `/api/models` 的 `verification.cold_this_call == 0`（命中不重新哈希）；替换文件（体积/mtime 变化）后**只有它**被重新验证；同体积 + mtime 变化同样触发重新验证；公式检测器被替换后**不**复用内存里的旧会话 |
| 队列预留（M1 评审 P2-1） | 容量 1 时 8 个并发请求全部 503 且**都没有读入 body**（声明 1 MiB、一个字节不发）；8 个线程抢同一个槽位只有 1 个成功，释放后容量完好（不泄漏、也不放大） |
| 引擎重建线程（M1 评审 P2-2） | 无 body 的 `POST /api/engine/reload` 在建会话被闸门按住的整段时间里：`/api/status` 显示 `loading` 且 `POST /api/ocr` 仍返回 202 |
| 评估沙箱（M1 评审 P2-3） | 没有 `--eval-root` → 400 且理由点名开关；清单越界 / 图片越界（绝对路径、`..`、符号链接）→ 400 且点名违规路径；根内清单照常返回与 CLI 逐字段同值的报告 |
| 令牌熵（M1 评审 P2-4） | 生成成功且每次不同、是十六进制；熵源字符串被报告；注入一个必然失败的填充器时错误被传播（fail-closed 分支被真的执行） |
| 启动期冷验证（A1） | 损坏/缺失的计划模型 → `rapidocr serve --reverify-models` **非零退出**并点名那个文件（单元层 + 真实子进程的集成用例）；健康模型 → 每个文件**这一次**都算了一次摘要（`digests_computed == 文件数`）；不在本轮使用范围里的文件**不**被哈希 |
| 运行期重新校验（A2） | 损坏模型 → 端点报 `corrupt` 且引擎是**可定位的错误态**（不是"旧引擎继续 ready"）；还原后同一端点恢复 `ready` 并**真的重建会话**（建会话次数 +1）；缓存命中之后调用它 `computed == 文件数`（每个文件的 `cause` 都不是 `cache_hit`）；并发调用一个真跑、另一个立刻 503 `busy`（确定性地用钩子构造重叠），且 `busy` 由 accept 线程产生（序列不在 accept 线程上）；有 body → 400 |
| 局部摘要（B） | 同体积 + 同 mtime：改**头部**、改**尾部**都必须被发现并触发完整重哈希（`cause = content_changed`、`partial_mismatches` 增长）；只改**中段**必须**不**被发现（限制被测试钉住，不是只写在文档里）；`size ≤ 128 KiB` 时首尾重叠 ⇒ 任意位置改动都被发现；空文件与正好 128 KiB 良定义；命中路径不完整哈希（`computed == false`）；`/api/models` 命中成本与加局部摘要前对比（前后实测见 `docs/06`） |

**证据要求**：每个里程碑在 `docs/06-local-web-demo-reports.md`（新建）记录命令、关键输出、与验收标准对照、未覆盖风险。**不得**以"界面看起来正常"作为验证通过。

---

## 13. 参考命令（已实现后的实测命令；`<...>` 是每次运行都要替换的值）

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
| 拒绝前读入大 body | 内存/带宽被消耗 | 准入顺序：**先原子预留队列槽位与检查长度，再读**（§4.4；M1 评审 P2-1 之前"先检查后入队"是两个临界区，并发下会读入 body 才拒绝） |
| 损坏的模型被静默加载 | 结果错/崩溃难定位 | 检测模型与识别模型**同一条**哈希规则（`HashMismatch`）；准入按哈希状态在**读 body 之前** 409（§4.2.1，M1 评审 P1-1/P1-2） |
| 模型文件被替换后继续用旧会话 | 改对了也不生效 | 会话缓存按**文件身份**（路径 + 体积 + mtime）失效（§4.2.1，M1 评审 P1-2） |
| 评估端点读取任意本机路径 | 本机文件的可读范围不受控 | `--eval-root` 沙箱，未配置即拒绝整个端点（§4.2、§7.2，M1 评审 P2-3） |
| 令牌熵可预测 | 本机恶意页面可猜出共享密钥 | 操作系统 CSPRNG + fail-closed（§7.2，M1 评审 P2-4） |
| tombstone 无界增长 | 内存泄漏 | 容量 + TTL 双上限（§4.5） |
| 结果序列化爆内存 | OOM | `--max-result-mb` + 有界写入器（§4.6） |
| Windows 覆盖语义 | 文件损坏/丢失 | `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` + 回归测试（§6.7） |
| provider 静默回退 | 用户误判性能 | 默认强制失败 + 三字段展示（§7.5） |
| 双权威模型来源 | 行为不一致 | 单一来源规则 + 缺 role 报错（§5.3） |
| 公式任务饿死普通 OCR | 体验退化 | 双队列 + 连续上限（§8.3） |
| `serve` 依赖泄漏进默认构建 | 影响发布 | feature 隔离 + `cargo tree`/`package` 验证 |
| HTTP 逻辑渗入库 | 破坏库边界 | §2.1 硬性约束 + review checklist |

---

## 15. 冻结过程中的待确认问题（**已定案**，取值即 §3 的默认值）

以下问题在 M0 冻结时以 §3 表格里的默认值定案，并已随实现生效（唯一来源是
`src/bin/serve/limits.rs` 的 `DEFAULT_*` 常量，选项面由 `cli.rs` 的逐项枚举测试锁定）。
保留这一节是为了让"当时的取舍"可追溯，而**不是**表示它们仍未决：

1. 默认端口 `8760` → 采用（占用时报可定位错误，不静默换端口）。
2. 默认模型集 → 由 `--config` 的 `model_type`/`lang` 选择，默认表提供 `PP-OCRv6`。
3. `--max-download-mb` 默认 1024 MB → 采用（公式模型 566 MB + 普通模型约 40 MB + 余量）。
4. 结果上限默认 8 MB / 保留 32 个 64 MB → 采用。
5. 队列默认（text 4 / formula 2）与配额（text 连续 4 / formula 连续 1）→ 采用。
6. `--max-export-mb` 默认 32 MB → 采用。

---

## 16. 参考资料

- MDN：[Using Fetch](https://developer.mozilla.org/en-US/docs/Web/API/Fetch_API/Using_Fetch) · [XMLHttpRequest.upload](https://developer.mozilla.org/en-US/docs/Web/API/XMLHttpRequest/upload) · [AbortController](https://developer.mozilla.org/en-US/docs/Web/API/AbortController)
- [`tiny_http::Server`](https://docs.rs/tiny_http/latest/tiny_http/struct.Server.html)（`recv_timeout`、`unblock`）
- OWASP：[SSRF Prevention Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Server_Side_Request_Forgery_Prevention_Cheat_Sheet.html)
- WCAG 2.2：[Dragging Movements](https://www.w3.org/WAI/WCAG22/Understanding/dragging-movements) · [Focus Visible](https://www.w3.org/WAI/WCAG22/Understanding/focus-visible) · [Focus Not Obscured](https://www.w3.org/WAI/WCAG22/Understanding/focus-not-obscured-enhanced.html)


---

## 实施完成记录（M0-M4）

上面的 M0-M4 清单已全部勾选，每项的证据在 docs/06-local-web-demo-reports.md 对应里程碑记录里（命令、结果、前后行为对比、未覆盖风险）。提交序列：

| 里程碑 | 提交 |
| --- | --- |
| M0a 库内前置（ModelSet / 单一来源 / 30 个字典 SHA-256） | 1144ddb |
| M0c serve 核心纯逻辑（状态机 / ServeError / job+tombstone / 双队列 / 准入 / loopback 安全 / CLI 面） | 1144ddb |
| M0b 加固下载器（download_verified / DownloadError / 预算 / 调用方迁移） | dbab12e |
| M1 serve HTTP 层 + 内联页面接入 | 06bd0af |
| M2 模型管理（真实下载 / 进度 / 文件边界取消 / host opt-in / 惰性建引擎） | 8532fd8 |
| M2b 下载重定向逐跳校验（解 M2 阻塞） | dcf8583 |
| M3 诊断与导出（annotated.png / 三格式 / 时间账本 / provider 切换） | c2f35d6 |
| M4 公式集 / 真实第二队列 / 公式区域 / 评估 | ef9c005 |

### 三处如实保留的缺口（勾选不等于全部验证过）

1. 公式模型 566 MB 的真实下载没有重跑：/api/models 的大小、哈希与来源，以及页面上的下载按钮（含体积提示）都验证过，但该文件本机已存在；同一套与集合无关的下载路径已在 M2b 用真实网络验证（v6-tiny 权重集 3/3 文件、三个 SHA-256 全部匹配）。
2. 评估以 manifest 路径而非浏览器上传：POST /api/evaluate 接收 manifest 路径，因为设计明确不引入 multipart；数值与 CLI 逐位一致（12 张标注图 mean CER 0.44765135645866394）。
3. 没有真实浏览器手工点击闭环：交互只在数据层，以及"在 node 里直接运行页面自身的 modelsReadyFor / renderBanner"层面验证过。

其他已记录的未覆盖风险见各里程碑记录（真实加速器上的 provider 切换成功路径、original_evicted 只能由单任务超预算触发、M2 的目标损坏重下只在库层验证等）。
