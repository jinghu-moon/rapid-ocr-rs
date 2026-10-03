# 变更记录（开发期）

本 crate 处于开发期，**不承诺向后兼容**。这里只记录结构性的、会影响下游的变更。

## 平台收窄：Windows x64 + MSVC ABI（未发布）

破坏性平台收窄，对应 `docs/03-windows-only-optimization-tasks.md` 的阶段 0–8
（任务文档的文件名保留首轮的写法）。

### 平台

- **唯一支持的平台是 Windows x64 + MSVC ABI（`x86_64-pc-windows-msvc`）**。
  其他目标在编译期直接命中 `src/platform_gate.rs` 的 `compile_error!`。
- **明确非目标**：Windows x86（32 位，`i686-pc-windows-msvc` / `i686-pc-windows-gnu`）、
  Windows ARM64（`aarch64-pc-windows-msvc`）、Windows GNU ABI（`x86_64-pc-windows-gnu`），
  以及 Wine、WSL、Linux、macOS。
- 32 位 x86 不是“改一个 `cfg`”就能支持：预编译的 ONNX Runtime、DirectML/CUDA 的
  provider DLL 与约 566 MB 的公式识别模型都是 x64 产物，32 位地址空间对后者本身就是
  真实限制；Windows ARM64 需要重新收集并重测整套原生依赖。
- 峰值内存只保留 Windows PSAPI 口径
  （`windows:GetProcessMemoryInfo.PeakWorkingSetSize`），Linux `/proc` 实现已删除。

### 删除的公开 API

| 删除项 | 原因 |
| --- | --- |
| `RuntimeBackend` 与 `RuntimeConfig::backend` | 只有一个取值（`OnnxCpu`）的伪抽象，却要序列化并出现在 YAML 里 |
| `ProviderPreference::Cann` / `ResolvedProvider::Cann` / `cann-provider` feature | CANN 不是 Windows x64 目标 |
| `VisionBackend` 与 `RuntimeConfig::vision_backend` | 视觉后端分派只服务 OpenCV，而 OpenCV 无法在本机构建与测量 |
| `opencv-backend` feature 与 `opencv` 依赖 | 同上：没有测量依据支持保留，且让 `--all-features` 不可用 |
| `turbojpeg` 依赖与 CMake 原生构建链 | 实测比 `image` crate 的 JPEG 解码**慢 7%–33%**，端到端差值 0.036% |
| `det.runtime` / `cls.runtime` / `rec.runtime` 三段重复配置 | 合并为单一 `EngineConfig.runtime`；残留的旧键会被 `deny_unknown_fields` 拒绝 |

### 新增

- `RuntimeProfile` / `ThreadPlan` / `ThreadSource`（`runtime::profile`）：
  统一管理 provider、ORT intra/inter、Rayon 线程、arena、严格回退与公式批大小，
  并在 benchmark 报告中输出 `meta.thread_plan`；Rayon 全局池初始化失败不再是静默失败。
- `RuntimeConfig::formula_batch`（默认 16）：公式识别器按此分块，
  修掉了“17–64 个公式区域导致整页请求失败”的缺陷。
- `ort_runtime_version()`：报告实际加载的 ONNX Runtime 版本。
- `PEAK_MEMORY_SOURCE` / `peak_memory_failure_reason()`。
- `tools/check_platform_gate.ps1`、`tools/run_windows_baseline.ps1`、
  `tools/run_windows_provider_matrix.ps1`、`tools/run_thread_matrix.ps1`。

### 单变体抽象清理

- `RuntimeBackend` 删除后，`runtime/session.rs` 直接构造 ORT Session。
- 视觉入口不再接受 `backend` 参数；热路径上没有 enum 分派。
- 删除 `resolve_backend_or_pure_rust` 这条宽松回退：核心路径没有“静默回退”概念。

### 线程策略与计时账本（终审第二轮）

- **线程策略只剩一份实现**：`RuntimeConfig::effective_session_threads()`（显式值优先；
  否则按 `auto_tune_threads` 从 `runtime::profile::auto_tuned_thread_budget()` 推导；
  否则 `(None, None)` = 不配置）。`RuntimeProfile::plan()` 与 `OrtSession::open_session()`
  都调用它，因此引擎、`FormulaSession`、`formula_bench`、`formula_eval` 的线程行为
  由构造保证一致。此前 `OrtSession` 只做字段透传，默认配置下公式路径拿到的是 ORT 默认
  线程数，而引擎路径用推导值。
- `RuntimeConfig::has_explicit_thread_request()`：`intra_threads` / `inter_threads` /
  `rayon_threads` 任意一个 `Some(>0)` 即算显式请求；`ThreadSource::Explicit` 由它决定，
  因此只显式设置 `rayon_threads` 时，“已有全局池大小不同”也会报错而不是被静默采纳。
- 新增 `OrtSession::session_threads()`、`FormulaSession::session_threads()`、
  `FormulaRecognizer::session_threads()`：报告实际下发给 ONNX Runtime 的
  `(intra, inter)`（`None` = 未配置）。
- `formula_bench` / `formula_eval` 报告新增 `effective_intra_threads` /
  `effective_inter_threads`（前者在 `threads` 下，后者在 `provider` 下）。
- `LedgerConservation` 新增 `overlap_ms`（= `max(0, -residual_ms)`，占比成立的上界）与
  `interpretation`（进入 JSON 的读法说明）。时间账本是**诊断**工具：负残差表示计时窗口
  重叠（外层 `preprocess_ms` 与阶段计时跨越 `inner.run()`），**不是** `total_ms` 算错了，
  也不能作为性能验收依据。

## 已知限制

见 README 的 *Known limitations* 一节（检测器误检、抹白导致的文本重新分割、
公式后处理不是 RapidDoc 的完整等价实现、CUDA 在本机未验证等）。
