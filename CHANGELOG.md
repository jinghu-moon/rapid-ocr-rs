# 变更记录（开发期）

本 crate 处于开发期，**不承诺向后兼容**。这里只记录结构性的、会影响下游的变更。

## Windows-only 收窄（未发布）

破坏性平台收窄，对应 `docs/03-windows-only-optimization-tasks.md` 的阶段 0–8。

### 平台

- **只支持 `x86_64-pc-windows-msvc`**。非 Windows 目标在编译期直接命中
  `src/platform_gate.rs` 的 `compile_error!`；ARM64 / GNU ABI / Wine / WSL / Linux / macOS
  是明确非目标。
- 峰值内存只保留 Windows PSAPI 口径
  （`windows:GetProcessMemoryInfo.PeakWorkingSetSize`），Linux `/proc` 实现已删除。

### 删除的公开 API

| 删除项 | 原因 |
| --- | --- |
| `RuntimeBackend` 与 `RuntimeConfig::backend` | 只有一个取值（`OnnxCpu`）的伪抽象，却要序列化并出现在 YAML 里 |
| `ProviderPreference::Cann` / `ResolvedProvider::Cann` / `cann-provider` feature | CANN 不是 Windows 目标 |
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

## 已知限制

见 README 的 *Known limitations* 一节（检测器误检、抹白导致的文本重新分割、
公式后处理不是 RapidDoc 的完整等价实现、CUDA 在本机未验证等）。
