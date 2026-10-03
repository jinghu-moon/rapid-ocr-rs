# rapid-ocr-rs Windows x64（MSVC ABI）阶段执行记录

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
| target | Windows x64 + MSVC ABI：`x86_64-pc-windows-msvc`（本阶段冻结；谓词必须含 `target_env = "msvc"`） |
| 明确非目标 | Windows x86（32 位 i686）、Windows ARM64（`aarch64-pc-windows-msvc`）、Windows GNU ABI（`x86_64-pc-windows-gnu`）、Wine、WSL、Linux、macOS |
| OS | Microsoft Windows 11 IoT 企业版 LTSC 10.0.26100（Build 26100，x64） |
| CPU | 13th Gen Intel Core i5-13600KF，14 物理核 / 20 逻辑核，3.5 GHz |
| GPU | NVIDIA GeForce RTX 4070 Ti SUPER，驱动 32.0.15.9186（591.86） |
| 工具链 | rustc 1.98.1 (48a229cea 2026-09-01)，cargo 1.98.1 |
| ort crate | `=2.0.0-rc.13` |
| ORT 链接方式 | `rustc-link-lib=static=onnxruntime`：**ONNX Runtime 被静态链接进可执行文件**，进程里没有 `onnxruntime.dll` 模块 |
| 被链接的 ORT 静态库 | `…/ort.pyke.io/dfbin/x86_64-pc-windows-msvc/f7c654b3…/onnxruntime.lib`，341,152,186 字节，SHA-256 `c3f5bb80…6cdd0` |
| ORT API 版本（运行库自报） | `1.28.0`（`OrtGetApiBase()->GetVersionString()`） |

> **勘误（终审修复，详见文末「终审修复」）**：本节原文写的是“实际加载的 ORT 运行库
> `C:\Windows\system32\onnxruntime.dll`，10,572,960 字节，版本 `1.17.260311-1434.1.os-germanium`
> （Microsoft Windows 内置）”，这是**两处误读的叠加**：
>
> 1. 那份 DLL **从未被加载**（`GetModuleHandleW("onnxruntime.dll")` 返回 0，
>    `GetLastError = 126`），它只是 Windows 自带的同名文件；
> 2. 它的 `VersionInfo.FileVersion` 是 **文件版本** 1.17.x，而运行库自报的
>    **ORT API 版本**是 `1.28.0` —— 两者本来就不是一个东西。
>
> 真正决定行为的是被静态链接进 exe 的那份 `onnxruntime.lib`（见上表）。
> `1.17` 这个数字因此不能作为阶段 0 的运行时标识；阶段 0/8 两次基线实际上跑在
> **同一份 ORT 1.28.0** 上。

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

> **根因勘误（终审修复）**：本节原文把根因写成“本机加载的是 Windows 内置 ORT 1.17
> （仅 CPU/DirectML 方向），`System32` 下不存在 `onnxruntime_providers_cuda.dll`”。
> 这两句都不成立：`System32\onnxruntime.dll` 从未被加载（也不是本 crate 的运行库），
> 而 `onnxruntime_providers_cuda.dll` 恰恰**存在**（就在 exe 旁边，62 MB，来自 ort 缓存）。
> 真正的根因是 **cuDNN 缺失**：CUDA EP 的 provider 库能加载、`is_available()` 返回 true，
> 但没有 cuDNN 时它无法执行模型，节点全部落在 CPU上 —— 而 API 层看不到这一点。

因此当前 API 会给出 `selected_ep = Cuda, fallback_used = false` 的“成功”结论，
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

## 阶段 1：建立 Windows x64 + MSVC ABI 编译边界

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
它无法真正执行模型，节点全部落在 CPU —— 而 `selected_ep = Cuda, fallback_used = false`
会让调用方以为加速已生效。
因此 crate 明确写入规则：**加速结论必须来自实测 P50/P90，不得仅凭 `selected_ep` 宣称加速**；
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
- DirectML 只在一台机器上验证；`selected_ep = DirectMl` + 实测 2× 收益目前一致，
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

> **勘误（终审修复，P1-2）**：`ThreadPlan` 的三个线程字段是**无条件**算出来的，
> 完全没有看 `auto_tune_threads`——本阶段文档里“`ort_intra = budget`”这条规则因此
> 在 `auto_tune_threads = false` 时是**错的**：它把“不要自动配置”变成了“按预算配置”，
> 而 `runtime/session.rs::derive_runtime_threads` 又按字段名把 `false` 理解成“不配置”。
> 同一份 `RuntimeConfig` 于是在引擎路径与公式路径上行为不同。
> 终审把三个字段改成 `Option<usize>`（`None` = 不配置），删除了 `derive_runtime_threads`，
> 并新增了 profile 级与引擎级回归测试。现在的示例（auto 配置）不变，但
> `auto_tune_threads = false` 时报告里会写
> `{"source":"auto","budget":14,"ort_intra":null,"ort_inter":null,"rayon":null,"sessions":2}`。

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
  `https://github.com/jinghu-moon/rapid-ocr-rs`；补 `rust-version = "1.88"`（当时写的 1.85 是错的，见终审修复）、
  `categories`（computer-vision / multimedia::images / api-bindings）、
  keywords 与 description 改为反映 Windows x64 + MSVC ABI + PP-OCRv6/PP-FormulaNet；
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
`auto_tune_threads` 由 `runtime/profile.rs::RuntimeProfile::plan` 消费（`false` 时三个线程
字段保持 `None`＝不配置），**不再**存在第二份推导逻辑；
`rayon_threads` / `enable_cpu_mem_arena` / `fail_provider_unavailable` / `formula_batch`
都由 `RuntimeProfile` 消费。**没有发现无调用方字段**，因此未删除任何字段
（删除没有依据的“清理”同样是错误方向）。

> **勘误（终审修复）**：本节原文写“`auto_tune_threads` 仍被
> `runtime/session.rs::derive_runtime_threads` 使用（公式 benchmark/eval 工具直接构造
> `RuntimeConfig`，不经过 profile）”。那个函数正是 P1-2 的根因——它与 profile 对同一个
> 公开字段给出**相反**的解释，已在终审修复中删除：现在只有 `RuntimeProfile::plan`
> 推导线程数，`OrtSession` 只做透传。

**未覆盖风险**：

- `rust-version = "1.88"` 是**实测下限**，不是保守估计：代码使用了 `as_chunks`/`as_chunks_mut`（1.88 稳定），声明得更低会让 `cargo clippy` 的 `incompatible_msrv` 直接失败；
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

> **口径勘误（终审修复，P2-1）**：上面这段用的是
> `page_total.preprocess_ms + page_total.postprocess_ms` 作为“Rust 成本”，它**漏项且口径不对**：
> 三个模型各自的 preprocess/postprocess、输入 decode/resize/crop 都没有进账，
> 而外层 `preprocess_ms` 与阶段计时根本不在同一层。终审把它换成时间账本
> （`src/runtime/timing.rs`，报告字段 `timing_ledger`），重算后的结果见文末「终审修复」：
> **ORT 推理 ≈85.6%、被命名的 Rust 侧 ≈13.7%（其中输入窗口 8.60%、识别后处理 4.19%），
> 并带有 −6.75 ms／页（0.69%）的残差**。也就是说原文的 “0.58%” 低估了约 23 倍，
> 而 “86.03%” 本身量级正确（换口径后为 85.63%）。
>
> **残差必须一起读**：这个账本**不是**严格划分。`total_ms` 是“外层 `preprocess_ms` 窗口
> （`rapid_ocr.rs:619-719`）+ 内层 `inner.run()` 窗口（`rapid_ocr.rs:106-130`）”的总窗口，
> 而命名分量只是这些窗口里的若干子窗口（内层窗口里的 resize / padding / crop / 批次装配不属于
> 任何阶段计时），**两个窗口首尾相接、串行，不是重叠**，因此 `conserved = false` —— 这是
> **口径差异**（测量仪器的精度边界），不是 `total_ms` 算错了，也**不能**作为任何性能声明的
> 验收依据；各项占比只在 ±0.69% 内成立。阶段门槛的结论（瓶颈在 ORT、Rust 热路径没有可测量
> 空间）依据的是**推理占比比每一个 Rust 分量都大一个数量级**，0.69% 的残差无法推翻这个量级判断。

`-C target-cpu=x86-64-v3`（同 commit、独立 scratch target 目录、5 组交错 A/B）：
默认 p50 中位 999.05 ms vs v3 970.68 ms → 中位差 **−2.84%**（4/5 组支持 v3，其中一组 +19.35%）。
**结论：不写入本库的 Cargo.toml**（已遵守）；可作为消费方应用 release profile 的可选设置，
但落在噪声范围内，不得当作保证收益 —— 且约 86% 页面时间在**静态链接进来的预编译
ONNX Runtime**（`onnxruntime.lib`）内，该 flag 够不到主要成本。

公式 batch 1/2/4/8/16（CPU，`selected_ep=Cpu`，无回退）：
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

> 上面是阶段 6 当时的三份报告（旧字段名 `resolved`，且没有 ORT 指纹）。
> 终审重采后同样三份文件的输出见文末「终审修复」（CUDA 仍然 FAIL，DirectML 仍然约 2×）。

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

## 阶段 8：Windows 发布与最终验收

**阶段**：8
**日期**：2026-10-03
**提交**：`（本阶段提交）`
**变更摘要**：不新增功能，只做验收与最终报告。

### 8.1 构建与包验收

| 检查 | 命令 | 结果 |
| --- | --- | --- |
| 干净 clone 的二进制 fixture | `python tools/verify_committed_binaries.py` | 35 个 fixture 与工作区逐字节一致 |
| 干净 clone 默认测试（无外部资产） | `cargo test --all-targets` | lib 280 + bin 18 passed / 0 failed |
| 干净 clone fmt / clippy | `cargo fmt --all -- --check`、`cargo clippy --all-targets -- -D warnings` | 均通过（0 warning） |
| 干净 clone release 构建 | `cargo build --release --bins` | 通过 |
| 真实资产测试（`RAPID_OCR_REQUIRE_EXTERNAL_ASSETS=1`，禁止静默跳过） | `cargo test --all-targets` | lib 280 passed / 32.3 s |
| 打包内容 | `cargo package --list --allow-dirty` / `cargo package --allow-dirty --no-verify` | 154 文件，`12.0MiB (2.6MiB compressed)`；**包内没有模型权重、没有 `Formula-TestSet`/`OCR-Model`、没有开发机绝对路径、没有生成报告** |
| default features | 上表测试与 `rapidocr` CLI 运行 | CPU 可运行；不下载隐式模型（`allow_download: false`）；不需要 OpenCV（已删除） |
| `cli-default` | `cargo check --features cli-default` | 通过（该 feature 现在只影响 ORT dylib 下载/复制与 DirectML） |
| `cuda-provider` | 阶段 0/2 的 provider 矩阵 + `tools/check_provider_claims.ps1` | **本机无 CUDA 加速能力（缺 cuDNN），记为未验证**；不出现“伪成功” |
| `cann-provider` | `cargo check --features cann-provider` | 明确失败：`does not contain this feature` |

**Windows 运行环境要求（记录，不做隐式假设）**：

- **VC 运行库**：MSVC 目标需要 VC++ 2015-2022 x64 运行库（本机已具备）。
- **ONNX Runtime 的链接方式（勘误）**：本节原文写“本 crate 链接 `onnxruntime` **导入库**，
  运行期加载顺序为 exe 目录 → System32 → PATH，本机实际加载
  `C:\Windows\system32\onnxruntime.dll`”。实际构建输出是
  `cargo:rustc-link-lib=static=onnxruntime`：**ORT 被静态链接进可执行文件**，
  运行期不会去找 `System32\onnxruntime.dll`（`GetModuleHandleW` 对该名字返回 0）。
  应用若要绑定特定 ORT，需要替换 ort 缓存里的静态库并重新链接（或用 `load-dynamic`），
  把 DLL 放在 exe 旁并不能改变已静态链接的版本。`ort_runtime_version()` 报的 `1.28.0`
  来自被链接的那份库；`meta.ort_runtime` 记录它的路径/体积/SHA-256。
- **provider 运行库**：`directml-provider` 需要 `DirectML.dll`（ort 缓存里随分发提供）；
  `cuda-provider` 需要匹配版本的 `onnxruntime_providers_cuda.dll` **加 cuDNN**（本机缺 cuDNN）。
- **模型目录**：模型与字典不随 crate 分发；默认 `allow_download: false`，
  配置文件中的 `model_path`/`rec_keys_path` 必须是本机可读路径。
- **Defender/SmartScreen**：未签名二进制首次运行可能触发 SmartScreen 提示；
  本阶段未做签名，也未做任何注册表/allocator/亲和性调优（无 profiling 依据）。

### 8.2 功能回归

功能矩阵由测试套件覆盖（真实资产下 0 跳过）：

| 区域 | 覆盖 |
| --- | --- |
| 普通 OCR 输入 | `Encoded` / `File` / `Pixels` / `Image` / `Url`（含超时、响应体上限、header 像素探测、EXIF 方向）——`input::image_loader` 测试 |
| 公式 OCR | 独立识别、页面 route、显式区域、懒加载、批处理分块、`JSON`/`Markdown`/`HTML` 输出、公式关闭时普通文本路径不变 ——`formula_integration_tests`（真实模型，10 passed）与 `formula::*` 测试 |
| 输出 | 阅读顺序、多栏、空区域、非有限/退化检测框、超大输入、模型契约错误 |
| provider | CPU / DirectML / CUDA 的 `selected_ep` / `fallback_to_cpu` / strict 语义（阶段 2 的穷举测试） |

### 8.3 最终指标（基线 vs 现在）

| 指标 | 阶段 0 基线 | 阶段 8 | 说明 |
| --- | ---: | ---: | --- |
| 12 图 mean CER | 0.44765135645866394 | **0.44765135645866394** | 逐位相同（硬门槛） |
| 12 图区域数均值 | 34.833333333333336 | **34.833333333333336** | 逐位相同（硬门槛） |
| 12 图 OCR p50 | 1001.1 ms | 981–1098 ms（多次） | 落在本机噪声内 |
| 12 图 P90 | 1153.6 ms | 1109–1230 ms | 同上 |
| 启动时间 | 143.6 ms | 127–140 ms | 未退化 |
| 12 图峰值工作集 | 1284 MB | 1283–1342 MB | 未退化 |
| `rapidocr.exe` | 34.6 MB | 33.3 MB | −3.7% |
| `bench_warm_e2e.exe` | 34.4 MB | 33.1 MB | −3.7% |
| `target/` 体积 | 18.6 GB | （见下） | 非发布指标 |
| 公式 smoke（im2latex-100） | exact 24.00% / CER 0.0863 / 失败 0 | **完全相同** | 阶段 5 改了批处理，故重跑 |
| polygon IoU | null | null | golden manifest 未标注 boxes，无法作为门槛（已记录） |

**全量公式集是否重跑**：**否**。理由（按 §0.3 与 §11.3 的规则）：
阶段 5 修改了公式**批处理分组**（分块而非一次性传入）与 `formula_batch` 配置，
属于“batch 逻辑”范畴，因此执行了**第 3 档**验证：im2latex-100 smoke（结果与基线逐位相同）
+ 真实模型集成测试（10 passed）+ batch 1/2/4/8/16 的 `deterministic_tokens=True` 断言。
tokenizer、postprocess、EOS 语义与指标实现未改动，故未重跑 val-501 与全量集。
`tests/baseline/formula-evaluation-2026-10-03.json` 中 501/10,355/UniMER 的结果仍然有效，
其 `sample_set_sha256` 与 `content_sha256` 可复核。

### 8.4 未实现或环境相关限制（写入 README Known limitations）

- **CUDA 未验证**：本机缺 cuDNN，`selected_ep=Cuda` 但实测与 CPU 无差异；
  `tools/check_provider_claims.ps1` 会把这类声明判为 FAIL。
- **DirectML 只在一台机器上验证**（RTX 4070 Ti SUPER / 驱动 591.86）：约 2× OCR 加速。
- **OpenCV 与 turbojpeg 已删除**：前者在本机无法构建，后者实测更慢；
  两者都没有测量依据支持保留。
- **本机计时噪声大**（同二进制 p50 波动 29–39%）：任何延迟结论只能按量级解读，
  且必须用交错 A/B 或多次运行。
- **polygon IoU 无门槛**：golden manifest 的 `boxes` 为空。

**未覆盖风险**：见各阶段记录；整体上最大的两个是“CUDA 未验证”与“延迟噪声导致的
优化空间无法被可靠测量”。

---

## 终审修复

**日期**：2026-10-03
**范围**：阶段 8 之后的最终评审提出 3 个 P1 与 4 个 P2；本节记录修复、证据与仍然存在的
限制。**没有**放宽任何硬门槛：12 图 mean CER 仍是 `0.44765135645866394`，
区域数均值仍是 `34.833333333333336`（逐位相同）。

### 修复清单

| 编号 | 问题 | 根因 | 修复 |
| --- | --- | --- | --- |
| P1-1 | 平台门槛把 Windows GNU 也放进来 | 谓词只有 `all(windows, target_arch = "x86_64")`，`x86_64-pc-windows-gnu` 同样满足 | `src/platform_gate.rs` 与 `src/lib.rs` 的**每一个** `#[cfg]` 加上 `target_env = "msvc"`；`tools/check_platform_gate.ps1` 增加 GNU 用例（std 缺失时打印 “target not installed -> this case is NOT verified” 并 exit 3）；`lib.rs` 增加编译期 `const` 断言 + 运行期 target 测试 |
| P1-2 | `auto_tune_threads = false` 被 profile 静默忽略 | `ThreadPlan` 的三个字段是无条件算出的 `usize`；`runtime/session.rs::derive_runtime_threads` 却把同一字段解释成“不配置”，两份实现互相矛盾 | `ThreadPlan.ort_intra/ort_inter/rayon` 改为 `Option<usize>`（`None` = 不配置）；`plan()` 遵守 `auto_tune_threads`；删除 `derive_runtime_threads`；新增 profile 级、Rayon 级与**引擎级**回归测试。**复审第二轮补齐了反向的偏差**：`OrtSession` 不再只做透传，而是调用共享的 `RuntimeConfig::effective_session_threads()`，见文末「复审第二轮」 |
| P1-3 | 阶段 0 与阶段 8 的 ORT 版本不可比 | 阶段 0 报告既没有版本也没有路径/体积/哈希，而且把 Windows 自带 DLL 的**文件版本** 1.17.x 当成了运行时版本；实际 ORT 是**静态链接**的 `onnxruntime.lib`（API 1.28.0） | 新增 `src/runtime/ort_runtime.rs`（+ `build.rs` 捕获链接信息）；bench 报告字段 `meta.ort_runtime`；用带指纹的报告重采基线；本节统一在 ORT 1.28.0 下比较 |
| P2-1 | `timing_split` 不是完整账本 | 只把 `page_total.preprocess/postprocess` 当 Rust 成本，漏掉三阶段前后处理与输入项 | 新增 `src/runtime/timing.rs`：时间账本（每项只算一次 + 显式 `unattributed` + `excess_ms`/`interpretation`），报告字段 `timing_ledger`；`timing_split` 改为从账本派生；86.03%/0.58% 两个数字按新口径重算。**复审第二轮**明确它是**诊断**工具、不是严格划分；**复审第三轮**更正了残差的因果陈述（口径差异，不是窗口重叠），字段名从 `overlap_ms` 改为 `excess_ms` |
| P2-2 | `resolved` 夸大了已证事实 | 该字段只说明“EP 链已交给 ORT 且 `is_available()` 为真”，逐节点分配不可观测 | `ProviderResolution::resolved` → `selected_ep`，`ProviderResolutionInfo::resolved` → `selected_ep`；类型文档写明“selected 只指交给 ORT 的 EP 链”；所有调用方/报告字段名/检查器同步 |
| P2-3 | 检查器太宽松 | 只要求两个字段，缺失当默认值，不校验可比性，也不校验 `-Reference` 真的是全 CPU | `tools/check_provider_claims.ps1` 重写：必需字段（三阶段 `selected_ep`/`fallback_to_cpu`、p50/p90、image_count、rounds、max_side、ORT 指纹）逐项校验；不可比一律 exit 2；`-Reference` 非全 CPU exit 2；新增 `tools/test_provider_claims.ps1` 覆盖 14 个用例 |
| P2-4 | 文档里的 `rust-version` 过期 | 文档写 1.85，`Cargo.toml` 是 1.88 | 阶段 7 的两处改为 `1.88` 并写明依据（`as_chunks` 需要 1.88，`clippy::incompatible_msrv` 是可执行证据） |

### P1-3：ORT 运行库指纹与阶段 0/8 的可比性

**指纹来源**（`src/runtime/ort_runtime.rs`）：

| 字段 | 值 |
| --- | --- |
| `api_version` | `1.28.0` |
| `runtime_source` | `static_link` |
| `runtime_module.path` | `C:\Users\seeyuer\AppData\Local\ort.pyke.io\dfbin\x86_64-pc-windows-msvc\f7c654b3729cb9e5ad2a36a0c38e5b48e63bf4eed22968931aed33a0ad0b527d\onnxruntime.lib` |
| `runtime_module.size_bytes` | `341152186` |
| `runtime_module.sha256` | `c3f5bb800fd19c05d3bd13614654f0c37ea3db1fbfba4d10963fa678cdb6cdd0` |
| `link.base_dir` | `…\ort.pyke.io\dfbin\x86_64-pc-windows-msvc`（编译期由 `build.rs` 记录） |
| `provider_dlls` | `DirectML.dll`（18,527,776 B，`loaded=true`）；CUDA 构建下另有 `onnxruntime_providers_cuda.dll`（62,276,096 B） |

`runtime_source = static_link` 是因为构建输出是
`cargo:rustc-link-lib=static=onnxruntime`：**ORT 被静态链接进 exe**，
`GetModuleHandleW("onnxruntime.dll")` 返回 0（`GetLastError = 126`）。
指纹因此按“加载的 DLL → 编译期记录的静态库 → exe 自身”的顺序确定，
每一步都可复核；任何一步读不到都会在 `link_reason` 里给出可定位原因，**不编造哈希**。

`C:\Windows\system32\onnxruntime.dll`（10,572,960 B，
SHA-256 `a9d3e0e13cadb011209013eee40ccbd6255d8a45090c7a12b635b7dde487934b`，
`FileVersion 1.17.260311-1434.1.os-germanium`）**从未被本进程加载**，
它只是 Windows 自带的同名文件。阶段 0 报告的 “ORT 1.17” 就是这个文件的**文件版本**，
而不是运行库的 API 版本。

**阶段 0 与阶段 8 的比较（修正后）**：两次基线实际上都跑在
**同一份 ORT（API `1.28.0`，`onnxruntime.lib` SHA-256 `c3f5bb80…`）**上，
所以“1.17 vs 1.28 导致基线不可比”这个判断本身是错的；真正的问题是**当时没有记录指纹**，
无法证明它们可比。本次重采后，`bench-cpu.json` / `bench-direct_ml.json` /
`bench-cuda.json` / `evaluation-cpu-ortfp.json` 都带同一指纹，
`tools/check_provider_claims.ps1` 会把指纹不一致直接拒绝（exit 2）。

**关于更早的已提交文件**：`tests/baseline/windows-baseline/bench-cpu-2000.json`、
`bench-cpu-1280.json`、`evaluation-cpu.json` **保留原样**——它们**早于指纹化**，
没有 `meta.ort_runtime`，因此属于历史证据而不是可比基线。
`bench-cpu.json` / `bench-direct_ml.json` / `bench-cuda.json` 已就地升级为新字段
（`selected_ep` + `meta.ort_runtime` + `timing_ledger`），另存
`bench-cpu-ortfp.json`（与 `bench-cpu.json` 逐字节相同，同一次运行）。

### P2-1：重算后的时间账本（12 图，max_side_len 2000，CPU）

来源：`tests/baseline/windows-baseline/bench-cpu.json` 的 `timing_ledger`。

> 表里的百分比都是相对 `total_ms` 的**诊断占比**，**不是**严格划分：被命名分量之和
> （979.03 ms）与 `total_ms`（985.79 ms）相差 6.75 ms（0.69%），原因见下面的守恒检查。
> 引用任何一项占比之前必须一起看该残差。

| 项目 | 均值 (ms) | 占 `total_ms` |
| --- | ---: | ---: |
| **total_ms（报告值）** | 985.79 | 100% |
| ONNX Runtime 推理（det 570.57 + rec 273.53） | 844.10 | **85.63%** |
| 输入侧（decode + resize + crop + 外层窗口其余部分） | 84.81 | 8.60% |
| — 其中 resize | 3.82 | 0.39% |
| — 其中 crop | 1.14 | 0.12% |
| — 其中 decode | 0.001 | 0.0001% |
| — 其中外层其余部分（`input_other_ms`） | 79.85 | 8.10% |
| 模型侧 preprocess（det + rec） | 4.27 | 0.43% |
| 模型侧 postprocess（det + rec；含 CTC 解码与 word boxes） | 45.82 | 4.65% |
| 页面级 postprocess | 0.02 | 0.002% |
| **被命名分量合计（`attributed_ms`）** | 979.03 | 99.31% |
| **口径残差（`unattributed_ms` = `-residual_ms`；`excess_ms` = 6.75）** | 6.75 | **0.69%** |
| 其中 Rust 侧合计（输入 + 模型前后处理 + 页面后处理） | 134.93 | **13.69%** |

**守恒检查是诊断判据，不是验收依据**：`conservation = { attributed_ms: 979.03,
total_ms: 985.79, residual_ms: −6.75, excess_ms: 6.75, tolerance_ms: 0.00099,
conserved: false, interpretation: "…" }`。账本**没有**把差额藏起来——它显式报出
6.75 ms／0.69% 的残差、给出 `conserved = false`，并用 `excess_ms` 与
`interpretation` 说明该怎么读。`interpretation` 随报告进入 JSON。下面是**按随仓库提交的
基线（`tests/baseline/windows-baseline/bench-cpu.json`）复算出来的实际文案**（第二次采集的
均值账本是 `residual_ms = −7.441531`、`total_ms = 1002.831984`；两次采集只差重采噪声，
文案结构相同）：

```json
"conservation": {
  "attributed_ms": 979.0328996998206,
  "total_ms": 985.7866990831163,
  "residual_ms": -6.7537993832957,
  "tolerance_ms": 0.0009857866990831163,
  "conserved": false,
  "excess_ms": 6.7537993832957,
  "interpretation": "NOT a strict partition, and NOT a wrong total either: this is a SCOPE difference between total_ms and the sum of its named parts. residual_ms = -6.753799, i.e. the named components sum to 6.753799 ms (0.69% of total_ms) LESS than total_ms. The two timing windows are SEQUENTIAL and do not overlap: the outer OcrTimings::preprocess_ms window runs from rapid_ocr.rs:619 to :719 and inner.run() is only entered at :720, whose own end-to-end window runs from rapid_ocr.rs:106 to :130; total_ms is their sum (total_ms = exec.e2e_ms + preprocess_ms, rapid_ocr.rs:910). No wall-clock interval is therefore counted twice. What the residual measures is the part of inner.run()'s own window that falls outside every named stage window and therefore has no component in this ledger: the resize inside prepare_image (rapid_ocr.rs:156-162), apply_vertical_padding (rapid_ocr.rs:189-195), crop_text_regions (rapid_ocr.rs:210-212), and the recognizer's batch assembly (rec/recognizer.rs:89-98, 222-231). total_ms is a single wall-clock measurement and is unaffected by how the parts are named; conversely, because the parts were measured independently, this ledger alone cannot decide which side is closer to the real elapsed time, so it does NOT claim that total_ms overstates. This ledger is a DIAGNOSTIC instrument, not acceptance evidence: every share is valid only to within 6.753799 ms (0.69% of total_ms), and the stage-6 conclusion (ONNX Runtime inference is an order of magnitude larger than every Rust component) is not affected by a residual of this size."
}
```

**残差是口径差异，不是计时窗口重叠**（本节曾在终审时把它写成“两个窗口重叠、同一段墙钟被算了
两次”，那是与实现矛盾的因果说法，已按代码更正）：外层窗口在 `rapid_ocr.rs:619` 开始、
`:719` 结束，`inner.run()` 在 `:720` 才进入，其 e2e 窗口在 `:106` 开始、`:130` 结束，
`total_ms = exec.e2e_ms + preprocess_ms`（`rapid_ocr.rs:910`）——**两段墙钟首尾相接，没有任何
区间被计两次**。残差的真实来源是 `inner.run()` 自己的窗口里有墙钟不属于任何被命名的阶段计时
（临时探针在 12 图上量到的量级）：

| `inner.run()` 里未被命名的部分 | 代码位置 | 实测量级 |
| --- | --- | --- |
| `prepare_image` 内的 `resize_image_within_bounds`（记在 `resize_ms`，但不参与任何阶段计时） | `rapid_ocr.rs:156-162` | ≈ 4.7 ms/页 |
| `apply_vertical_padding`（`proc_img.clone()` + 填充，在检测阶段但不在 `detect_ms` 里） | `rapid_ocr.rs:189-195` | 0.8–3.5 ms/页 |
| `crop_text_regions`（记在 `crop_ms`，同样不属于任何阶段计时） | `rapid_ocr.rs:210-212` | 1.5–2.5 ms/页 |
| recognizer 在三次求和之外的批次装配 / 排序 / bidi | `rec/recognizer.rs:89-98, 222-231` | 0.2–2.1 ms/页 |
| `run()` 内阶段之间的零头 | `rapid_ocr.rs:106-132` | ≈ 0.2 ms/页 |

各项之和（约 8–13 ms/页）覆盖了实测残差（5.28–9.74 ms/页）。因此：

- 账本是**诊断（diagnostic）仪器**：它回答“瓶颈在哪一侧”，各项占比只在
  ±`excess_ms`（0.69%）内成立；
- **它不能作为任何性能声明的验收依据**，也不是严格划分（strict partition）；
- 分量是各自独立测量的，单凭账本**无法**判定哪一侧更接近真实用时，因此账本同样
  **不声称** `total_ms` 高估了；
- 阶段门槛的结论不依赖它：ORT 推理占比（≈85.6%）比**每一个** Rust 分量都大一个数量级，
  0.69% 的残差无法推翻这个量级判断。

36 个样本各自的残差落在 −9.74 … −5.28 ms（p50 −6.63 ms），所以它不是个别样本的抖动。
根因（`inner.run()` 窗口内存在未被命名的墙钟）记录在
`src/runtime/timing.rs` 的模块文档（含上面的逐项表）与
`real_world_outer_window_does_not_conserve_the_reported_total` /
`interpretation_states_sequential_windows_and_never_claims_overlap` 两条测试里（debug 构建下
同一残差放大约一个数量级，约 −55 … −100 ms/页）。
`timing_ledger.conservation.interpretation` 把这段话原样带进 JSON，因此报告是自解释的。

**与原文对比**：

| 口径 | 原文（阶段 6） | 终审重算 | 说明 |
| --- | ---: | ---: | --- |
| “ORT 推理占比” | 86.03% | **85.63%**（±0.69%） | 量级一致（差异来自重采与分母口径） |
| “被命名的 Rust 前后处理” | 0.58% | **13.69%**（Rust 侧合计） | 原文漏掉了输入窗口与三阶段前后处理；**低估约 23 倍** |
| 未归属 / 口径残差 | 未报告 | **0.69%** | 显式报出：是 `total_ms`（总窗口）与命名分量之和的**口径差异**，不是漏项，也不是总量错误，更不是计时窗口重叠 |

阶段门槛的结论**不变**：瓶颈仍在 ORT（≈85.6%），Rust 侧总量约 13.7%，
因此“Rust 热路径没有可测量的优化空间”这一判断成立；
但原文的具体百分比是不可用的，本节的数字才是时间账本给出的值——**并且只在 0.69% 的
残差量级内成立**。

### P2-2 / P2-3：`selected_ep` 与加固后的检查器

**重命名**：`ProviderResolution::selected_ep`、`ProviderResolutionInfo::selected_ep`；
`bench_warm_e2e` 的 `meta.provider_resolution.<stage>.selected_ep`；
`formula_bench` 的 `provider_selected_ep`；`formula_eval` 的 `provider.selected_ep`。
`ResolvedExecutionProvider` / `ResolvedProvider` 枚举名**未改**（它们描述的是“解析出的
EP 值”，不是“已证明执行”），但文档已明确 `selected_ep` 只代表交给 ORT 的 EP 链头部。

**重采后的检查器输出**（对三份新报告；退出码 1 = 存在 FAIL，是预期结果）：

```text
Provider claim check
Rule: a non-CPU `selected_ep` with fallback_to_cpu=false must show p50 at least 10% better than the CPU reference; fallback_to_cpu=true is always a FAIL.
Metric: stats.ocr_total_ms.p50 (same p50 field the committed baseline reports use).
Comparability: same images_dir/image_count/rounds/max_side_len/model and the same ORT fingerprint (api + runtime sha256).

Reference (CPU): bench-cpu.json  p50=990.11 ms  p90=1,136.14 ms
ORT runtime:     api=1.28.0 sha256=c3f5bb800fd19c05d3bd13614654f0c37ea3db1fbfba4d10963fa678cdb6cdd0

Report               Claimed        Fallback P50ms    P90ms    DeltaPct Speedup Verdict
bench-cpu.json       (CPU baseline) n/a      990.11   1,136.14                  REFERENCE
bench-direct_ml.json DirectMl       no       495.28   659.41   -50.0%   2.00x   PASS
bench-cuda.json      Cuda           no       1,042.33 1,164.74 5.3%     0.95x   FAIL

FAIL: bench-cuda.json - claims Cuda but p50 (1,042.33 ms) is within +/-10% of the CPU reference (990.11 ms): acceleration claim without measured evidence
```

**退出码矩阵**（由 `tools/test_provider_claims.ps1` 逐条执行验证，14/14 通过）：

| 用例 | 退出码 | 定位信息 |
| --- | ---: | --- |
| 合法：仅 CPU 参考 | 0 | `PASS` |
| 合法：CPU + DirectML（约 2×） | 0 | `PASS` |
| 合法：CPU + CUDA（p50 相同） | 1 | `acceleration claim without measured evidence` |
| 合法：显式 `-Reference <cpu>` | 0 | `PASS` |
| JSON 非法 | 2 | `report is not valid JSON` |
| 缺 `meta.thread_plan` | 2 | `missing required field 'meta.thread_plan'` |
| 旧字段名（只有 `resolved`，无 `selected_ep`） | 2 | `selected_ep` |
| `max_side_len` 不一致 | 2 | `used max_side_len … but the reference` |
| `image_count` 不一致 | 2 | `measured … images but the reference` |
| `rounds` 不一致 | 2 | `ran … rounds but the reference` |
| `images_dir` 不一致 | 2 | `used images_dir … but the reference` |
| 模型不一致 | 2 | `used model … but the reference` |
| ORT 指纹不一致 | 2 | `ran on a different ONNX Runtime than the reference` |
| `-Reference` 不是全 CPU | 2 | `is not all-CPU` |

### 终审后的 12 图硬门槛（新基线）

**命令**：

```powershell
target\release\bench_warm_e2e.exe --config <OCR-Model>\test-config-small.yaml `
    --images-dir <OCR-test-image> --warmup-rounds 1 --rounds 3 --max-side-len 2000 `
    --intra-threads 16 --output tests\baseline\windows-baseline\bench-cpu.json
target\release\rapidocr.exe evaluate --manifest <OCR-test-image>\golden-manifest.json `
    --config <OCR-Model>\test-config-small.yaml `
    --output tests\baseline\windows-baseline\evaluation-cpu-ortfp.json
```

| 指标 | 硬门槛 | 终审实测 | 判定 |
| --- | ---: | ---: | --- |
| 12 图 mean CER | 0.44765135645866394 | **0.44765135645866394** | 逐位相同 |
| 12 图区域数均值 | 34.833333333333336 | **34.833333333333336** | 逐位相同（三份报告都是 34.83） |
| OCR p50（CPU，max_side 2000） | 观察值 | 990.11 ms | 噪声范围内 |
| OCR p90（CPU） | 观察值 | 1,136.14 ms | 噪声范围内 |
| DirectML p50 | 观察值 | 495.28 ms（2.00×） | 与本机历史一致 |
| CUDA p50 | 观察值 | 1,042.33 ms（0.95×） | **仍记为未验证**（缺 cuDNN） |

三份 provider 报告与评估报告的 `meta.ort_runtime` / `ort_runtime`
指纹完全一致（`api_version = 1.28.0`，`sha256 = c3f5bb80…`），因此这次比较是**严格可比**的。

### 终审仍然存在的限制

- **时间账本是诊断工具，不是性能验收依据**（本机 release 下残差 −6.75 ms／页，0.69%）：
  `total_ms` 是“外层 `preprocess_ms` 窗口（`rapid_ocr.rs:619-719`）+ 内层 `inner.run()` 窗口
  （`rapid_ocr.rs:106-130`）”的总窗口，而命名分量只是其中的若干子窗口——**两个窗口串行，
  不是重叠**；内层窗口里有一部分墙钟（`prepare_image` 的 resize、`apply_vertical_padding`、
  `crop_text_regions`、recognizer 的批次装配）不属于任何阶段计时。本 crate 没有在不改动
  计时语义的前提下消除这一口径差。账本把该残差作为一等公民报出
  （`conservation.excess_ms` + `conservation.interpretation`），因此
  `conserved = false` **不能**被读成“`total_ms` 算错了”。**任何性能声明都不得以该账本
  作为验收依据**；它的占比只在 ±0.69% 内成立。**未做**任何“让数字好看”的调整。
- **ORT 指纹依赖编译期记录的缓存路径**：若 ort 缓存被清理或换机，
  `runtime_module` 会退回 exe 自身并在 `link_reason` 里说明；此时
  `runtime_source = "executable"`，报告的跨机器可比性下降（但仍能标识二进制）。
- **`x86_64-pc-windows-gnu` 用例需要安装 GNU target**：本次为取得证据安装了
  `rustup target add x86_64-pc-windows-gnu`；若目标机未安装，检查器会明确
  打印 “target not installed -> this case is NOT verified” 并 exit 3，**不静默通过**。
- **CUDA 仍然未验证**：本机缺 cuDNN，`selected_ep = Cuda` 但实测与 CPU 无差异；
  这是 provider 的真实结论，不是工具缺陷。
- **计时噪声**（同二进制 p50 波动可达 39%）意味着所有延迟数字只能按量级解读。
- **公式工具的线程行为已统一**（复审第二轮修复，原先记在这里的限制不再成立）：
  `formula_bench` / `formula_eval` 直接构造 `RuntimeConfig` 时，线程数由共享的
  `RuntimeConfig::effective_session_threads()` 推导，与引擎路径同一个函数；报告里
  `provider.effective_intra_threads` / `effective_inter_threads` 给出真正下发给 ORT 的值
  （`null` = 未配置）。详见文末「复审第二轮」。

---

## 复审第二轮

**范围**：终审之后的第二轮评审提出 1 个 P1 + 2 个 P2 与一项措辞统一。本节记录修复、
证据与仍然存在的限制。**没有**放宽任何硬门槛：12 图 mean CER 仍是
`0.44765135645866394`，区域数均值仍是 `34.833333333333336`（逐位相同）。

### 修复清单

| 编号 | 问题 | 根因 | 修复 |
| --- | --- | --- | --- |
| P1 | `auto_tune_threads` 在不同公开入口含义不同 | 终审删掉 `derive_runtime_threads` 之后 `OrtSession` **只做字段透传**：引擎路径经 `RuntimeProfile::plan` 推导（intra = 预算、inter = 1），而 `FormulaSession` / `formula_bench` / `formula_eval` 直接把 `RuntimeConfig` 交给 `OrtSession`，默认配置下**不配置** ORT 线程（拿到 ORT 默认值） | 新增 `RuntimeConfig::effective_session_threads()`（**唯一**线程策略实现：显式值优先 → 否则按 `auto_tune_threads` 从 `auto_tuned_thread_budget()` 推导 → 否则 `(None, None)`）；`RuntimeProfile::plan` 与 `OrtSession::open_session` 都调用它；`OrtSession::session_threads()` / `FormulaSession::session_threads()` / `FormulaRecognizer::session_threads()` 暴露实际下发的值；`formula_bench` / `formula_eval` 报告新增 `effective_intra_threads` / `effective_inter_threads` |
| P2-1 | `ThreadSource::Explicit` 只看 `intra_threads` | `plan()` 只从 `explicit_intra` 推导 source；`rayon_threads: Some(n)` 的配置被标成 `Auto`，于是已有全局池会被静默采纳而不是报告“显式请求无法生效” | `RuntimeConfig::has_explicit_thread_request()`（三个字段任意一个 `Some(>0)`）成为 source 的依据；冲突决策抽成纯函数 `reconcile_existing_rayon_pool(source, requested, actual)`，`apply_rayon_global_pool()` 调用它 |
| P2-2 | 计时账本被当成验收证据 | 负残差（release −6.75 ms／页）被写成“重复计数”而 `unattributed_ms` 的正数被写成“漏项”，两者都被读成“总量有问题” | `src/runtime/timing.rs` 模块文档改为“诊断判据、不是验收证据”；`LedgerConservation` 新增 `excess_ms`（= `max(0, -residual)`，占比成立的上界）与 `interpretation`（人可读的读法，进入 JSON）；本文件阶段 6 与 P2-1 两节的措辞按“残差一起读”重写 |
| 措辞 | “Windows-only” 不能表达支持范围 | 支持范围是 **Windows x64 + MSVC ABI**，非目标是 Windows x86 (i686)、Windows ARM64、Windows GNU ABI（以及 Wine/WSL/Linux/macOS），而不是笼统的“Windows-only” | `README.md`、`src/lib.rs`、`src/platform_gate.rs`、`CHANGELOG.md`、`docs/03`、本文件统一措辞；非目标显式列出 i686/ARM64/GNU ABI 与理由（ORT、DirectML/CUDA provider DLL、566 MB 公式模型让 32 位 x86 成为真实限制，而不是一个 `cfg` 改动）。**未改任何代码行为** |

### 关键行为对比（P1）

| 入口 | 修改前（默认 `RuntimeConfig`） | 修改后 |
| --- | --- | --- |
| `RapidOcrEngine`（经 `RuntimeProfile::plan`） | `ort_intra = Some(budget)`（本机 14）、`ort_inter = Some(1)` | 不变：`Some(14)` / `Some(1)` |
| `FormulaSession` / `FormulaRecognizer` | `(None, None)` → ORT 自己决定 | **`Some(14)` / `Some(1)`**（与引擎一致） |
| `formula_bench`（无 `--threads`） | 同上 | 同上，且报告新增 `effective_intra_threads = 14` |
| `formula_eval`（无 `--threads`） | 同上 | 同上，且 `provider.effective_intra_threads = 14` |
| 显式 `intra_threads: Some(6)` | 引擎与公式路径都得到 `(Some(6), ...)` | 不变（显式值永远优先） |
| `auto_tune_threads = false` 且无显式值 | `(None, None)`（两条路径恰好一致） | 不变：`(None, None)` |
| `intra_threads: None, rayon_threads: Some(4)` 且已有不同大小的全局池 | `ThreadSource::Auto` → 静默采纳已有池 | **`ThreadSource::Explicit` → `RapidOcrError::Config`**，错误同时给出实际与请求的线程数 |

### 证据

- `cargo test --lib runtime::profile`：15 passed（新增
  `default_config_derives_the_same_session_threads_in_both_paths`、
  `auto_tune_disabled_derives_no_session_threads`、
  `explicit_threads_win_in_both_auto_tune_modes`、
  `rayon_only_explicit_request_conflicts_with_an_existing_pool`）。
  最后一条的顺序无关性已实测：`-- --test-threads=1` 全绿，且用
  `--exact runtime::profile::tests::rayon_only_explicit_request_conflicts_with_an_existing_pool`
  单独运行（它自己最先建立全局池）同样通过——冲突**决策**由纯函数验证，
  不依赖“池是否已经存在”。
- `cargo test --lib formula::session`：12 passed（新增
  `standalone_session_uses_the_shared_thread_policy`、
  `standalone_session_honours_explicit_threads`）。两条都用**仓库内**fixture
  `tests/fixtures/formula-onnx/formula_ok.onnx`，不需要外部 566 MB 模型，因此不会被 skip。
- `cargo test --lib runtime::timing`：8 passed（新增
  `positive_residual_is_explained_as_a_different_direction`，
  并强化 `real_world_outer_window_does_not_conserve_the_reported_total`：
  断言 `excess_ms` 与残差量级一致、解释文案包含 “NOT a strict partition”
  “DIAGNOSTIC instrument, not acceptance evidence”“valid only to within”）。
  **注**：当时的 `excess_ms` 还叫 `overlap_ms`，文案还把残差写成“窗口重叠”；
  那是与实现矛盾的因果说法，已在「复审第三轮」更正，本节其余内容保持不变。
- 12 图硬门槛（`bench_warm_e2e` + `rapidocr evaluate`）与终审一致，
  mean CER `0.44765135645866394`、区域数均值 `34.833333333333336` 逐位相同
  （输出写到 `target/gate-verify/`，未覆盖 `tests/baseline/` 里已提交的基线）。
- `formula_bench`（**不带** `--threads`，用仓库内 fixture）实测报告：
  `threads = { intra_threads: null, inter_threads: null, auto_tune_threads: true,
  effective_intra_threads: 14, effective_inter_threads: 1, logical_cpus: 20,
  physical_cpus: 14 }` —— 公式路径与引擎路径拿到同一对线程数
  （`budget = min(20, 14) = 14`）。
- `cargo test --lib formula_integration_tests -- --test-threads=1`（真实模型）：
  11 passed / 0 failed（79.1 s）。**注意**：本 crate 的该模块现在有 11 条测试，
  阶段 4/8 记录里写的 “10 passed” 是当时的状态，本轮未改动该模块。
- `pwsh -File tools/check_platform_gate.ps1`：`PASS`（两个非支持 target 各自只命中
  一条自定义 `compile_error!`，本机 target 干净通过），新措辞已在其中验证。
- `cargo test --all-targets`：lib 304 + bin 18 passed；`cargo test --features
  directml-provider`：304 passed；`cargo check --features directml-provider,cuda-provider`
  与 `cargo build --release --bins` 通过；`cargo fmt --all -- --check` 与
  `cargo clippy --all-targets -- -D warnings` 干净。
  （**本轮（复审第三轮）之后**：lib 306 + bin 18，仍全绿；见文末「复审第三轮」。）

### 仍然存在的限制

- **公式链路的延迟数字不能与终审报告直接比较**：默认配置下公式会话的线程数从
  “ORT 默认值”变成了 `intra = 预算(14)`、`inter = 1`，这是**行为变化**；
  本节的公式质量指标（CER / 精确匹配）不受线程数影响，但延迟必须在同一份二进制上重测。
- **账本仍然是诊断工具**（见上）：本轮的改动只是让它**说清楚**这一点，
  没有、也不打算消除“`total_ms` 总窗口 vs 命名分量之和”的口径差（它**不是**窗口重叠）。
- **Windows x86 (i686) 只是被显式列为非目标**，没有任何 32 位构建证据；
  理由（ORT / provider DLL / 公式模型）与 `src/platform_gate.rs` 保持一致。

---

## 复审第三轮：因果陈述、工具字段名与账本输入不变量

**范围**：第三轮评审提出 1 个 P1、3 个 P2 与一项 P3（提交基线的 schema 标注）。本节记录修复、
证据与仍然存在的限制。**没有**放宽任何硬门槛：12 图 mean CER 仍是
`0.44765135645866394`，区域数均值仍是 `34.833333333333336`（逐位相同）。

### P1：时间账本把“口径差异”误写成“窗口重叠”

**代码级结论（先用临时探针在 12 图上验证，测完回滚，仓库不留探针代码）**：

| 事实 | 代码位置 |
| --- | --- |
| 外层 `preprocess_ms` 窗口 = `preprocess_start…elapsed()`，在 `inner.run()` 之前**结束** | `rapid_ocr.rs:619`（start）→ `:719`（end） |
| `inner.run()` 在**之后**才被调用，其 e2e 窗口自 `e2e_start` 起 | `rapid_ocr.rs:720` 调用；`:106`（start）→ `:130`（end） |
| `total_ms` 就是这两个串行窗口之和 | `rapid_ocr.rs:910`：`total_ms = exec.e2e_ms + preprocess_ms` |

→ 两个窗口**首尾相接、串行**。旧文案“同一段墙钟时间在 `total_ms` 的两项里各算了一次”与实现
矛盾。残差的真实来源是 `inner.run()` 自己的窗口里有墙钟不属于任何被命名的阶段计时；逐项量级
见「P2-1：重算后的时间账本」中的表。

**修改**：

- `src/runtime/timing.rs`：`LedgerConservation::overlap_ms` → `excess_ms`（语义是“占比成立的
  上界 = `max(0, -residual)`”，不是“重叠量级”）；`conservation_interpretation` 按正/负残差
  两向重写，明确写出“**SCOPE difference**”“两个窗口**串行**（附 `rapid_ocr.rs:619-719` 与
  `:106-130`）”“没有任何墙钟被计两次”“未命名部分是哪几处（附行号）”“账本是诊断工具、不是
  验收证据”“占比只在残差量级内成立”；
- `src/bin/bench_warm_e2e.rs`：文档注释与 `totals` 同步（新增 `input_preprocess_ms` /
  `input_overflow_ms`）；
- `src/api.rs`：阶段分解测试的注释与断言按“口径差异（非重叠）”改写，并去掉
  “`decode + resize` 必须落在 `preprocess_ms` 内”这条与实现不符的断言（改由账本的
  `input_overflow_ms` 显式承载）；
- 测试：`interpretation_states_sequential_windows_and_never_claims_overlap` 锁定正确因果陈述
  （必须含“SEQUENTIAL and do not overlap”“No wall-clock interval is therefore counted
  twice”与三条行号；必须**不含**“windows overlap”“the same wall-clock interval is counted in
  two”“is duplicated”“cross the inner run() boundary”）；`positive_residual_...` 与
  `real_world_outer_window_...` 的措辞断言同步更新。

**修改前后（JSON 字段与文案）**：

| 项目 | 修改前 | 修改后 |
| --- | --- | --- |
| 字段名 | `conservation.overlap_ms` | `conservation.excess_ms` |
| 负残差文案 | “the named timing windows overlap … the same wall-clock interval is counted in two terms of total_ms … duplicated” | “this is a SCOPE difference … The two timing windows are SEQUENTIAL and do not overlap … No wall-clock interval is therefore counted twice” |
| 未命名部分 | 未指出 | 逐条给出 `prepare_image` resize / `apply_vertical_padding` / `crop_text_regions` / recognizer 批次装配及其行号 |

### P2-c：`input_ms() <= preprocess_ms` 现在按构造成立

`TimingLedger` 新增两个字段，把“三项之和超出外层窗口”的冲突变成显式数据而不是靠 `.max(0.0)`
掩盖：

- `input_named_within_ms = min(decode + resize + crop, preprocess_ms - input_other_ms)`；
- `input_overflow_ms = decode + resize + crop - input_named_within_ms`；
- 于是 `input_ms() = input_named_within_ms + input_other_ms <= preprocess_ms` **恒成立**；
- `attributed_ms()` 仍计入**完整**的三项（`input_named_within_ms + input_overflow_ms`），
  因此超出量留在口径残差里，不会被丢掉；`rust_ms()` 用截断后的 `input_ms()`（“外层窗口内的
  Rust 成本”），两者差值恰好等于 `input_overflow_ms`。

**选定的不变量**：`input_ms() <= preprocess_ms`（账本不得声称比它唯一能观测输入时间的窗口
更多的输入时间），超出量以 `input_overflow_ms` 公开。这不是“掩饰冲突”：掩饰的做法是保留
`.max(0.0)` 让 `input_other_ms` 归零、同时让 `input_ms()` 悄悄超过 `preprocess_ms`；现在超出量
有名字、进残差、可断言。**未改动** `total_ms` / `preprocess_ms` / `postprocess_ms` 的任何语义，
也未重采基线。测试：`input_overflow_keeps_the_input_invariant_by_construction`（构造
`decode + resize + crop = 12 > preprocess_ms = 5` 的样本，断言溢出为 7 ms、`input_ms() = 5`、
残差 = +7、且原始分量能与 `total_ms` 对账）。

### P2-a / P2-b：工具读/打已删除字段名

- `tools/summarize_formula_eval.py`：`compact_benchmark` 读 `provider_resolved` → 改读
  **`provider_selected_ep`**（当前 `formula_bench` 输出字段）。新增
  `ReportFieldError` / `field()` / `nested()` / `list_field()`：缺字段时抛出**定位错误**，消息里
  同时给出报告路径、期望字段与实际字段列表；CLI 以退出码 2 结束。顶层 baseline 负载新增
  `schema_version = 2` / `schema_epoch = "provider_selected_ep"` / `provider_field_renames`。
  回归测试：`tools/test_summarize_formula_eval.py`（8 条，`python tools/test_summarize_formula_eval.py`；
  装了 pytest 时也可 `python -m pytest tools/test_summarize_formula_eval.py -q`；本机**未安装
  pytest**，因此用 stdlib `unittest` 跑的）。
- `tools/run_windows_provider_matrix.ps1:93`：`$resolution.resolved` → `$resolution.selected_ep`，
  并把控制台提示改成 `selected_ep=…`（此前该行打印空白）。已核对当前三份 bench 报告的
  `meta.provider_resolution.recognizer` 确实是 `selected_ep`。
- **`tools/` 全目录扫描**（`resolved` / `provider_resolved` / `RuntimeBackend` /
  `vision_backend` / `cann` / `GenericOcrInput` / `GenericOcrOutput`）的其余命中都是**有意保留**：
  - `tools/check_provider_claims.ps1:102-109,250,294-295`：同名局部变量与“旧字段名被拒绝”的
    报错文案/兼容提示；`check_provider_claims.ps1` 正是用来抓旧字段名的工具。
  - `tools/refingerprint_windows_baseline.ps1:4`：注释说明旧报告为何不可比较。
  - `tools/test_provider_claims.ps1:16,62,171-172`：**负向用例**——故意构造 `resolved` 字段，
    断言检查器以退出码 2 拒绝并给出含 `selected_ep` 的定位信息。
  - `src/runtime/session.rs:244`、`src/config.rs:157,407,416-417` 与 `docs/03`：说明已删除的
    `RuntimeBackend` / `vision_backend` / `cann` 为何被删，以及相应的拒绝测试（历史说明，不是
    活代码）。

### P3：提交的 baseline 用旧字段名

**选定的选项：加显式 schema 标记，保留为历史证据**（另一个选项——用当前 summarizer 重新生成——
做不到：`target/formula-eval/bench-*.json` 本身也是旧 schema，`provider_resolved: "Cpu"/"Cuda"/
"DirectMl"` 仍在，但 `selected_ep` 从未被记录过，无法从磁盘恢复；同目录里的
`provider.resolved` 同样如此）。因此：

- `tests/baseline/formula-evaluation-2026-10-03.json` 新增 `schema_version: 1`、
  `schema_epoch: "provider_resolved"`、`historical: true`、`historical_reason`、
  `provider_field_renames`，并在 `note` 里点名“本文档的 provider 字段是旧 schema”；
  `evaluations` / `benchmarks` / `dataset_manifests` / `reference_comparison` 的**内容一字未改**
  （7 份评测、3 份 benchmark、7 份 manifest，`mean_cer` 等数值逐位不变）；
- 测试 `CommittedBaselineTest`（3 条）锁定：标记存在且指向旧 schema、`benchmarks[]` 用
  `provider_resolved`、`evaluations[].provider` 用 `resolved`。

### 证据

- `cargo test --all-targets`：lib **306**（原 304 + 新增 `input_overflow_...`、
  `interpretation_states_sequential_windows_...`）+ bin 18 passed / 0 failed。
- `cargo fmt --all` / `cargo fmt --all -- --check` / `cargo clippy --all-targets -- -D warnings`：
  干净；`cargo build --release --bins` 通过。
- 12 图硬门槛（`bench_warm_e2e` + `rapidocr evaluate`，输出写 `target/gate-verify/`）：
  mean CER `0.44765135645866394`、区域数均值 `34.833333333333336`，与终审**逐位相同**。
- 端到端 Python 验证：对 `target/formula-eval`（旧 schema）现在给出
  `error: … expected field 'provider_selected_ep' but this report does not contain it; available
  fields: … provider_resolved …` 且退出码 2；对在 `target/` 下合成的**当前 schema** 报告目录
  （3 份 `bench-*` 的 `provider_resolved` 改名为 `provider_selected_ep`，评测报告原样复制）
  成功写出 baseline（`schema_version = 2`、7 份评测、3 份 benchmark）。
- `pwsh -NoProfile -File tools/check_provider_claims.ps1 -Reports …`：仍然退出码 **1**，
  CUDA 仍为 `FAIL`（与终审一致）。

### 仍然存在的限制

- **账户口径差没有被消除**：`total_ms` 与“命名分量之和”仍是两个口径（release 约 0.69%）。
  本轮只修**因果陈述**与**输入侧不变量**，没有改 `total_ms` / `preprocess_ms` /
  `postprocess_ms` 的语义，也没有重采基线。
- **`input_overflow_ms` 在当前 12 图 release 基线上为 0**（`decode + resize + crop ≈ 5 ms`
  远小于外层窗口 ≈ 85 ms）；它覆盖的是 `Pixels` 输入等“外层不解码、内层仍要 resize”的电路，
  由合成样本测试锁定，本机真实数据尚未命中该分支。
- **随仓库提交的公式 baseline 仍是历史 schema**（已显式标注）。若要新的 provider 字段，
  必须在当前二进制上重跑 `formula_bench` 并重新生成 baseline，本轮未做。

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
