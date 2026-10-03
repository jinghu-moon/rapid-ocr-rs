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
