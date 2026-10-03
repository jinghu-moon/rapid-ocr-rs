# rapid-ocr-rs Windows-only 重构与性能优化任务清单

> 文档状态：待实施计划
>
> 目标：在项目开发期将 `rapid-ocr-rs` 的正式支持范围收窄为 Windows，并借此删除无效的平台分支、收紧模块边界、减少原生依赖，同时用 Windows 实测数据优化 OCR 与公式识别性能。
>
> 适用范围：`crates/rapid-ocr-rs`。本计划允许破坏性 API 修改，不为 Linux/macOS 保留兼容层。

---

## 0. 决策边界与总验收规则

### 0.1 支持范围

- 第一阶段目标平台：`x86_64-pc-windows-msvc`。
- Windows ARM64、GNU ABI、Wine、WSL 不纳入本轮正式验收；若以后需要，另立平台计划。
- Windows CPU 是基础路径；DirectML 和 CUDA 是可选加速 provider。
- CANN 不属于 Windows 目标平台，应从公开配置、代码和验证矩阵删除。
- OpenCV、turbojpeg 是否保留必须由 Windows 基准决定，不能凭主观判断删除或保留。

### 0.2 不可变工程规则

- 不考虑历史版本兼容性，允许删除 public enum、配置字段、feature 和旧模块。
- 先建立基线，再改动，再运行新旧行为对比；不得通过跳过测试、放宽断言或修改错误真值制造通过。
- 性能优化必须有测量依据，至少记录吞吐、P50/P95、峰值工作集和启动时间。
- 普通 OCR 和公式 OCR 的正确性门槛独立记录；公式全量数据集不是日常回归。
- 任何阶段完成时都必须记录命令、结果、环境、未覆盖风险。

### 0.3 日常与重型验证分层

| 变更类型 | 必须执行 | 不要求默认执行 |
| --- | --- | --- |
| 文档、模块移动、平台声明 | `cargo fmt --all -- --check`、`cargo test --all-targets`、`cargo clippy --all-targets -- -D warnings` | 公式全量集 |
| provider/config/API 重构 | 上述命令 + Windows CPU/DirectML/CUDA 编译或运行 smoke | 7 小时全量评测 |
| 普通 OCR 预处理/后处理 | 上述命令 + 12 图真实 OCR 回归 + `bench_warm_e2e` | 公式全量集 |
| 公式模型输入、tokenizer、EOS、batch、指标 | 上述命令 + im2latex-100 + val-501 Rust/Python 对比 | 仅受影响数据集的全量集 |
| 发布验收 | Windows clean clone、CPU/DirectML/CUDA 可用性和基准 | CANN/Linux/macOS 矩阵 |

---

## 1. 当前实现审计与候选优化点

### 1.1 已确认的设计问题

| 区域 | 当前状态 | Windows-only 下的处理 |
| --- | --- | --- |
| 平台边界 | 没有 crate 级 Windows 编译约束；代码包含 Linux/macOS 语义 | 增加明确的非 Windows 编译错误，文档/CI 只保留 Windows |
| 峰值内存 | `runtime/memory.rs` 同时实现 Windows、Linux、其它平台 | 只保留 `GetProcessMemoryInfo`，删除 `/proc` 和 unsupported 分支 |
| provider | CPU、CUDA、DirectML、CANN 四套公开语义 | 删除 CANN；保留 CPU/CUDA/DirectML |
| 视觉后端 | Pure Rust 与可选 OpenCV 两套实现，调用链有多处 dispatch | 先做 Windows 基准；纯 Rust 达标后删除 OpenCV feature 和 dispatch |
| JPEG 解码 | `image` 与 turbojpeg 双路径 | 先测 JPEG decode 与端到端收益；收益不足则删除 turbojpeg/cmake 依赖 |
| runtime 配置 | det/cls/rec 各自带线程/provider/视觉配置，Rayon 另有全局池 | 在基准后设计统一 RuntimeProfile，消除线程过度订阅和重复配置 |
| 后端抽象 | `RuntimeBackend` 只有 `OnnxCpu` 一个枚举变体 | 删除无实际选择价值的枚举和字段，直接固定 ONNX Runtime |
| 公式 batch | 有最大 batch 和动态 Loop，但未形成 Windows 机器的最优 profile | 用固定样本测量 batch、线程和 provider 组合，形成推荐策略 |
| 元数据 | Cargo repository 仍是旧地址 `mg-chao/rapid-ocr-rs` | 改为实际 GitHub 仓库并补齐发布元数据 |

### 1.2 不应直接做的“伪优化”

- 不把 `target-cpu=native` 写入可发布 crate 的默认 profile；它会破坏跨机器分发，应只在应用或本机 benchmark profile 使用。
- 不因为 Windows-only 就把所有纯 Rust代码改成 Win32 API；OCR 数值算法仍应保持可测试、可复用和与模型契约解耦。
- 不把 DirectML 强行设为默认 provider；当前已有数据表明 GPU 速度依赖图片集、动态 batch 和驱动，必须由基准决定。
- 不用并发包围模型调用来“堆性能”；ORT、Rayon、DirectML 同时开线程可能造成过度订阅，先测量再改。
- 不为了删除 Linux/macOS 而删除跨平台数据结构、序列化格式或算法测试；删除的是平台实现，不是业务能力。

---

## 2. 分阶段任务总表

| 阶段 | 名称 | 核心产出 | 依赖 | 完成门槛 |
| --- | --- | --- | --- | --- |
| 0 | 基线与平台决策冻结 | Windows x64 基线报告、目标矩阵、风险清单 | 无 | 基线可复跑，工作区干净 |
| 1 | Windows-only 编译边界 | 非 Windows 明确拒绝，删除跨平台内存实现 | 0 | Windows 全测通过，非 Windows 失败信息明确 |
| 2 | Provider 与运行时裁剪 | 删除 CANN，固定/简化 ONNX Runtime 入口 | 1 | CPU/DirectML/CUDA 行为和错误语义有测试 |
| 3 | 原生视觉依赖决策 | OpenCV/turbojpeg 保留或删除的实测结论 | 0、1 | 有同机质量/性能/构建成本对比，禁止凭感觉决策 |
| 4 | 视觉与输入路径重构 | 单一视觉后端、统一 decode/crop/resize 缓冲路径 | 3 | 12 图准确率不退化，内存拷贝和耗时有数据 |
| 5 | Runtime 配置与线程模型 | 统一 RuntimeProfile，消除 ORT/Rayon 过度订阅 | 2、4 | CPU/DirectML/CUDA 基准改善或有明确不退化证据 |
| 6 | OCR/公式性能优化 | Windows 推荐 profile、batch、懒加载和缓存策略 | 5 | P50/P95、启动、峰值内存达到目标 |
| 7 | API/模块边界清理 | 删除无价值枚举、重复 dispatch、死配置和旧文档 | 2-6 | `rg` 无残留，公共 API 与文档一致 |
| 8 | Windows 发布与回归验收 | CI、打包、模型运行说明、最终报告 | 7 | clean clone + release build + smoke 全通过 |

---

## 3. 阶段 0：建立 Windows 基线并冻结决策

### 3.1 任务

- [x] 记录机器信息：Windows 版本、CPU 型号/物理核心、GPU、驱动、Rust/Cargo、ORT 运行库版本。（§0 环境，含 System32 内置 ORT 1.17 的事实）
- [x] 以 `--profile release` 建立普通 OCR 12 图基线：启动时间、单图 wall time、OCR pipeline P50/P95、区域数、CER、峰值工作集。（2000/1280 两侧，见 §0）
- [x] 建立公式 smoke 基线：im2latex-100（exact 24.00% / CER 0.0863 / 链路失败 0，manifest `271424c18c000f95`）；只在模型链路改动时再运行 val-501 对比。
- [x] 分别记录 CPU、DirectML、CUDA 是否能加载并运行；不可用 provider 必须记录真实错误，不得把 CPU fallback 当加速成功。（**发现 CUDA 报告 resolved 但实测与 CPU 逐位相同 → 阶段 2 必修**，见 §0）
- [x] 记录当前二进制体积、依赖树中原生库、编译耗时和 `target` 产物大小。（§0 构建与体积）
- [x] 冻结正式 target 为 `x86_64-pc-windows-msvc`，将 ARM64/GNU/Wine/WSL 列为明确非目标。（写入 `environment.json`）

### 3.2 基线命令

```powershell
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo build --release --bin rapidocr
cargo tree --features cli-default
cargo run --release --bin bench_warm_e2e -- --config <config> --images-dir <OCR-test-image> --rounds 5 --warmup-rounds 1
cargo run --release --bin formula_eval -- --model <model> --dataset-root <Formula-TestSet> --dataset latexocr --split test --limit 100
```

### 3.3 验收

- [x] 基线 JSON 和环境说明保存到 `docs/` 或 `target/` 外的受控报告中。（`tests/baseline/windows-baseline/` + `docs/04-windows-phase-reports.md`）
- [x] 明确哪些指标是硬门槛，哪些只是观察值；不得在优化后临时修改门槛。（§0 硬门槛与观察值）
- [x] 全量公式集不属于本阶段必跑项。

---

## 4. 阶段 1：建立 Windows-only 编译边界

### 4.1 代码任务

- [x] 在 `src/lib.rs` 最前面增加非 Windows 的 `compile_error!`，错误信息明确写出当前只支持 `x86_64-pc-windows-msvc`。（定义在 `src/platform_gate.rs`，可独立验证）
- [x] 用 `#[cfg(windows)]` 保护 crate 模块；确保非 Windows 不会出现“缺少某个 Windows API”的模糊编译错误。（每个模块同一谓词 + `exports.rs` 收拢公开面；实测诊断数 = 1）
- [x] `src/runtime/memory.rs` 删除 Linux `/proc/self/status` 和其它平台 `None` 分支，只保留 Windows PSAPI 实现。
- [x] 将峰值内存来源固定为 `windows:GetProcessMemoryInfo.PeakWorkingSetSize`，测试改为必须返回正值或报告明确的 Win32 失败原因。（`PEAK_MEMORY_SOURCE` + `peak_memory_failure_reason()`）
- [x] `src/evaluation/formula/sampling.rs` 的符号链接测试保留 Windows 实现，但将“权限导致跳过”的行为改成显式测试环境说明；核心 `..` 越界测试必须始终执行。
- [x] README、Cargo metadata、验证矩阵删除 Linux/macOS 支持措辞，改写为 Windows-only 约束。（README 新增 Platform support 一节；Cargo metadata 在阶段 7 统一处理）

### 4.2 测试

- [x] Windows：`cargo test --all-targets`、fmt、clippy 全通过。
- [x] Windows clean clone：二进制 fixture 校验、默认测试、release build。（阶段 8 复跑；阶段 1 已在本机验证）
- [x] 用一个非 Windows target 做静态/交叉编译检查，预期是稳定地命中自定义 `compile_error!`，而不是意外错误。（`tools/check_platform_gate.ps1`，诊断数 = 1）

### 4.3 禁止事项

- [x] 不删除公共算法测试来绕过平台差异。（测试数 254 → 256，只增不减）
- [x] 不把 `cfg(windows)` 散落到每个业务函数；平台门槛应集中在 crate 边界。（`provider.rs` 的 `target_os` 谓词也已移除）

---

## 5. 阶段 2：裁剪 Provider 与 ONNX Runtime 运行时

### 5.1 删除 CANN

- [x] 从 `Cargo.toml` 删除 `cann-provider = ["ort/cann"]`。（`cargo check --features cann-provider` 明确报“没有该 feature”）
- [x] 从 `config::ProviderPreference`、`api::ProviderPreference`、`ResolvedExecutionProvider`、provider resolution、pipeline 映射和输出序列化中删除 `Cann`。
- [x] 删除 CANN 专属 `cfg`、测试、README、阶段 02 文档中的验收项；历史报告只保留为历史记录，不作为当前支持声明。（README 与 provider 模块文档已改为 CPU/DirectML/CUDA）
- [x] 为删除后的 provider 集合增加穷举测试：CPU、DirectML、CUDA 的请求、不可用、strict/fallback 语义均有覆盖。

### 5.2 简化 ONNX Runtime 入口

- [x] 评估 `RuntimeBackend`：当前只有 `OnnxCpu` 一个变体且实际承载 CPU/DirectML/CUDA，删除枚举和 `backend` 字段，避免伪抽象。
- [x] `runtime/session.rs` 直接构造 ORT Session；保留 `GraphOptimizationLevel::Level3`、arena 和 provider resolution 的真实配置。
- [x] 统一 provider 错误文本，区分“feature 未启用”“运行库不可用”“严格模式拒绝 fallback”。（并修掉“DirectML is only available on Windows”这条与本机矛盾的文案）
- [x] 保持公式 session 的“不允许静默回退”契约；普通 OCR 的 fallback 行为要用测试锁定。

### 5.3 验收

- [x] `cargo test --all-targets`。（lib 263 + bin 18 passed）
- [x] `cargo test --features directml-provider`。（263 passed）
- [x] `cargo test --features cuda-provider`。（261 passed）
- [x] `cargo check --features directml-provider,cuda-provider`。
- [x] 配置 YAML 不再接受 `cann` 或 `onnx_cpu` 之外的已删除字段；错误必须可定位。（`unknown field` + `unknown variant` 测试）

---

## 6. 阶段 3：OpenCV 与 turbojpeg 的实测决策

这一阶段只产生决策和基准，不直接以“依赖少”为理由删除实现。

### 6.1 OpenCV 对比

- [ ] 在同一 Windows 机器、同一 release profile、同一 12 图集合测量 Pure Rust 与 OpenCV 的：resize、rotate、quad crop、det postprocess、端到端 OCR。
- [ ] 对比输出：CER、检测区域数、polygon IoU、P50/P95、峰值工作集、冷启动、编译时间、发布目录体积。
- [ ] 检查 OpenCV 是否真的被应用配置启用；当前默认路径是 Pure Rust，OpenCV 主要增加构建和安装前置条件。
- [ ] 设定删除门槛：若 OpenCV 端到端 P95 没有至少 10% 的稳定收益，且质量无优势，则删除 `opencv-backend`、`VisionBackend::OpenCv` 和全部 OpenCV dispatch/测试。
- [ ] 若保留，必须说明适用场景、安装要求和实测收益，不得把 OpenCV 写成默认依赖。

### 6.2 turbojpeg 对比

- [ ] 对 JPEG 大图和 PNG/WEBP 代表集分别测量 `image` 解码与 turbojpeg 解码的耗时、峰值内存、输出差异和 EXIF 方向行为。
- [ ] 若 JPEG 端到端收益低于 10%，或只改善单独 decode 而不改善 OCR wall time，则删除 `turbojpeg` 和 CMake 原生构建链。
- [ ] 无论删除与否，保留“编码字节上限、header 像素探测、EXIF 处理、解码错误”测试。

### 6.3 阶段验收

- [ ] 形成 `docs/` 决策记录，包含原始数据和选择理由。
- [ ] 不得只用 microbenchmark 宣称端到端收益；至少包含真实 OCR 图片。

---

## 7. 阶段 4：视觉与输入路径重构

### 7.1 若删除 OpenCV/turbojpeg

- [ ] 删除 `VisionBackend`、`resolve_backend_*`、各模块的 OpenCV 分支和 feature 条件。
- [ ] 将 resize、rotate、quad crop、det postprocess 的入口参数移除 `backend`，减少重复传参和运行时分支。
- [ ] 删除 `RuntimeConfig.vision_backend` 及 YAML 字段。
- [ ] 把 `RecImage`、`DynamicImage`、`OwnedPixelBuffer` 的转换集中到一个明确的输入边界；禁止普通 OCR 和公式 OCR 各自维护解码逻辑。
- [ ] 检查所有转换是否发生多余 BGR/RGB 拷贝；优先使用复用 scratch/buffer 的 `*_into` API。

### 7.2 若保留某个原生后端

- [ ] 保留后端边界，但将后端选择集中到初始化阶段；热路径不再每个 crop/resize 动态匹配 enum。
- [ ] 默认固定 Pure Rust，只有明确配置才建立 OpenCV path。
- [ ] 为两种后端保留同一 golden 输出和端到端质量门槛。

### 7.3 输入路径

- [ ] Windows 文件路径统一使用 `PathBuf` 和 canonical/metadata 检查；不手工拼接反斜杠。
- [ ] 保留 URL 的 reqwest 超时和响应体限制；Windows-only 不意味着取消网络安全边界。
- [ ] 评估将 `ImageInput::Image` 与 `ImageInput::Pixels` 归并为明确的 owned/borrowed 两个输入类型，避免同一图像多种 public 表达重复维护。

### 7.4 验收

- [ ] 12 图 OCR 的 CER、区域数和已标注 polygon IoU 不退化超过阶段 0 门槛。
- [ ] 公式 detector golden、公式 route 集成测试、输入限制测试全部通过。
- [ ] 用 allocation/profiling 或至少阶段 timing 证明拷贝减少；没有证据的“零拷贝”描述不得写入文档。

---

## 8. 阶段 5：统一 RuntimeProfile 与线程模型

### 8.1 根因目标

当前 det/cls/rec 各持有 `RuntimeConfig`，同时 ORT session 自己开线程、Rayon 共享线程池、recognition/classification 还可能并行处理。Windows-only 的价值在于可以针对固定 Windows 硬件建立稳定策略，但不能直接把线程数改成“CPU 核数”。

### 8.2 任务

- [ ] 先用基线测量矩阵：ORT intra/inter、Rayon threads、recognition batch、det/rec 并发组合。
- [ ] 统计 CPU 逻辑/物理核心、线程数、上下文切换和 P50/P95；记录 oversubscription 情况。
- [ ] 设计 `RuntimeProfile`：provider、ORT intra/inter、Rayon threads、arena、公式 batch、是否 strict fallback 由一个 profile 管理。
- [ ] 删除 det/cls/rec 中重复的运行时字段；如确有阶段独立需求，明确保留 override，而不是复制整套配置。
- [ ] 禁止多次调用 `rayon::build_global` 产生静默失败；将线程池初始化结果纳入 engine 构造错误或显式状态。
- [ ] 将“自动线程数”从隐式启发式改为可解释策略：默认值、上限、手工覆盖和报告字段必须一致。

### 8.3 验收

- [ ] CPU 12 图 benchmark：P50/P95 不退化，至少一项主要指标改善；若无改善，保留简化但不宣称加速。
- [ ] DirectML/CUDA 运行时不出现 CPU fallback 伪成功。
- [ ] 公式 batch=1/2/4/8 的 EOS 前 token 结果保持一致。
- [ ] 所有 benchmark 报告包含 provider、线程、batch、构建 profile 和峰值工作集。

---

## 9. 阶段 6：Windows 性能优化

### 9.1 普通 OCR

- [ ] 以 12 图真实集合测量 `max_side_len` 1280/1600/2000 的质量-延迟曲线，不擅自改变库默认值。
- [ ] 对 detector preprocess、resize、postprocess、crop、recognizer preprocess 分别记录阶段耗时。
- [ ] 检查 x86_64 AVX2/SSE4.1 运行时分派；为 scalar、SSE4.1、AVX2 建立相同输出测试，避免仅凭 CPU 型号选择指令集。
- [ ] 评估 `target-cpu=x86-64-v3` 仅用于 SnapClip 应用 release profile，不写入 crates.io 通用库默认配置。
- [ ] 对大图输入复用 `RecImage`/scratch buffer，减少 resize、padding、crop 的临时 Vec 峰值。

### 9.2 公式识别

- [ ] 在 Windows CPU、DirectML、CUDA 上测量 batch 1/2/4/8/16 的吞吐、P50/P95、峰值工作集和 token 截断率。
- [ ] 区分“单图延迟”和“批吞吐”，不得用 batch wall time 伪装单图延迟。
- [ ] 评估 formula detector/recognizer 的懒加载：普通 OCR 请求不启用公式时不得加载公式模型。
- [ ] 评估 tokenizer metadata、model hash 和 session 的生命周期；只在 profiling 证明有收益时增加缓存。
- [ ] 对长序列设置明确的资源上限和失败分类；不得为追求吞吐取消 EOS、输入大小或响应体限制。

### 9.3 Windows 资源策略

- [ ] 以 `GetProcessMemoryInfo` 记录峰值工作集，必要时增加 committed/private bytes 观测，但不要混用口径。
- [ ] 不手写未经 profiling 证明有益的 Win32 allocator、线程亲和性或优先级调整。
- [ ] DirectML 只在真实目标 GPU/驱动上验收；CPU 与 GPU 结果分别记录，不能互相替代。

### 9.4 阶段门槛

- [ ] 普通 OCR：相同质量下 P50/P95 有可重复改善，或明确证明主要瓶颈在 ORT 模型而非 Rust 热路径。
- [ ] 公式 OCR：batch/profile 选择有数据；smoke 与 val-501 链路结果不变。
- [ ] 无新增单图硬失败、OOM、provider fallback 或输入限制回归。

---

## 10. 阶段 7：API、模块和文档清理

- [ ] 删除 `RuntimeBackend`（若阶段 2 确认只有 ONNX Runtime）。
- [ ] 删除 CANN 的配置、序列化、输出和文档残留。
- [ ] 根据阶段 3 决策删除或收敛 `VisionBackend`；删除无效的 `resolve_backend_or_pure_rust` fallback，核心路径使用严格错误。
- [ ] 清理 `RuntimeConfig` 中重复字段、无调用方字段和只为旧 API 保留的名字。
- [ ] 将普通 OCR、公式 OCR、共享输入/模型/runtime 的模块边界写入 `src/*/mod.rs` 和 README，避免再次把两条 pipeline 复制出来。
- [ ] 更新 crate metadata：repository 改为 `https://github.com/jinghu-moon/rapid-ocr-rs`，补齐 `rust-version`、categories/description，并检查 `cargo package --list`。
- [ ] README 只写 Windows 支持、CPU/DirectML/CUDA provider、OpenCV/turbojpeg 最终决策和准确验证命令。
- [ ] 增加 `CHANGELOG` 或开发期变更记录，明确这是破坏性平台收窄，不承诺 Linux/macOS。

### 7.1 静态清理门槛

```powershell
rg -n "linux|macOS|macos|CANN|cann-provider|unsupported platform|OpenCV|turbojpeg|RuntimeBackend" src README.md Cargo.toml docs
```

命中项必须全部归类为：当前实现、历史记录、第三方说明或待决策项；不能留下错误的当前支持声明或死代码。

---

## 11. 阶段 8：Windows 发布与最终验收

### 11.1 构建与包验收

- [ ] 在干净 Windows x64 clone 中执行 `cargo test --all-targets`、fmt、clippy、release build。
- [ ] 执行 `cargo package --allow-dirty=false`，检查包内没有模型、测试数据、开发机绝对路径或生成报告。
- [ ] 验证 default features：CPU 可运行，不下载隐式模型，不要求 OpenCV（除非阶段 3 明确决定保留并启用）。
- [ ] 验证 `cli-default`：按文档复制 ORT dylib，DirectML feature 与 Windows DLL 说明一致。
- [ ] 验证 CUDA feature：在有 NVIDIA 环境运行；无 CUDA 环境必须返回可定位错误或按普通 OCR 约定 fallback。
- [ ] 记录 Windows Defender/SmartScreen、DLL 搜索路径、VC runtime 和模型目录要求。

### 11.2 功能回归

- [ ] 普通 OCR：文件、encoded bytes、pixels、decoded image、URL（含限制和 timeout）各至少一条测试。
- [ ] 公式 OCR：独立识别、页面 route、JSON/Markdown/HTML、公式禁用时普通文本路径不变。
- [ ] 输出：reading order、多栏、空区域、非有限/退化检测框、超大输入、模型契约错误。
- [ ] provider：CPU、DirectML、CUDA 的 resolved/fallback/strict 语义。

### 11.3 最终指标

- [ ] 提交一份 Windows-only final report：基线/改后 CER、区域数、polygon IoU、P50/P95、峰值工作集、启动时间、二进制体积。
- [ ] full formula datasets 只在阶段 5/6 修改了模型调用、预处理、tokenizer、postprocess、batch/EOS 或指标实现时重跑；否则复用已锁定 baseline，并记录理由。
- [ ] 所有未实现或环境相关限制写入 README 的 Known limitations，不得用“支持”掩盖未验证 provider。

---

## 12. 推荐实施顺序与停线条件

### 12.1 推荐顺序

1. 阶段 0 基线冻结。
2. 阶段 1 Windows 编译边界和内存实现收窄。
3. 阶段 2 删除 CANN、删除单变体 RuntimeBackend。
4. 阶段 3 对 OpenCV/turbojpeg 做真实决策。
5. 阶段 4 根据决策合并视觉和输入路径。
6. 阶段 5 统一 RuntimeProfile 和线程模型。
7. 阶段 6 做有数据支撑的 OCR/公式性能优化。
8. 阶段 7 清理 API、文档和发布元数据。
9. 阶段 8 clean clone、打包和最终验收。

### 12.2 必须暂停并重新评估的情况

- Windows-only 改动导致普通 OCR CER、区域数或 polygon IoU 超出阶段 0 门槛。
- OpenCV/turbojpeg 删除后出现真实质量差异，或端到端 P95 退化超过 10%。
- 线程改动使 P95、峰值工作集或稳定性变差，即使平均吞吐上升。
- DirectML/CUDA 只能通过 CPU fallback“通过”时，必须标记 provider 不可用，不得继续优化该结果。
- 任何阶段需要重新运行 7 小时全量集时，先确认修改确实触及模型调用链，再运行受影响数据集，不把全量评测作为默认回归。

---

## 13. 阶段完成记录模板

每个阶段完成后追加以下记录：

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

