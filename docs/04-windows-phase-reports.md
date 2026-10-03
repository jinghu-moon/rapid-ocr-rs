# rapid-ocr-rs Windows-only 阶段执行记录

> 本文件按 `docs/03-windows-only-optimization-tasks.md` §13 的模板，逐阶段追加
> 执行记录。原始数据保存在 `tests/baseline/windows-baseline/`（随仓库提交）。

---

## 阶段 0：建立 Windows 基线并冻结决策

**阶段**：0
**日期**：2026-10-03
**提交**：`（本阶段提交）`
**变更摘要**：只增加测量工具，不改动 OCR/公式行为。

- 新增 `tools/run_windows_baseline.ps1`：采集环境、构建统计、12 图 warm 基准、
  12 图质量评估。
- 新增 `tools/run_windows_provider_matrix.ps1`：CPU / DirectML / CUDA 的可用性、
  fallback 语义与严格模式错误文本。
- `bench_warm_e2e` 增加 `meta.init_ms`、`meta.provider_resolution` 与
  `memory.peak_working_set_bytes`（口径与库内 `runtime::memory` 一致，
  都是 `GetProcessMemoryInfo.PeakWorkingSetSize`）。
- `rapidocr evaluate` 在报告中附加 `peak_working_set_bytes` / `memory_source`
  （由 CLI 附加，纯算法报告类型保持不变）。

### 环境

| 项目 | 值 |
| --- | --- |
| target | `x86_64-pc-windows-msvc`（本阶段冻结） |
| 明确非目标 | `aarch64-pc-windows-msvc`、`x86_64-pc-windows-gnu`、Wine、WSL、Linux、macOS |
| OS | Microsoft Windows 11 IoT 企业版 LTSC 10.0.26100（Build 26100，x64） |
| CPU | 13th Gen Intel Core i5-13600KF，14 物理核 / 20 逻辑核，3.5 GHz |
| GPU | NVIDIA GeForce RTX 4070 Ti SUPER，驱动 32.0.15.9186（591.86） |
| 工具链 | rustc 1.98.1 (48a229cea 2026-09-01)，cargo 1.98.1 |
| ort crate | `=2.0.0-rc.13` |
| 实际加载的 ORT 运行库 | `C:\Windows\system32\onnxruntime.dll`，10,572,960 字节，版本 `1.17.260311-1434.1.os-germanium`（Microsoft Windows 内置） |

### 12 图普通 OCR 基线（release，`test-config-small.yaml`，warmup 1 / rounds 3）

| max_side_len | init (ms) | OCR p50 (ms) | OCR p90 (ms) | OCR avg (ms) | 区域数均值 | 峰值工作集 (MB) |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 2000（质量基线） | 143.6 | 1001.1 | 1153.6 | 821.0 | 34.8 | 1284.0 |
| 1280（低延迟） | 149.0 | 664.3 | 753.5 | 644.5 | 33.4 | 900.6 |

12 图质量（golden manifest，`boxes` 为空故检测指标为 `null`）：
**mean CER = 0.4477，exact match = 0.0，峰值工作集 1162.8 MB**。
该 CER 与 README 已记录的 2000 侧基线（0.4477）一致，说明基线可复跑。

### 构建与体积

| 项目 | 值 |
| --- | --- |
| crate 自身 release 重编译（`cargo clean -p rapid-ocr-rs --release` 后 `build --bins`） | 18.1 s |
| `rapidocr.exe` | 34,607,616 B |
| `bench_warm_e2e.exe` | 34,359,808 B |
| `formula_eval.exe` | 29,712,384 B |
| `formula_bench.exe` | 29,022,208 B |
| `target/` 总体积 | 18,596,612,855 B（≈18.6 GB） |

### Provider 可用性（12 图，max_side_len 2000，intra_threads 16）

| provider | 解析结果 | fallback | OCR p50 (ms) | 严格模式 | 结论 |
| --- | --- | --- | ---: | --- | --- |
| CPU | `Cpu` | false | 1013.5 | exit 0 | 可用，基线 |
| DirectML | `DirectMl` | false | **508.7** | exit 0 | 可用，约为 CPU 的 2.0× |
| CUDA | `Cuda` | false | **1013.5** | exit 0 | **报告为已解析，但耗时与 CPU 完全相同 → 实际未加速** |

**P1 发现（阶段 2 必须处理）**：CUDA 的 p50 与 CPU 逐位相同（1013.5 ms），
而同一 workload 上 DirectML 快 2 倍，说明 CUDA EP 并没有真正执行模型。
根因是本机加载的是 Windows 内置 ORT 1.17（仅 CPU/DirectML 方向），
`System32` 下不存在 `onnxruntime_providers_cuda.dll`；
`cuda-provider` 构建配合 `download-binaries`/`copy-dylibs` 在本机没有下载到任何运行库，
只在 `target/release` 留下 5 个**零字节**占位 DLL（已删除）。

因此当前 API 会给出 `resolved = Cuda, fallback_used = false` 的“成功”结论，
违反任务文档「不得把 CPU fallback 当加速成功」。阶段 2 必须让 provider 报告诚实：
要么能观测到 ORT 的逐节点回退，要么明确区分“EP 已注册/自报可用”与“实测加速”。

### 硬门槛与观察值（阶段 0 冻结，优化后不得临时修改）

**硬门槛（不得退化）**：

- 12 图 mean CER ≤ 0.4477（2000 侧）与 ≤ 0.4355（1280 侧，README 已记录）；
- 12 图区域数均值（2000 侧 34.8）；
- 公式 im2latex-100：exact 24.00% / normalized 25.00% / mean CER 0.0863 /
  链路失败 0（manifest `271424c18c000f95`，模型 SHA-256 `71b6d389…d9493b`）；
- 公式链路对比（val-501）：501/501 token·EOS·LaTeX 一致，`link_differences = 0`；
- 无新增单图硬失败、provider fallback 或输入限制回归。

**观察值（用于判断优化是否值得，不作为门槛）**：

- init 时间、P50/P90、峰值工作集、二进制体积、编译耗时、`target/` 体积。

### 未覆盖风险

- CUDA 在本机**无法验证**：无可用的 CUDA 版 ORT。任何 CUDA 性能结论都不得成立，
  直到提供 CUDA 版 `onnxruntime.dll` + `onnxruntime_providers_cuda.dll` 后重测。
- DirectML 只在一台机器（RTX 4070 Ti SUPER / 591.86 驱动）上测得，
  README 必须继续声明“不得假设 GPU 一定更快”。
- 12 图 golden manifest 的 `boxes` 为空 → 检测 precision/recall/IoU 无法作为门槛，
  本阶段只能把区域数作为代理指标。
- 普通 OCR 的 CER 绝对值较高（0.4477），因为 golden 文本来自原型页面的可见文本，
  包含大量符号与混排；该值只用于**同集合回归对比**，不代表生产质量。

### 是否触发公式 smoke / val-501 / 全量评测

- 公式 smoke（im2latex-100）：**复用**当前提交已锁定的结果（本阶段未改动模型调用链）。
- val-501 / 全量：未触发；理由同上。

---

## 阶段 1：建立 Windows-only 编译边界

**阶段**：1
**日期**：2026-10-03
**提交**：`（本阶段提交）`
**变更摘要**：

- 新增 `src/platform_gate.rs`：平台门槛的**唯一定义处**，无任何依赖，
  因此在非 Windows target 上只会产生这一条 `compile_error!`。
- `src/lib.rs` 改为：`#[path]` 引入门槛 + 每个模块都带
  `#[cfg(all(windows, target_arch = "x86_64"))]`；公开 API 收拢到新的
  `src/exports.rs`，于是平台谓词只需在 `lib.rs` 写一次，而不是在十几个 `pub use`
  上重复。
- `src/runtime/memory.rs` 删除 Linux `/proc/self/status` 与其它平台的 `None` 分支，
  只保留 Windows PSAPI；新增 `PEAK_MEMORY_SOURCE`、`peak_memory_failure_reason()`
  （失败时带 `GetLastError`），测试改为“必须拿到正值，否则报告可定位的 Win32 原因”。
- `src/runtime/provider.rs` 去掉 `target_os = "windows"` 谓词（crate 已经只编译
  Windows）；`src/bin/formula_bench.rs` 去掉 Linux `/proc` 口径描述，改为报告真实的
  Win32 失败原因。
- `src/evaluation/formula/sampling.rs` 的符号链接用例：把“权限导致跳过”改成显式的
  `ENVIRONMENT:` 说明，并注明同一条不变式由**无条件执行**的 `..` 越界用例覆盖。
- README 新增 “Platform support” 一节（Windows x64 only + 非目标 + 门槛可验证），
  删除 CANN 与 Linux 内存口径措辞。

**执行命令**：

```powershell
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --features directml-provider
cargo test --features cuda-provider
pwsh -NoProfile -File tools/check_platform_gate.ps1
```

**关键结果**：

| 检查 | 结果 |
| --- | --- |
| `cargo test --all-targets` | lib 256 + bin 18 passed / 0 failed |
| `--features directml-provider` | lib 259 passed / 0 failed |
| `--features cuda-provider` | lib 257 passed / 0 failed |
| fmt / clippy `-D warnings` | 通过 |
| `check_platform_gate.ps1`（`x86_64-linux-android`） | 命中自定义 `compile_error!`，**诊断数 = 1**（没有额外错误泄漏） |
| `check_platform_gate.ps1`（本机 target） | 门槛干净通过 |

**基线对比**（12 图，max_side_len 2000，intra_threads 16）：

| 指标 | 阶段 0 基线 | 阶段 1 后 | 判定 |
| --- | ---: | ---: | --- |
| mean CER（硬门槛） | 0.4477 | 0.4477 | 未退化 |
| 区域数均值（硬门槛） | 34.8 | 34.8 | 未退化 |
| OCR p50 (ms) | 1001.1 | 1009.8 | +0.9%，噪声范围 |
| OCR p90 (ms) | 1153.6 | 1187.4 | +2.9%，噪声范围 |
| init (ms) | 143.6 | 140.7 | 未退化 |
| 峰值工作集 (MB) | 1284.0 | 1284.0 | 未退化 |

**未覆盖风险**：

- 交叉编译检查用的是已安装的非 Windows target `x86_64-linux-android`（本机
  rustup 未安装 `x86_64-unknown-linux-gnu`）。Windows ARM64 与 GNU ABI 没有对应
  target 可验证，只能靠同一个谓词推出结论；谓词已在 `platform_gate.rs` 集中，
  将来装好 target 可以直接复跑同一个脚本。
- 本次未安装任何新 target，因此“非 Windows 失败”的证据来自门槛文件本身，
  而不是整 crate 的交叉编译（依赖里的 ort/turbojpeg 需要目标平台原生工具链）。

**是否触发公式 smoke / val-501 / 全量评测**：均未触发。本阶段只改平台边界与
内存采集，不触及模型调用、预处理、tokenizer、postprocess、batch/EOS 或指标实现。

---

## 阶段 2：裁剪 Provider 与 ONNX Runtime 运行时

**阶段**：2
**日期**：2026-10-03
**提交**：`（本阶段提交）`
**变更摘要**：

- **删除 CANN**：`cann-provider` feature、`config::ProviderPreference::Cann`、
  `api::ProviderPreference::Cann`、`api::ResolvedProvider::Cann`、provider 解析分支、
  pipeline 映射与全部 CANN 测试。`cargo check --features cann-provider` 现在给出
  `the package 'rapid-ocr-rs' does not contain this feature: cann-provider`。
- **删除 `RuntimeBackend`**：单变体枚举（只有 `OnnxCpu`）连同 `RuntimeConfig.backend`
  字段与 `runtime/session.rs` 里的检查一起删除 —— 它承载不了任何选择，却要序列化、
  要出现在 YAML 里。provider 选择只由 `provider_preference` 表达。
  YAML 中残留的 `backend: onnx_cpu` 现在被 `deny_unknown_fields` 拒绝，并列出可接受字段。
- **统一 provider 错误文本**（三类可区分）：feature 未编译进来 / 运行库不可用 /
  严格模式拒绝回退。顺带修掉了一条与本机事实矛盾的文案：未启用 feature 时不再报
  “DirectML is only available on Windows”。
- **新增 `ort_runtime_version()`**（查询 `OrtGetApiBase()->GetVersionString()`），并写入
  benchmark 报告：本 crate 链接导入库，运行时实际加载哪个 ONNX Runtime 决定了哪些 EP 可用。
- 移除外部配置文件 `OCR-Model/test-config{,-small,-tiny}.yaml` 中已删除的 `backend:` 行
  （否则引擎构造会被新校验拒绝）。

**执行命令**：

```powershell
cargo test --all-targets
cargo test --features directml-provider
cargo test --features cuda-provider
cargo check --features directml-provider,cuda-provider
cargo check --features cann-provider   # 预期并实测：明确报“没有该 feature”
cargo clippy --all-targets -- -D warnings
```

**关键结果**：

| 检查 | 结果 |
| --- | --- |
| `cargo test --all-targets` | lib 263 + bin 18 passed / 0 failed（阶段 1 为 256） |
| `--features directml-provider` | lib 263 passed |
| `--features cuda-provider` | lib 261 passed |
| `--features directml-provider,cuda-provider` | 编译通过 |
| `--features cann-provider` | 明确失败：`does not contain this feature` |
| 删除字段的 YAML | 明确失败：`unknown field 'backend', expected one of …` |
| fmt / clippy | 通过 |

**CUDA 报告不诚实的根因（阶段 0 发现，本阶段定性）**：

| 现象 | 实测 |
| --- | --- |
| CPU p50 | 1039.5 ms |
| DirectML p50 | **508.7 ms**（约 2×；`DirectML.dll` 已加载） |
| CUDA p50 | **1034.2 ms**（与 CPU 相同，**即使 `onnxruntime_providers_cuda.dll` 就在 exe 旁**） |
| 加载的 ONNX Runtime | `ort_runtime_version()` = **1.28.0** |
| CUDA 工具链 | 已安装（`cudart64_12.dll`、`cublas64_12.dll` 在 PATH 上） |
| **cuDNN** | **缺失**（`cudnn*.dll` 不存在） |

结论：CUDA EP 的 provider 库能被加载、`is_available()` 返回 true，但缺少 cuDNN 时
它无法真正执行模型，节点全部落在 CPU —— 而 `resolved = Cuda, fallback_used = false`
会让调用方以为加速已生效。
因此 crate 明确写入规则：**加速结论必须来自实测 P50/P90，不得仅凭 `resolved` 宣称加速**；
在装上与 ORT 版本匹配的 cuDNN 之前，本机 CUDA 记为**未验证**，不作为可用 provider 声明。
报告字段同时记录 provider 解析结果、实测耗时与 ORT 版本，便于审查时一眼看出矛盾。

**基线对比**（12 图，max_side_len 2000，intra_threads 16）：

| 指标 | 阶段 0 基线 | 阶段 2 后 | 判定 |
| --- | ---: | ---: | --- |
| mean CER（硬门槛） | 0.4477 | 0.4477 | 未退化 |
| 区域数均值（硬门槛） | 34.8 | 34.8 | 未退化 |
| OCR p50 (ms) | 1001.1 | 1037.2 | +3.6%，噪声范围 |
| OCR p90 (ms) | 1153.6 | 1135.3 | 未退化 |
| init (ms) | 143.6 | 140.9 | 未退化 |
| 峰值工作集 (MB) | 1284.0 | 1285.0 | 未退化 |

**未覆盖风险**：

- CUDA 在本机仍**无法验证**：缺 cuDNN。装上与 ORT 1.28 匹配的 cuDNN 后必须重跑
  本阶段的 provider 矩阵，才能改变“未验证”的结论。
- DirectML 只在一台机器上验证；`resolved = DirectMl` + 实测 2× 收益目前一致，
  但仍需在目标机器上复测（README 保留“不得假设 GPU 更快”的说明）。
- `is_available()` 与实际执行之间的落差是 ORT 的行为，本 crate 无法在 API 层消除，
  只能通过文档规则 + 报告字段暴露；阶段 6 会加入“声称加速但实测与 CPU 无差异”的检查工具。

**是否触发公式 smoke / val-501 / 全量评测**：未触发。本阶段只改 provider 枚举、
runtime 配置形状与错误文本，不触及模型调用、预处理、tokenizer、postprocess、batch/EOS 或指标实现。

---

## 阶段 3：OpenCV 与 turbojpeg 的实测决策

**阶段**：3
**日期**：2026-10-03
**提交**：`（与阶段 4 同一提交）`
**变更摘要**：只做决策与测量，删除动作在阶段 4。

### 3.1 OpenCV：删除

**证据（本机实测，不是“依赖少所以删”）**：

| 项目 | 结果 |
| --- | --- |
| `cargo check --features opencv-backend` | **失败**：`failed to run custom build command for opencv v0.94.4`（找不到 OpenCV 安装，`CMAKE_PREFIX_PATH`/`OPENCV_CMAKE_NAME` 均未设置） |
| 默认路径 | 纯 Rust（`VisionBackend::default()` 在未启用 feature 时就是 `PureRust`） |
| 调用方 | crate 内没有任何地方启用它；`opencv-backend` 只出现在 Cargo.toml、vision 分派代码与 README 说明里 |

**判定**：无法在本机构建、因而**无法测量**其端到端收益。任务文档 §0.2 要求“性能优化必须有测量依据”，
§0.3 要求进验证矩阵的东西必须可验证 —— 一个既不能构建也不能测量的后端不能作为当前支持面保留。
因此删除 `opencv-backend`、`VisionBackend`、`vision/backend.rs` 与全部分派分支。
README 同步删除“`--all-features` 需要 OpenCV 安装”的说明。

> 这不是“因为依赖多所以删”，而是“没有任何测量支持保留，且它让 `--all-features` 在本机不可用”。

### 3.2 turbojpeg：删除

对 12 张真实页面（源 PNG 为 3200×2000）生成 q90 JPEG 派生集（4:2:0 与 4:4:4），
在**独立探针程序**里对 `image`（zune-jpeg）与 turbojpeg 做**交错 A/B** 解码对比，
两条路径的调用方式与 crate 内完全一致（含 crate 实际使用的 BGR 输出路径），
两解码器输出每通道最大差异 3–5/255：

| 配置 | image | turbojpeg | 加速比 |
| --- | ---: | ---: | ---: |
| side=2000 4:2:0（RGB，N=40） | 4.839 ms | 5.212 ms | **0.928×** |
| side=2000 4:2:0（**BGR，crate 实际调用**） | 5.586 ms | 6.930 ms | **0.806×** |
| 3200×2000 4:2:0（RGB） | 10.586 ms | 11.612 ms | **0.912×** |
| 3200×2000 4:2:0（**BGR**） | 10.755 ms | 14.276 ms | **0.753×** |
| 3200×2000 4:4:4 q90（RGB） | 13.345 ms | 17.268 ms | **0.773×** |

turbojpeg **在所有配置下都更慢**（7%–33%），12 个文件中只赢 1 个（1%）。
峰值内存也没有优势（解码增量约 18.0 vs 18.5 MiB）。
解码在端到端中的占比：side=2000 时 4.84 ms ≈ 1030 ms 页面的 **0.47%**，
两条路径的**差值是 0.37 ms = 0.036%**；即使不降采样（3200×2000）也只有 0.10%。

**判定**：删除门槛要求“端到端收益低于 10% 则删除”，实测收益为**负值**（−0.036% 到 −0.10%），
且“只改善单独 decode 而不改善端到端”的例外条款也不适用 —— 它连单独 decode 都更慢。
因此删除 turbojpeg 依赖与 CMake 原生构建链。

**测量注意事项（必须随结论一起记录）**：本机（i5-13600KF 混合 P/E 核）绝对耗时波动可达 25%，
两次**完全相同**的基线跑出 p50 726 ms 与 1011 ms；因此结论建立在**交错 A/B 的比值**
（4 次运行稳定在 0.906–0.920）与**逐图 CER 逐位一致**上，而不是单次绝对值。

---

## 阶段 4：视觉与输入路径重构

**阶段**：4
**日期**：2026-10-03
**提交**：`（本阶段提交）`
**变更摘要**：执行阶段 3 的删除决策，并收敛视觉/输入边界。

- 删除 `src/vision/backend.rs` 与 `mod backend`；`VisionBackend` 从 `config.rs`、`exports.rs`
  与公开 API 消失。
- 删除 `image_backend.rs` / `rotate_crop.rs` / `resize.rs` / `det/postprocess` /
  `det/preprocess.rs` / `cls/preprocess.rs` / `rec/preprocess.rs` / `rec/word_boxes.rs` 中
  的全部 OpenCV 分支与 `#[cfg(feature = "opencv-backend")]`；保留的正是原先
  `not(feature = "opencv-backend")` 的行为。
- 视觉入口不再接受 `backend` 参数：`image_backend::{resize_image, rotate_180_image}`、
  `rotate_crop::rotate_crop_image`、`image_ops::{resize_image_within_bounds,
  crop_text_regions, map_img_to_original, resize_with_bound}`、
  `cls/rec::preprocess::write_resize_norm_img_into_slice*`、`word_boxes::compute_word_boxes`、
  `rapid_ocr::prepare_image`。`DetPreProcess` / `DbPostProcess` / `Classifier` / `Recognizer`
  去掉 backend 字段。热路径上不再有“每个 crop/resize 动态匹配 enum”。
- 删除 `resolve_backend_or_pure_rust` 这条宽松回退：核心路径只有纯 Rust，没有“静默回退”概念。
- turbojpeg：`image_loader.rs` 删除 import、`decode_bytes_with_turbojpeg`、`looks_like_jpeg`
  与“orientation==1 时先试 turbojpeg”的分支；保留编码字节上限、header 像素探测、
  EXIF 转置、解码错误语义及全部相关测试。
- 顺带把**标识符里残留 `opencv` 的纯 Rust 辅助函数**改名（`lu_solve_8x8`、
  `sklansky_*`、`convex_hull_*`、`unclip_polygon_like_opencv_db` 等），
  它们与 OpenCV 的数值一致性要求改写进注释，不再有引用非依赖库的名字。
- `Cargo.toml`：删除 `opencv`、`turbojpeg` 依赖与 `opencv-backend` feature；
  `[lints.rust]` 的原因说明收窄为只涉及 `ort`。
- `Cargo.lock` 减少 322 行，`opencv`/`turbojpeg`/`cmake`/`clang` 条目全部消失。
- 外部配置 `OCR-Model/test-config{,-small,-tiny}.yaml` 删除 9 行 `vision_backend: pure_rust`
  （现在会被 `deny_unknown_fields` 拒绝）。

**执行命令与关键结果**：

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --all-targets -- -D warnings` | 通过（0 warning） |
| `cargo test --all-targets` | lib 258 + bin 18 passed / 0 failed（阶段 2 为 263；减少的是仅覆盖 OpenCV 的对比测试） |
| `cargo test --features directml-provider` | 通过 |
| `cargo check --features directml-provider,cuda-provider` | 通过，0 warning |
| `cargo build --release --bins` | 通过 |

**基线对比**（12 图，max_side_len 2000，intra_threads 16）：

| 指标 | 阶段 0 基线 | 阶段 4 后 | 判定 |
| --- | ---: | ---: | --- |
| mean CER（硬门槛） | 0.44765135645866394 | **0.44765135645866394**（逐位相同） | 未退化 |
| 区域数均值（硬门槛） | 34.833333 | **34.833333** | 未退化 |
| OCR p50 (ms) | 1001.1 | 997.6 | 未退化 |
| OCR p90 (ms) | 1153.6 | 1109.5 | 未退化 |
| init (ms) | 143.6 | 127.0 | 略优 |
| 峰值工作集 (MB) | 1284.0 | 1283.0 | 未退化 |
| `rapidocr.exe` | 34.6 MB | 33.3 MB | −3.7% |
| `bench_warm_e2e.exe` | 34.4 MB | 33.1 MB | −3.7% |

**未覆盖风险**：

- 删除了 OpenCV 与纯 Rust 的数值一致性对比测试（它们只在 `opencv-backend` 下编译，
  本机无法运行）。原有的纯 Rust 实现本身未改动，且 12 图 CER 逐位不变；
  但**“纯 Rust 与 OpenCV 数值一致”这一历史结论不再由测试守护**，将来若要重新引入
  OpenCV，必须重建这些对比测试。
- 逐图 CER 逐位一致说明这次重构没有改变数值行为；但本机耗时噪声大（同一二进制两次
  p50 可差 39%），因此“耗时未退化”只能作为量级判断，不能当成精确收益。
- 大图输入的 scratch/buffer 复用仍是阶段 6 的范围（本阶段只删分派，未改缓冲策略）。

**是否触发公式 smoke / val-501 / 全量评测**：未触发。本阶段不触及公式链路、
tokenizer、postprocess、batch/EOS 或指标实现；普通 OCR 的预处理数值结果逐位未变。

---

## 阶段完成记录模板（后续阶段沿用）

```text
阶段：
日期：
提交：
变更摘要：
执行命令：
关键结果：
基线对比：
未覆盖风险：
是否触发公式 smoke / val-501 / 全量评测：
```
