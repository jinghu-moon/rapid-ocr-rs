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

## 阶段 5：统一 RuntimeProfile 与线程模型

**阶段**：5
**日期**：2026-10-03
**提交**：`（本阶段提交）`
**变更摘要**：

- **测量先行**：`tools/run_thread_matrix.ps1` 在改动前跑了 5 组 intra/rayon 组合
  （12 图真实页面，max_side_len 2000，写入 `tests/baseline/windows-baseline/thread-matrix.json`）：

| intra / rayon | 16 / 16 | 16 / 4 | 8 / 4 | 8 / 8 | 4 / 8 |
| --- | ---: | ---: | ---: | ---: | ---: |
| p50 (ms) | 978.6 | 969.4 | 1114.8 | 964.5 | 920.0 |

  极差 194.8 ms（21%），而本机同一二进制两次运行的历史极差可达 39% —— 即**线程配置差异落在噪声内**。
  因此阶段 5 的目标定为“简化 + 可解释策略”，**不宣称加速**（任务文档也允许这种结论）。
- **单一 runtime 段**：`EngineConfig.runtime: RuntimeConfig`；`DetectorConfig` /
  `ClassifierConfig` / `RecognizerConfig` 删除各自的 `runtime` 字段，
  构造函数改为接收 `runtime: &RuntimeConfig`。YAML 里残留的 `det.runtime:`
  现在报 `unknown field 'runtime', expected one of …`（有测试与 CLI 双重证据）。
- **`RuntimeConfig` 新增 `formula_batch`**（默认 16，校验 > 0）。
- **新增 `src/runtime/profile.rs`**：`RuntimeProfile` / `ThreadPlan` / `ThreadSource`，
  并导出到公开 API。策略（写在模块文档里并被测试覆盖）：
  `budget = min(available_parallelism, 物理核数)`（本机 14）；
  显式 `intra_threads` → `ThreadSource::Explicit`，否则 `Auto` 且 `ort_intra = budget`；
  `ort_inter = 1`；`rayon = 显式值，否则 clamp(budget/4, 1, 8)`（本机 3）；
  `sessions = 3`（启用分类器）或 `2`；`session_runtime()` 是三个阶段会话**唯一**的设置来源，
  故意不提供“每阶段一份完整配置”的覆写。
- **Rayon 不再静默失败**：`apply_rayon_global_pool()` 返回 `Result`；
  已有全局池且请求值是显式的且不一致时返回 `RapidOcrError::Config` 并同时报出实际值与请求值；
  `Auto` 情况下沿用既有池并把**实际**线程数写回 `ThreadPlan.rayon`（报告不撒谎）。
  删除了原来的 `let _ = builder.build_global();`。
- **公式批处理根因修复**：`FormulaPolicy::max_regions` 默认 64，而识别器批上限默认 16，
  且 `recognize_with_formula` 一次性传入全部 crop —— 因此 17–64 个公式区域的页面会**整页失败**。
  现在 `FormulaRecognizer::recognize_batch` 内部按 `max_batch_size` 分块并保持输入顺序，
  引擎用 `profile.formula_batch` 设置该上限。
- 删除 `init_rayon_global_pool` / `resolve_rayon_threads` / `available_parallelism` 等重复实现；
  `runtime/session.rs::auto_tuned_thread_budget` 提升为唯一实现并被 profile 复用。
- `bench_warm_e2e` 报告新增 `meta.thread_plan`（含 `rayon` 的实际生效值），
  并把原先三个阶段的线程字段合并为一个 `meta.benchmark.runtime`。
- 外部配置三个 `test-config*.yaml` 改为单一顶层 `runtime:` 段并加 `formula_batch: 16`。

**执行命令与关键结果**：

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --all-targets -- -D warnings` | 通过（0 warning） |
| `cargo test --all-targets` | lib 272 + bin 18 passed / 0 failed（阶段 4 为 258） |
| `cargo test --features directml-provider` | 通过 |
| `cargo check --features directml-provider,cuda-provider` | 通过 |
| `cargo test --lib formula_integration_tests -- --test-threads=1`（真实模型） | 10 passed / 0 failed（68.4 s） |
| `tools/run_thread_matrix.ps1 -PostRefactor` | 5 组组合 p50 1016–1033 ms（极差 1.6%） |

**基线对比**（12 图，硬门槛）：

| 指标 | 阶段 0 基线 | 阶段 5 后 | 判定 |
| --- | ---: | ---: | --- |
| mean CER（硬门槛） | 0.44765135645866394 | **0.44765135645866394**（逐位相同，12 张逐图相同） | 未退化 |
| 区域数均值（硬门槛） | 34.833333333333336 | **34.833333333333336**（逐位相同） | 未退化 |
| OCR p50 (ms) | 1001.1 | 1023.9（阶段报告）/ 1098.2（复核） | 噪声范围内 |
| 峰值工作集 (MB) | 1284.0 | 1342.2 / 1280.1 | 噪声范围内 |

`meta.thread_plan` 示例（auto 配置）：
`{"source":"auto","budget":14,"ort_intra":14,"ort_inter":1,"rayon":3,"sessions":2}`

**未覆盖风险**：

- **没有加速**：本阶段的收益是结构简化与可解释策略。矩阵显示 intra/rayon 选择在本机是噪声，
  因此不支持任何“调线程变快”的说法；代码与文档中均无加速声明。
- `ThreadSource` 的语义边界：Rayon 不一致的错误只在显式固定 ORT intra 时触发；
  同进程内第二个引擎若显式指定 `rayon_threads` 而 intra 为 auto，会沿用既有池而不是报错。
  这是刻意的取舍（避免对 auto 策略过度失败），已在模块文档与本节记录。
- `rayon = 3`（auto）相对旧行为（auto 路径 14、`--intra-threads` 路径 20）是行为变化，
  实测在噪声内、指标逐位不变；将来若在别的机器上出现退化，应先用同一矩阵脚本复测。
- 公式分块测试使用的契约 fixture 每行输出恒定（只有 batch 维与输入有关），
  因此测试断言的是数量、内容与“分块结果 == 显式分块结果”，**顺序由结构保证而非内容证明**。
  这一点写在测试注释里。

**是否触发公式 smoke / val-501 / 全量评测**：**触发了公式集成测试**（`formula_integration_tests`，
使用真实 PP-FormulaNet 模型，10 passed），因为本阶段修改了公式批处理的调用链。
未触发 im2latex-100 smoke / val-501 / 全量集：批处理分块不改变单图结果，
且引擎级与识别器级测试已覆盖；若发布前需要，可按 §0.3 的第 3 档执行。

---

## 阶段 7：API、模块和文档清理

**阶段**：7
**日期**：2026-10-03
**提交**：`（本阶段提交）`
**变更摘要**：

- **crate metadata**（`Cargo.toml`）：`repository` 改为实际仓库
  `https://github.com/jinghu-moon/rapid-ocr-rs`；补 `rust-version = "1.85"`、
  `categories`（computer-vision / multimedia::images / api-bindings）、
  keywords 与 description 改为反映 Windows-only + PP-OCRv6/PP-FormulaNet；
  新增 `exclude = ["tests/baseline/**"]`。
- **新增 `CHANGELOG.md`（开发期变更记录）**：明确这是破坏性平台收窄、不承诺
  Linux/macOS，并逐项列出删除的公开 API 与原因。
- **模块边界写入 `mod.rs`**：`runtime`、`input`、`vision`、`output` 补齐模块文档，
  说明共享层角色与依赖方向（都不依赖 `ocr` / `formula`），与既有的
  `ocr/mod.rs`、`formula/mod.rs` 一起构成“两条 pipeline 不再被复制”的边界说明。
- **`cargo package` 自包含**：`tests/baseline/**` 是我方证据（记录了采集机器的绝对路径
  与 benchmark 结果），属于生成产物而不是 crate 内容，因此排除；`tests/fixtures/**`
  必须保留，因为契约 fixture 是干净 clone 能跑测试的前提。
- **静态清理门槛分类**（`rg` 命中项逐条归类，见下）。

**执行命令与关键结果**：

| 命令 | 结果 |
| --- | --- |
| `cargo test --all-targets` | lib 272 + bin 18 passed / 0 failed |
| `cargo package --list --allow-dirty` | 154 个文件（排除 baseline 前为 171） |
| `cargo package --allow-dirty --no-verify` | `Packaged 154 files, 12.0MiB (2.6MiB compressed)` → 压缩后 2.6 MB，远低于 crates.io 10 MB 上限 |
| 包内容检查 | 包内**没有任何**开发机绝对路径（`[A-Z]:\100_Projects` / `[A-Z]:\Users` 命中 0），没有模型权重，没有 `Formula-TestSet`/`OCR-Model`，没有生成报告 |

**静态清理门槛**（`rg -n "linux|macOS|macos|CANN|cann-provider|unsupported platform|OpenCV|turbojpeg|RuntimeBackend"`）：

| 归类 | 命中示例 | 处理 |
| --- | --- | --- |
| 当前支持声明（非目标） | `lib.rs`、`platform_gate.rs`、`README.md` 的 “Linux/macOS 是明确非目标” | 保留：这是**当前**、正确的平台声明 |
| 当前支持声明（已删除能力） | `api.rs`/`config.rs`/`provider.rs`/`session.rs` 的 “CANN 已删除”“`RuntimeBackend` 是伪抽象” | 保留：说明为什么现在没有该能力 |
| 负向测试 | `config.rs` 断言 `provider_preference: cann` 与 `vision_backend` 被拒绝 | 保留：这就是期望行为 |
| 第三方说明 | `THIRD_PARTY_NOTES.md`、`docs/01`、`docs/02` 的历史记录 | 保留：历史记录，不作为当前支持声明 |
| 数值一致性注释 | `det/preprocess.rs:720`、`vision/resize.rs:232`、`vision/rotate_crop.rs:289` 记录“与 Pillow/OpenCV 的数值对齐要求” | 保留：删除依赖不等于放弃数值契约 |
| 正则误报 | `cann` 命中 `cannot`；`macos` 命中注释里的历史措辞 | 无需处理 |

**`RuntimeConfig` 字段复核**：逐字段确认仍有调用方 ——
`auto_tune_threads` 仍被 `runtime/session.rs::derive_runtime_threads` 使用
（公式 benchmark/eval 工具直接构造 `RuntimeConfig`，不经过 profile），
`rayon_threads` / `enable_cpu_mem_arena` / `fail_provider_unavailable` / `formula_batch`
都由 `RuntimeProfile` 消费。**没有发现无调用方字段**，因此未删除任何字段
（删除没有依据的“清理”同样是错误方向）。

**未覆盖风险**：

- `rust-version = "1.85"` 是保守声明（edition 2024 的最低要求），未在本机验证更低版本；
  本机工具链是 1.98。
- `exclude = ["tests/baseline/**"]` 意味着**从 crates.io 安装的源码包不含基线 JSON**；
  基线仍随仓库提供。如果将来希望发布时也带证据，应改为把基线放进 `docs/` 或单独仓库。
- 交叉引用：`docs/03` 里的 `rg` 门槛命令仍会命中上述“保留”项，这是预期的，
  分类表就是它的判据。

**是否触发公式 smoke / val-501 / 全量评测**：未触发。本阶段只改 metadata、文档、
模块注释与打包范围，不改任何代码路径。

---

## 阶段 6：Windows 性能优化（测量优先）

**阶段**：6
**日期**：2026-10-03
**提交**：`（本阶段提交）`
**变更摘要**：只增加测量与“关掉死分支/补上缺失的等价性测试”，**没有做任何未测量的优化**。

- **补齐 SIMD 等价性测试（发现的真实缺口）**：`det/preprocess.rs` 按
  `is_x86_feature_detected!("avx2")` 在 AVX2 与 scalar 行写入之间分派，却**没有任何测试**证明两者一致；
  `det/postprocess/mod.rs` 的 `threshold_chunk_{scalar,sse41,avx2}` 与
  `dilate_row_2x2_{scalar,avx2}` / `sum_f32_slice_avx2` 同样只有间接覆盖。新增 8 个测试直接调用各实现：
  宽度 1/2/7/8/9/15/16/17/23/24/31/33/64/65（8 的倍数走纯向量路径，其余强制向量+标量尾部混合）、
  plane_stride 0/1/7/8/33、并行分支的 `out_ptr` 行偏移形式（并断言 13 个 float 前缀未被改写）、
  以及 NaN/±inf 与 0..127 长度。比较用 `to_bits()` 而不是 `==`（缓冲区哨兵是 NaN，`==` 会假失败）。
  同时删除 `src/ocr/det/` 下已死的 `#[cfg(not(target_arch = "x86_64"))]` 分支。
  `rec/cls/preprocess.rs` 与 `vision/resize.rs` **没有** SIMD 分派（标量 + LUT），因此没有缺口，
  也没有添加任何推测性的 SIMD。
- **`bench_warm_e2e` 增加逐阶段耗时**：`stages`（输入 decode/resize/crop、detector/classifier/
  recognizer 的 preprocess/infer/postprocess、page_total 的 count/min/max/avg/p50/p90，
  复用既有 `stats()` 口径）与 `timing_split`（ORT vs Rust 占比）。
  `Option<f32>` 只在 `Some` 时入样，分类器关闭时报 `count:0` 而不是伪造 0 ms。
- **新增 `tools/check_provider_claims.ps1`**：读取多份 bench 报告，凡“声称非 CPU provider 且
  `fallback_to_cpu=false`”而实测 p50 未比 CPU 参考好 10% 的，判 FAIL 并给出说明。
  这把“加速必须有实测依据”变成可执行检查。

**关键测量结果**：

`max_side_len` 质量-延迟曲线（12 图，交错 6 轮，取每轮 p50 的中位数）：

| max_side | p50 中位 (ms) | p90 中位 (ms) | mean CER | 区域数均值 | 峰值 (MB) |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 960 | —（只测 CER） | — | 0.45248 | — | 756 |
| 1280 | 550.57 | 793.21 | 0.43547 | 33.4167 | 801 |
| 1600 | **715.41** | 1011.18 | **0.43693** | 33.4167 | 1159 |
| 2000 | 981.25 | 1208.53 | **0.44765**（= 硬门槛） | **34.8333**（= 硬门槛） | 1219 |

同一设置的同一二进制最坏相差 1.29×（1600 侧 840.87 vs 652.77），这就是必须交错测量的原因。
1600 在两个轴上都不差于 2000，但**库默认值未改**（任务明确要求只测不改）。

ORT vs Rust 时间分布（max_side_len 2000）：**ORT 推理 841.44 ms = 86.03%**
（detector 561.0 / recognizer 280.4）；所有被命名的 Rust 前后处理合计
**5.70 ms = 0.58%**；最大的单项 Rust 成本是识别后处理（CTC 解码 + word boxes）41.67 ms = 4.26%，
crop 1.66 ms（0.17%），输入 resize 6.85 ms（0.70%）。
**阶段门槛按“瓶颈是 ORT 而不是 Rust 热路径”达成**：即使把 image_ops/resize/preprocess 全部降为零成本，
也无法把页面 p50 改变超过约 1.5%，小于本机噪声。

`-C target-cpu=x86-64-v3`（同 commit、独立 scratch target 目录、5 组交错 A/B）：
默认 p50 中位 999.05 ms vs v3 970.68 ms → 中位差 **−2.84%**（4/5 组支持 v3，其中一组 +19.35%）。
**结论：不写入本库的 Cargo.toml**（已遵守）；可作为消费方应用 release profile 的可选设置，
但落在噪声范围内，不得当作保证收益 —— 且 86% 页面时间在预编译的 `onnxruntime.dll` 内，
该 flag 够不到主要成本。

公式 batch 1/2/4/8/16（CPU，`resolved=Cpu`，无回退）：
单图延迟 p50 = 207.8 / 186.0 / 312.6 / 244.1 / 216.5 ms，**不单调且基本持平**；
专用单图路径 p50 = 196.08 ms；`session.run` 随 batch 近似线性（195.8 → 3287.9 ms），
即**该模型在 CPU 上批处理买不到吞吐**。全部 batch 输出一致且 `deterministic_tokens=True`，
无截断；峰值工作集 2.89 GiB。
**公式模型懒加载已证明**：把两个公式 ONNX 移走后普通 12 图 bench 正常完成
（exit 0、区域数不变），移回后 SHA-256 与移动前一致。
**公式资源上限未放宽**：序列 4096（> 实测图内 Loop 宽度 2561）、batch 上限 16、
输入 24 Mpx + 共享加载层限制、detector 768/300/max_regions。

**Provider 声明检查器输出**（`tools/check_provider_claims.ps1`，对已提交的三份报告）：

```text
Report               Claimed        Fallback P50ms    DeltaPct Speedup Verdict
bench-cpu.json       (CPU baseline) n/a      1,013.50                  REFERENCE
bench-direct_ml.json DirectMl       no       508.65   -49.8%   1.99x   PASS
bench-cuda.json      Cuda           no       1,013.47 -0.0%    1.00x   FAIL
FAIL: bench-cuda.json - claims Cuda but p50 is within +/-10% of the CPU reference
NOTE: 这是本机（ORT 1.28.0、缺 cuDNN）的**预期**结论，不是工具缺陷。
```

**没有改动的东西（以及支撑该决定的数字）**：`image_ops.rs` 未改 ——
crop 0.17%、resize 0.70%、detector/recognizer preprocess 0.23%/0.35%，每项都远低于 1%，
没有可测量的收益，识别批处理路径已经在复用 `tmp_bgr` + `LinearResizeScratch`。
未加 SIMD、未加缓存、未改任何库默认值、未放宽任何上限。

**未覆盖风险**：

- **本阶段没有产生加速**：结论是“瓶颈在 ORT，Rust 侧无可测量空间”。这是任务允许的结论，
  但意味着普通 OCR 的延迟改善只能来自换模型/换 provider/降分辨率（见曲线），而不是改 Rust。
- `target-cpu=x86-64-v3` 的 −2.84% 落在噪声内，5 组里有 1 组明显反向（+19.35%）；
  若应用侧要启用，必须在目标机器上按同一交错方法复测。
- 逐阶段计时来自一次 `--rounds 3` 采样；`Option<f32>` 缺项按“不入样”处理，
  因此分类器关闭时不会污染均值，但分类器开启时的数据本次未单独统计。
- 本机噪声（同二进制 p50 波动可达 29–39%）意味着**所有延迟结论都只能按量级解读**。

**是否触发公式 smoke / val-501 / 全量评测**：触发了**公式 batch 基准**与**公式懒加载验证**
（本阶段确实动到了公式调用的可观测行为），但**未触发 im2latex-100 / val-501 / 全量集**：
没有修改 tokenizer、postprocess、EOS 或指标实现，公式单图输出与 batch 确定性均未变化。

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
