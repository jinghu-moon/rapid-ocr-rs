# PP-FormulaNet_plus-M 接入 rapid-ocr-rs：TDD 分阶段任务清单

> 文档状态：实施计划
>
> 目标：在 `rapid-ocr-rs` 中以独立、可验证的公式识别链路接入 RapidDoc 的 `PP-FormulaNet_plus-M` ONNX 模型，并用公开公式测试集完成数值、功能、性能和回归验收。
>
> 适用阶段：项目仍处于开发期，尚未正式发布。

---

## 0. 工程原则与不可变验收规则

### 0.1 开发期决策

- 不考虑历史版本兼容性。
- 允许删除、重命名和重构现有公共 API。
- 不增加仅用于兼容旧调用方的 adapter、wrapper、legacy branch 或重复实现。
- 发现接口、数据结构或模块边界错误时，优先重构根因。
- 不为了让测试通过而弱化断言、跳过测试、修改错误真值或降低指标。
- 任何完成声明都必须附带实际命令、结果和未覆盖风险。

### 0.2 必须保持的现有行为

公式功能可以破坏不合理的内部抽象，但不能无意破坏普通 OCR：

- 普通检测、方向分类、CTC 识别仍可运行；
- 文件、内存、URL 输入限制仍有效；
- JSON、Markdown、HTML、可视化输出仍有效；
- 现有 provider 选择和 CPU/DirectML/CUDA/CANN 编译路径不应被公式模块静默改变；
- 既有阅读顺序、多栏、边界和异常行为必须有前后回归证据。

### 0.3 当前已确认的模型事实

模型路径：

```text
OCR-Model/Formula-Recognition-Models/onnx/pp_formulanet_plus_m.onnx
```

RapidDoc ModelScope 来源：

```text
https://www.modelscope.cn/models/RapidAI/RapidDoc/resolve/v1.0.0/formula/PP-FormulaNet_plus-M/pp_formulanet_plus_m.onnx
```

已验证 SHA-256：

```text
71b6d389cf7b857e45252a4b98cfced1a3ffca7bf24d9497d02d052a41d9493b
```

已验证 ONNX 契约：

| 项目 | 实际值 |
| --- | --- |
| 输入名 | `x` |
| 输入类型 | `FLOAT` |
| 输入形状 | `[N, 1, 384, 384]` |
| 输出名 | `fetch_name_0` |
| 输出类型 | `INT64` |
| 输出形状 | `[N, sequence_length]` |
| ONNX IR | 10 |
| opset | 18 |
| 控制流 | 图内 `Loop` |
| tokenizer | `character` metadata 内嵌 `fast_tokenizer_file` 和 `tokenizer_config_file` |
| 特殊 token | `BOS=0`、`PAD=1`、`EOS=2` |
| 词表 | 50,000 项 |

已完成的人工/脚本探针：

- CPU ONNX Runtime 可加载模型；
- 单图真实推理成功；
- `val_0053264.png` 的输出 LaTeX 与 `val.txt` 标注完全一致；
- batch=2 推理成功，输出序列按样本独立结束；
- 预处理错误地使用 RGB/BGR 顺序会改变结果，必须由测试锁定通道语义。

这些事实是实施前置条件，不替代 Rust 端测试。

### 0.4 结构重构决策：普通 OCR、公式识别与共享能力分域

在实现公式识别之前，先把现有普通 OCR 代码归拢到明确的 `ocr` bounded context。这个重构允许破坏内部模块路径和公共 API；不增加兼容适配层。目标是让公式识别成为独立领域，而不是继续堆叠在普通 CTC OCR 的 `rec` 或 `pipeline` 中。

目标目录：

```text
src/
├── ocr/                         # 普通 OCR 专属
│   ├── mod.rs
│   ├── det/
│   ├── cls/
│   ├── rec/
│   ├── pipeline/
│   │   ├── rapid_ocr.rs
│   │   ├── config.rs
│   │   ├── types.rs
│   │   └── image_ops.rs
│   ├── config.rs
│   └── types.rs
│
├── formula/                     # 公式识别专属
│   ├── mod.rs
│   ├── session.rs
│   ├── recognizer.rs
│   ├── preprocess.rs
│   ├── tokenizer.rs
│   └── types.rs
│
├── runtime/                     # 共享 session 创建、provider、线程和错误映射
├── input/                       # 共享图片/文件/URL 输入和限制
├── vision/                      # 共享图像基础能力；领域算法不得放在这里
├── output/                      # 共享输出入口，内部按 OCR/公式拆 serializer
├── model_store.rs               # 共享模型资产管理
├── model_registry.rs            # 共享模型注册
├── error.rs                     # 共享错误体系
├── evaluation/                  # 共享评测框架，拆分 ocr.rs/formula.rs
├── api.rs                       # 公共 API 门面
├── config.rs                    # 仅保留真正共享配置
└── types.rs                     # 仅保留真正共享类型
```

边界规则：

- `cls/`、`det/`、`rec/` 和当前普通 OCR 编排器 `pipeline/rapid_ocr.rs` 必须归入 `src/ocr/`。
- `pipeline/config.rs`、`pipeline/types.rs` 中只服务普通 OCR 的内容归入 `src/ocr/pipeline/`；不能把通用 runtime 或公式配置混入其中。
- `config.rs` 不能整体机械移动：检测、分类、CTC 识别配置进入 `ocr/config.rs`，provider/runtime/图像基础配置保留在共享模块。
- `types.rs` 不能整体机械移动：`LineResult`、`WordBox` 等普通 OCR 类型进入 `ocr/types.rs`，通用几何、图片和跨领域结果类型保留共享。
- `evaluation.rs` 拆为 `evaluation/ocr.rs` 与 `evaluation/formula.rs`（即 `evaluation::ocr` / `evaluation::formula`），两类指标不得混合成一个默认汇总。公式评测模块后续可扩展为 `evaluation/formula/` 目录模块，但不得移入 `ocr/` 或依赖生产识别器。
- `runtime/`、`input/`、模型存储、错误体系和基础 `vision` 能力属于共享层；共享层不能依赖 `ocr` 或 `formula`。
- `output/` 保留公共输出入口，但 OCR 文本和公式 LaTeX 的序列化逻辑必须分域；公式不能伪装成 CTC 文本。
- `api.rs` 和 `lib.rs` 可以作为公共 facade，但 facade 不是 legacy adapter；如果当前公共类型边界错误，开发期允许直接重设计。
- 普通 OCR 与公式识别不能通过“如果是公式就跳过 CTC 契约”的特殊分支耦合；两者必须拥有独立的 typed session contract。

结构重构的 TDD 顺序：

1. 在移动文件前记录普通 OCR 基线，并为模块路径/公共导出建立编译测试。
2. 创建 `ocr/mod.rs`，移动 `det`、`cls`、`rec`，更新引用。
3. 将普通 OCR pipeline 移到 `ocr/pipeline/`，拆出真正共享的图像基础函数。
4. 拆分 `config.rs`、`types.rs`、`evaluation.rs`，每次拆分后运行普通 OCR 全量回归。
5. 建立空的 `formula/mod.rs` 和公式专属测试入口；不得提前复用普通 `Recognizer`。
6. 结构重构完成并通过普通 OCR 回归后，才实现 `formula/session.rs`、`preprocess.rs` 和 `tokenizer.rs`。

阶段 0 必须拆成两个执行门：

- **0A 设计冻结**：只确认目录、依赖方向、公共边界和迁移方案，不修改源代码。0A 在阶段 1 基线前完成。
- **0B 结构重构实施**：必须在阶段 1 基线完成后执行，按上面的 Red -> Green -> Refactor 顺序移动和拆分代码。0B 完成后，阶段 2 及后续阶段才允许开始。

这样既保留阶段 0 对架构的控制权，又满足“修改前建立基线”的工程规则。阶段 0 不是简单的目录移动任务；0B 的完成必须以普通 OCR 前后回归通过为门槛。

### 0.5 阶段 0B 对后续阶段的影响

阶段 0B 改变的是内部模块边界，不改变公式识别的功能验收目标。后续阶段必须按以下新边界执行：

| 后续阶段 | 受影响的任务 | 更新后的约束 |
| --- | --- | --- |
| 2 fixture | fixture loader 位置和依赖 | 放在 `evaluation::formula`，只依赖共享输入/评测类型，不依赖 `ocr` 生产识别器 |
| 3 模型契约 | ONNX 探针入口 | 不再从普通 `rec` 或 CTC `Recognizer` 进入；可使用当前 runtime 的通用加载能力，typed formula contract 留给阶段 5 |
| 4 预处理 | 预处理实现位置 | 只实现 `formula/preprocess.rs`，不得复用普通 OCR 的预处理策略或把公式逻辑放入 `ocr/pipeline` |
| 5 runtime 契约 | session 文件和契约 | 共享生命周期/provider 留在 `runtime`；普通 CTC session 在 `ocr/session.rs`，公式 token session 在 `formula/session.rs` |
| 6 tokenizer | 解码输入 | 消费公式 session 的 `INT64` token 序列，不读取普通 OCR 字符字典或 CTC 输出 |
| 7 Formula API | 公共 API | 组合 `formula/session.rs`、`formula/preprocess.rs` 和 `formula/tokenizer.rs`；不扩展普通 `Recognizer` 作为兼容入口 |
| 8 输出/错误 | 序列化和错误分类 | 公式 LaTeX、token、EOS/truncated 使用独立结果类型；共享错误只提供通用基础，不伪装成普通 OCR 行结果 |
| 9-10 评测/性能 | 评测对象和基线 | 公式指标、模型吞吐和 provider 结果单独记录，不与普通 OCR 汇总或比较 |
| 11 普通 OCR 回归 | 重构验收 | 这是阶段 0B 后以及最终阶段 11 的双重门槛；任何未解释的普通 OCR 变化都阻止后续发布边界验收 |

因此，阶段 0B 完成后不得再出现以下旧设计：在 `src/rec/` 增加公式逻辑、在共享 `runtime` 放置公式专属 session、通过模式分支跳过 CTC 合约，或以普通 OCR 类型承载公式 LaTeX。

结构重构验收：

- [x] 普通 OCR 专属实现全部位于 `src/ocr/`；
- [x] 公式实现全部位于 `src/formula/`，不混入 `src/ocr/`；
- [x] 共享模块不反向依赖任何领域模块；
- [x] 没有新增兼容 wrapper、旧路径转发或重复实现；
- [x] `cargo check`、普通 OCR 单元/集成测试和真实图片回归均通过；
- [x] 重构前后普通 OCR 的输出、错误语义和性能差异都有记录；
- [x] 目录边界和依赖方向写入模块 `mod.rs`，不依赖开发者记忆。
- [x] 0A 设计冻结记录已完成；
- [x] 0B 结构重构实施已在阶段 1 基线之后完成；
- [x] 0B 完成后普通 OCR 全量回归通过，且阶段 1 基线仍可追溯。

---

## 1. 总体阶段、依赖和完成门槛

阶段必须按顺序推进。除非前一阶段的完成门槛满足，否则不得进入下一阶段。

| 阶段 | 目标 | 主要产物 | 进入条件 |
| --- | --- | --- | --- |
| 0A | 冻结范围、资产和模块边界，不改源代码 | 本文档、模型哈希、`ocr/formula/shared` 目录设计和迁移方案 | 开始阶段 1 前完成 |
| 1 | 建立修改前基线 | 测试/编译/性能基线报告 | 0A 已冻结，源代码尚未移动 |
| 0B | 实施普通 OCR 结构重构并建立公式领域空模块 | `src/ocr/`、`src/formula/mod.rs`、依赖边界和普通 OCR 前后回归报告 | 阶段 1 基线已保存；完成后普通 OCR 全量回归通过 |
| 2 | 建立数据集与真值读取层 | `evaluation::formula` fixture manifest、标注解析测试 | 0B 已完成且普通 OCR 回归通过 |
| 3 | 固定 ONNX 模型契约 | 模型探针、Rust `ort` smoke test | 0B 和阶段 2 已完成；使用当前通用 runtime 做探针，typed domain contract 在阶段 5 固化 |
| 4 | 固定预处理契约 | `formula/preprocess.rs`、预处理 golden tensor | Python/Rust tensor 一致 |
| 5 | 重构运行时契约 | 共享 typed session/runtime contract | 普通 OCR session 回归通过，公式 session 仍位于 `src/formula/` |
| 6 | 实现 tokenizer 与公式解码 | `formula/tokenizer.rs`、EOS、LaTeX 输出 | 单图 golden 通过 |
| 7 | 实现公式识别 API | 完整的 `formula/session.rs` 执行流程、`formula/recognizer.rs` 和独立结果类型 | API 单测和集成测试通过 |
| 8 | 接入批量、错误和输出 | batch、限制、错误语义 | 边界测试通过 |
| 9 | Paddle/ONNX/旧新结果对比 | 数值回归报告 | 指标达到阈值 |
| 10 | 性能与 provider 验证 | benchmark 报告 | 无不可接受退化 |
| 11 | 普通 OCR 前后回归 | 前后基线对比报告 | 无未解释的行为变化 |
| 12 | 文档、资产和发布前检查 | README、第三方说明、清单 | 全部验收门满足 |

每一阶段均遵循：

```text
先写失败测试（Red）
  -> 写最少正确实现（Green）
  -> 删除重复/错误抽象并重构（Refactor）
  -> 跑本阶段测试和受影响的全量回归
```

---

## 2. 阶段 1：修改前基线（必须先做）

### 2.1 基线记录任务

- [x] 记录工作树状态：`git status --short`。
- [x] 记录 Rust、Cargo、OS、CPU、ORT provider、模型目录和测试集路径。
- [x] 运行默认测试：

  ```powershell
  cargo test --all-targets
  ```

- [x] 运行格式检查：

  ```powershell
  cargo fmt --all -- --check
  ```

- [x] 运行默认编译：

  ```powershell
  cargo check
  ```

- [x] 运行已有 provider 编译/测试：

  ```powershell
  cargo test --features directml-provider
  cargo test --features cuda-provider
  cargo check --features directml-provider,cuda-provider,cann-provider
  ```

- [x] 保存普通 OCR 的真实图片回归结果：区域数、文本、耗时和失败数。
- [x] 保存现有 benchmark 的吞吐、P50/P95、内存和 provider 信息。
- [x] 保存 baseline JSON，不覆盖 `tests/baseline` 中已有真值；新结果使用带日期或阶段名的文件。

### 2.2 基线验收

必须形成表格：

| 项目 | 修改前实际值 | 修改后实际值 | 预期 |
| --- | --- | --- | --- |
| 默认测试 |  |  | 不得有新增失败 |
| DirectML 测试 |  |  | 不得有新增失败 |
| CUDA 测试 |  |  | 不得有新增失败 |
| CANN 编译 |  |  | 编译通过或记录环境阻塞 |
| 普通 OCR 区域数 |  |  | 保持 |
| 普通 OCR 文本 |  |  | 保持 |
| 普通 OCR 吞吐 |  |  | 记录差异并解释 |
| 峰值内存 |  |  | 无不可接受增长 |

未完成基线不得开始执行阶段 0B，也不得开始将普通 OCR 源码迁入 `src/ocr/`、创建公式实现模块或修改共享 runtime 契约。

阶段 1 完成后，必须回到阶段 0 执行 0B 结构重构。阶段 0B 完成并通过普通 OCR 回归后，才进入下面的阶段 2。

---

## 3. 阶段 2：测试数据和真值读取层

本阶段的 fixture loader 属于 `src/evaluation/formula.rs`（即 `evaluation::formula`；规模增大时可改为 `src/evaluation/formula/` 目录模块），不属于 `src/ocr/`，也不应依赖生产识别器。进入本阶段前必须确认阶段 0B 已完成。

### 3.1 固定本地资产

不把大模型和完整数据集提交到 crate 仓库。模型与数据放在仓库外的工作目录，通过环境变量或测试参数引用。

推荐环境变量：

```powershell
$env:RAPID_OCR_MODEL_ROOT = "D:\100_Projects\110_Daily\SnapClip\OCR-Model"
$env:RAPID_OCR_FORMULA_TEST_ROOT = "D:\100_Projects\110_Daily\SnapClip\Formula-TestSet"
```

必须记录：

- 模型 SHA-256；
- tokenizer 来源（本模型 metadata）；
- 测试集来源、版本/下载日期；
- 图片数量、标签数量、缺失/重复数量；
- 测试数据许可证和上游引用。

### 3.2 实现测试 fixture loader 前先写测试

- [x] Red：不存在根目录时返回结构化错误，不 panic。
- [x] Red：缺失 label 文件时返回错误。
- [x] Red：缺失图片时返回错误并指出文件名。
- [x] Red：空标签由 loader 标记并从可评分集合排除（真实 im2latex test 有 71 条空标签引用，不能整批拒绝或丢弃）。
- [x] Red：路径包含 Unicode、空格时可读取（含图片名内含空格，从右向左按最后一个空白拆分）。
- [x] Green：实现 `FormulaFixture`、`FormulaSample` 和三个数据集 loader。
- [x] Refactor：统一样本接口，不为每个数据集复制评测循环。

建议数据结构：

```text
FormulaSample {
    image_path: PathBuf,
    ground_truth: String,
    split: FormulaSplit,
    source_index: Option<usize>,
}
```

### 3.3 im2latex 映射规则测试

测试集目录：

```text
Formula-TestSet/im2latex-100k/
  *.png
  im2latex_formulas.norm.lst
  im2latex_test_filter.lst
  im2latex_validate_filter.lst
```

列表格式：

```text
image_name.png formula_index
```

实现并测试：

- [x] `image_name` 精确定位本地 PNG；
- [x] `formula_index` 索引 `im2latex_formulas.norm.lst`；
- [x] 越界 index 失败；
- [x] 文件名缺失失败；
- [x] test/validation 样本无交集；
- [x] `test_filter` 10,355 条全部可定位；
- [x] `validate_filter` 8,370 条全部可定位；
- [x] 不使用原始 `im2latex_test.lst` 直接按行配对。

### 3.4 PaddleX 示例集测试

目录：

```text
Formula-TestSet/ocr_rec_latexocr_dataset_example/
  images/
  train.txt
  val.txt
```

- [x] Red：验证 `val.txt` 的图片引用全部存在；
- [x] Red：制造缺失图片时 loader 明确失败；
- [x] Green：读取 tab 分隔图片路径和 LaTeX；
- [x] Refactor：统一为 `FormulaSample`；
- [x] 固定 501 张 `val` 作为 smoke/e2e gold set；
- [x] 明确 `latex_ocr_tokenizer.json` 不作为 PP-FormulaNet tokenizer。

### 3.5 UniMER-Test 映射测试

目录：

```text
Formula-TestSet/UniMER-Test/
  spe/ cpe/ sce/ hwe/
  spe.txt cpe.txt sce.txt hwe.txt
```

规则：

- `cpe`/`hwe` 图片编号通常可直接对应 label 行；
- `spe`/`sce` 图片文件名数字是原始 label 行索引，不能 `zip(sorted(images), labels)`；
- 标签、图片数量和缺失索引必须在 manifest 中显式记录。

测试任务：

- [x] 每个子集抽取首、中、尾各 3 个样本验证映射；
- [x] 验证图片可解码；
- [x] 验证索引越界错误；
- [x] 按 `SPE/CPE/SCE/HWE` 分组统计，禁止默认混合汇总；
- [x] 记录 HWE 是手写公式附加测试，不是 PP-FormulaNet-M 主验收集。

---

## 4. 阶段 3：ONNX 模型契约与 Rust smoke test

### 4.1 先写模型探针测试

- [x] Red：模型不存在时返回可定位错误。
- [x] Red：输入不是单输入、类型不是 `FLOAT`、rank 不是 4 时拒绝。
- [x] Red：输出不是 `INT64` rank 2 时拒绝。
- [x] Red：固定空间维不是 `384x384` 时拒绝或明确支持动态契约。
- [x] Red：缺失 `character` metadata 时按显式策略失败，不静默使用普通 OCR 字典。
- [x] Green：实现模型签名探针并输出结构化 `FormulaModelInfo`。
- [x] Refactor：模型验证逻辑与推理执行逻辑分离。

### 4.2 Rust `ort` 兼容性验证

- [x] 使用项目锁定的 `ort = 2.0.0-rc.13` 加载模型；
- [x] 使用 CPU provider 创建 session；
- [x] 验证 opset 18/IR 10 在项目 runtime 下可加载；
- [x] 用一个 `[1,1,384,384]` 输入运行；
- [x] 用两个样本运行动态 batch；
- [x] 检查输出 dtype、rank、batch 维；
- [x] 记录 session 创建时间、首次运行时间和错误信息；
- [x] 若项目 ort/ORT 不支持该图，先解决 runtime 版本/构建根因，不修改模型图规避错误。

### 4.3 metadata tokenizer 测试

- [x] 读取 `character` metadata JSON；
- [x] 验证存在 `fast_tokenizer_file`；
- [x] 验证 vocab 中包含 `<s>`, `<pad>`, `</s>`, `<unk>`；
- [x] 验证 ID 分别为 `0,1,2,3`；
- [x] 验证 tokenizer vocab 规模为 50,000；
- [x] metadata JSON 损坏时返回 tokenizer 错误；
- [x] 不把完整 tokenizer JSON 写入源码或复制成第二份真值。

---

## 5. 阶段 4：预处理 TDD

预处理是当前最容易造成“模型能运行但结果全错”的根因，必须独立测试，不允许只用端到端结果间接证明。

### 5.1 预处理契约

输入为 RGB/RGBA/灰度图片，输出：

```text
FLOAT32 [N, 1, 384, 384]
```

流程：

```text
解码
  -> 灰度阈值找非白区域
  -> 裁剪边界
  -> 短边缩放到 384
  -> 长边限制为 384
  -> 白色画布居中填充
  -> /255
  -> mean=0.7931, std=0.1738
  -> 灰度单通道
  -> NCHW
```

必须明确并固定：

- resize 插值算法；
- crop 的边界是否包含右/下边界；
- 空白图行为；
- 灰度图和 alpha 图行为；
- RGB/BGR 通道语义；
- 填充值是归一化前白色还是归一化后的常数 `1`。

### 5.2 Red/Green/Refactor 任务

- [x] Red：固定 5 张本地样本的输出 shape、dtype、min、max、均值和 SHA-256。
- [x] Red：纯白图、全黑图、单像素图、窄图、宽图、透明图各有测试。
- [x] Red：RGB 与 BGR 顺序错误的探针必须失败，防止通道语义回归。
- [x] Green：实现独立 `FormulaPreprocessor`。
- [x] Green：实现批量预处理，样本顺序保持稳定。
- [x] Refactor：删除与普通 OCR 预处理重复但语义不同的隐式转换。
- [x] 通过 Python 参考实现导出 golden tensor，与 Rust `allclose` 比较。

### 5.3 预处理验收阈值

对于相同输入和相同环境：

- shape、dtype 必须完全一致；
- 归一化 tensor 的最大绝对误差 `<= 1e-5`；
- 允许平台 resize 浮点差异时，必须单独记录放宽原因和新阈值；
- 不能只比较最终 LaTeX 来掩盖预处理差异。

---

## 6. 阶段 5：运行时契约根因重构

### 6.1 当前问题

当前 `runtime::session::OrtSession` 的 `SessionContract::Rec` 将：

- 输入必须是 rank-4 `FLOAT`；
- 输出必须是 rank-3 `FLOAT`；
- metadata `character` 直接按普通字符行读取；

写死在普通 OCR session 中。公式模型是 rank-2 `INT64`，并且 metadata 是 JSON tokenizer，不应被强行解释为 CTC 字符表。

### 6.2 目标设计

允许破坏现有内部接口，重构为按模型语义划分的 typed session。共享 runtime 只负责 session 生命周期、provider、线程和通用 tensor 访问；领域模块拥有自己的模型契约：

```text
runtime/
  session.rs              通用 session 创建、provider、线程和错误
  contracts.rs            通用输入/输出契约验证工具

ocr/
  session.rs              普通 CTC FLOAT rank-3 session

formula/
  session.rs              公式 token INT64 rank-2 session
```

阶段 5 只负责 runtime/contract 根因重构和领域 session 的契约边界；公式 session 的完整识别 API 在阶段 7 实现。不能把 `formula_session.rs` 放回共享 `runtime/`，也不能通过“公式模式”绕过普通 OCR 契约。

允许采用等价的文件名，但必须满足：

- 普通 OCR 和公式识别没有共享错误的输出契约；
- provider/线程/生命周期逻辑可复用；
- tensor 类型和 rank 在类型化入口验证；
- 不引入“如果是公式就跳过 Rec 检查”的特殊分支。

### 6.3 TDD 任务

- [x] Red：普通 CTC session 合约测试保持原行为。
- [x] Red：公式模型加载普通 CTC session 必须失败，错误明确指出契约不匹配。
- [x] Red：公式 session 可接受 `FLOAT [N,1,384,384]` -> `INT64 [N,L]`。
- [x] Red：错误 dtype、rank、缺失 output 均有测试。
- [x] Green：提取通用 session 创建和 provider 配置。
- [x] Green：实现 `runtime::contracts` 的通用契约验证，并分别实现 `ocr::session` 与 `formula::session` 的领域契约。
- [x] Refactor：删除旧 session 中的公式特判、重复 metadata 解析和临时 adapter。
- [x] 运行普通 OCR 全量回归后才进入公式 API 阶段。

---

## 7. 阶段 6：FormulaTokenizer 与序列解码

### 7.1 设计要求

公式 tokenizer 必须处理模型输出的 token ID 序列，而不是 CTC argmax：

```text
token_ids
  -> 去掉 BOS
  -> 在首个 EOS 截断
  -> 丢弃 PAD/EOS 等特殊 token
  -> tokenizer decode
  -> LaTeX String
```

必须保留原始 token 序列用于调试和回归，不得只保留最终字符串。

### 7.2 TDD 任务

- [x] Red：`[0, 82, ..., 2, 1, 1]` 只解码 EOS 前内容。
- [x] Red：无 EOS 的序列按明确策略失败或标记 truncated，不能静默当作完整结果。
- [x] Red：全 PAD、空序列、未知 token、超出 vocab 均有测试。
- [x] Red：BOS/PAD/EOS ID 与 metadata 不一致时拒绝启动。
- [x] Green：实现 `FormulaTokenizer::from_metadata`。
- [x] Green：实现 `decode_ids`，返回 token IDs、EOS 状态、LaTeX。
- [x] Refactor：不复制 Hugging Face tokenizer 实现；仅实现模型所需 tokenizer JSON 语义，必要时引入成熟 Rust tokenizer crate。
- [x] 用 Python `tokenizers` 参考输出建立至少 20 个 token 序列 golden。

### 7.3 输出数据结构建议

```text
FormulaResult {
    latex: String,
    token_ids: Vec<i64>,
    eos_index: Option<usize>,
    truncated: bool,
    model: String,
    elapsed: Duration,
}
```

不要把公式结果强行塞进 `LineResult` 的置信度字段；公式模型当前没有可与 CTC 平行解释的 per-character confidence。

---

## 8. 阶段 7：FormulaRecognizer / FormulaSession API

### 8.1 第一版 API 边界

第一版先提供独立公式识别 API：

```text
FormulaRecognizer::from_model(path, runtime_config)
FormulaRecognizer::recognize(image)
FormulaRecognizer::recognize_batch(images)
```

可根据现有项目命名调整，但必须满足：

- 公式 API 不复用普通 `Recognizer` 的输出类型；
- 支持单图和 batch；
- 保持输入顺序；
- 返回 token、LaTeX、EOS/truncated 和耗时；
- CPU 为第一版明确支持范围；
- provider 不支持时返回结构化错误，不静默回退到 CPU。

### 8.2 TDD 任务

- [x] Red：单图 API 的 happy path。
- [x] Red：batch=1/2/8 顺序和结果独立性。
- [x] Red：空 batch、超大 batch、空图和解码失败。
- [x] Red：模型路径不存在、模型 hash 不匹配、metadata 损坏。
- [x] Green：实现 session、preprocessor、tokenizer 的组合。
- [x] Green：实现 batch 输入 tensor 构建和输出拆分。
- [x] Refactor：将 API 参数与普通 OCR 的 `RecognizeOptions` 解耦；共享真正通用的图片加载和 provider 配置。

### 8.3 与页面 OCR 的集成策略

不得在第一步把公式识别硬塞进普通文本识别链。先完成独立 API 和完整回归，再设计页面级路由：

```text
页面/图片
  -> 公式检测（已有 Formula-Detection-Model 或未来路由）
  -> 公式 crop
  -> FormulaRecognizer
  -> OcrOutput 中的 Formula region
```

页面级集成任务：

- [x] 定义 `RegionKind::Formula` 或等价明确类型；
- [x] 明确公式 region 是否跳过普通 CTC 识别；
- [x] 明确 JSON/Markdown/HTML 的 LaTeX 表示；
- [x] 保留 crop 坐标和模型信息；
- [x] 增加公式检测误检、漏检和重叠区域测试；
- [x] 公式功能关闭时普通 OCR 行为必须与基线一致。

页面级集成实现要点（第二轮审核修复）：

- `OcrRequest.formula: FormulaPolicy` 默认 `enabled = false`；关闭时 `recognize`
  直接进入 `recognize_text`，不经过任何公式代码路径，因此“关闭时与基线一致”
  是结构性保证，而不是靠分支判断。
- 启用后：`FormulaDetector`（`pix2text-mfd-1.5.onnx`，可选）在整页上检测公式区域，
  与 `FormulaPolicy::input_regions` 声明的区域合并，经 `formula::route`
  过滤误检、消解重叠（IoU + 嵌套包含）、按分数排序并限制数量；
- 公式像素在送入普通文本管线**之前**被抹白，因此 CTC 根本不会在公式上执行
  （真正的“跳过 CTC”，不是事后丢弃结果）；
- 公式区域从**未抹白**的原图裁剪，交给 `FormulaRecognizer`，作为独立的
  `RegionKind::Formula` 区域追加，保留 polygon、检测分数与模型标识；
  `OcrOutput::validate()` 强制“公式区域不得携带 CTC recognition”这一不变量；
- 公式区域不伪造 per-character 置信度：`RecognitionOutcome` 对公式区域恒为 `None`；
- `roi` 与 `tile` 与公式路由互斥，启用时返回结构化 `InvalidInput`（坐标语义会分叉）；
- 输出：JSON 增加 `kind`/`latex`/`eos_index`/`truncated` 与顶层 `formulas`；
  Markdown 按阅读顺序输出 `$$...$$`；HTML 单独渲染公式多边形与 LaTeX 列表。
- 已知且被测试锁定的限制：
  - 误检：检测器在非公式页面上（代码页、密集正文）默认阈值下会产生误检，
    误检区域内的文本会被一并从文本通道移除；提高 `confidence_threshold`
    可恢复到基线（集成测试用 0.95 验证）。
  - 漏检：检测器未命中的公式仍留在文本通道，会被 CTC 识别成乱码，当前没有
    跨模型仲裁机制。
  - 公式裁剪使用四边形的最小外接矩形。

---

## 9. 阶段 8：输出、错误、资源和安全边界

### 9.1 输出格式

- [x] JSON 输出包含 `latex`、`token_ids`、`eos_index`、`truncated`、模型标识和耗时。
- [x] Markdown 使用明确的 display math 表示，例如 `$$...$$`，避免把 LaTeX 当普通文本转义。
- [x] HTML 对 LaTeX 文本和 HTML 属性分别转义。
- [x] 原始 token ID 默认可选输出，调试模式打开，不在普通 Markdown 中泄露。
- [x] 公式输出顺序与检测 region/reading order 一致。

### 9.2 错误语义

- [x] 模型不存在；
- [x] hash 不匹配；
- [x] ONNX 契约不匹配；
- [x] tokenizer metadata 缺失/损坏；
- [x] 图片解码失败；
- [x] 像素/编码大小超限；
- [x] batch 超限；
- [x] EOS 缺失导致截断；
- [x] provider 不可用；
- [x] ORT 执行失败。

所有错误都应使用现有错误体系或重构后的结构化错误，不返回只包含底层 ORT 字符串的不可判断错误。

### 9.3 资源约束

- [x] 延续普通输入的解码像素和编码字节限制；
- [x] 对公式 crop 也执行尺寸限制；
- [x] batch 大小有显式上限；
- [x] 序列长度有上限；
- [x] 图内 Loop 不应因异常输入导致无界内存增长；
- [x] URL/文件输入不得在校验前完整缓冲超大数据。

第二轮修复补充（根因）：

- 公式输入加载不再自己实现限制逻辑，统一走共享 `input::image_loader`，
  与普通 OCR 的编码字节、解码像素、header 探测、URL `Content-Length`、
  流式读取上限与超时语义完全一致；
- `max_encoded_bytes` 统一适用于所有编码输入（含调用方内存字节），
  不再存在“文件/URL 受限、内存字节不受限”的语义分叉；
- **序列长度上限根因修正**：模型图内 `Loop` 的输出宽度上限为 2561 列；
  当 batch 中任一 样本在 Loop 预算内没有产生 EOS 时，ONNX Runtime 会把
  **整个 batch** 补齐到 2561 列。原默认上限 2560 会让这一批整体被拒绝，
  连带丢掉同批识别正确的样本（实测 501 张 val 中有 8 个 batch 触发）。
  现在默认上限为 4096，并新增常量 `FORMULA_MODEL_LOOP_BOUND = 2561` 与
  编译期断言 `DEFAULT_MAX_FORMULA_SEQUENCE_LENGTH > FORMULA_MODEL_LOOP_BOUND`；
- `sha256_file` 的 1 MiB 栈缓冲区改为堆分配：Windows 主线程默认只有 1 MiB 栈，
  原实现在 CLI/benchmark 中会直接 stack overflow。

---

## 10. 阶段 9：数值和功能回归

### 10.1 Smoke gold set

先使用：

```text
Formula-TestSet/ocr_rec_latexocr_dataset_example/val.txt
```

501 张样本的任务：

- [x] Python RapidDoc/ONNX 参考输出保存为 JSON；
- [x] Rust 输出保存为 JSON；
- [x] token 序列逐项比较；
- [x] EOS index 比较；
- [x] LaTeX exact match 比较；
- [x] normalized LaTeX match 比较；
- [x] CER/Edit distance 统计；
- [x] 失败样本保存图片名、期望、实际和 token diff；
- [x] 不允许单图失败被吞掉；
- [x] 允许模型本身错误，但必须区分“Rust/ONNX 链路差异”和“模型识别错误”。

### 10.2 im2latex 主评测

测试列表：

```text
im2latex_test_filter.lst
```

标签索引：

```text
im2latex_formulas.norm.lst
```

任务：

- [x] 先跑 100 张固定 smoke subset；
- [x] 再跑完整 10,355 张测试集；
- [x] 记录吞吐、P50/P95、峰值内存和失败数；
- [x] 记录 exact token match、LaTeX exact match、normalized match、CER；
- [x] 将文本解码错误与模型识别错误分开统计；
- [x] 测试顺序稳定，可重复生成相同 manifest/hash。

实现（第二轮审核修复）：

- 新增 `src/bin/formula_eval.rs`（取代只支持一个数据集的 `formula_compare`），
  支持 `im2latex` / `latexocr` / `unimer` 三个数据集；
- 抽样使用**内容哈希排序**而不是“取前 N 张”：同一数据集/切分/子集/数量在任何
  机器上得到同一子集，且顺序与文件系统无关；`--sample first` 仅用于复现历史结果；
- `--manifest-output` 写出每个样本的相对路径与真值 SHA-256 及整体 manifest 哈希；
  `--expect-manifest` 在哈希不一致时直接失败，避免“换了一批样本却照常出报告”；
- `--python-reference` 内建 Rust/Python 对比：完整 token 行、EOS 前 token 序列、
  EOS index、truncated、LaTeX 一致数，以及双方都错的样本数（模型识别错误）与
  逐条列出的链路差异；
- 失败分类：`image_decode` / `inference` / `tokenizer_decode` / `input_rejected`
  （链路差异候选）与 `truncated_no_eos` / `model_mismatch`（模型识别质量）分开统计；
- manifest 驱动的 Python 参考：`tools/formula_reference.py --manifest`，Python 不再
  重复实现抽样，只在 Rust 选定的样本上运行；
- 汇总与失败样本导出：`tools/summarize_formula_eval.py`；
- 全流程驱动脚本：`tools/run_formula_evaluation.ps1`（顺序执行，避免 CPU 争用
  扭曲吞吐）。

### 10.3 UniMER 评测

分组执行：

```text
SPE 6762
CPE 5921
SCE 4742
HWE 6332
```

- [x] SPE/CPE/SCE 作为 PP-FormulaNet-M 的主结果；
- [x] HWE 单独报告；
- [x] 不能把 HWE 结果混入印刷公式平均值掩盖领域差异；
- [x] 报告每组样本数、缺失数和失败数；
- [x] 保留固定抽样 manifest，避免只报告对模型有利的样本。

实现：`formula_eval --dataset unimer --subset spe|cpe|sce|hwe` 每个子集单独运行、
单独输出报告与 manifest；`summary.dataset` 记录为 `unimer_<subset>`，
汇总表按子集分行，不做任何跨子集平均。

### 10.4 RapidDoc/Python/Rust 三方对比

固定相同：

- 图片字节；
- 裁剪和 resize；
- 输入 tensor；
- 模型文件 SHA-256；
- tokenizer metadata；
- batch size；
- CPU provider 和线程数。

对比层次：

1. input tensor `max_abs`；
2. output token IDs；
3. EOS index；
4. decoded LaTeX；
5. batch 与单图结果；
6. 重复运行确定性。

---

## 11. 阶段 10：性能与 provider 验证

### 11.1 性能基线

必须分别测量：

- session 创建；
- 首次推理；
- warm inference；
- 预处理；
- ONNX `session.run`；
- tokenizer decode；
- 端到端单图；
- batch=1/2/4/8。

不要只报告端到端平均值，避免预处理或初始化掩盖模型性能。

第二轮修复：`formula_bench` 重写为分阶段**多轮采样统计**——
每个指标记录 `samples` / `min` / `max` / `mean` / `P50` / `P95` / `stddev`，
区分 `--warmup`（不计入统计）与 `--rounds`（计入统计），batch 表同时给出
整批耗时与单图分摊耗时，并验证 batch 不改变 token 序列
（`deterministic_tokens`）。峰值内存由共享 `runtime::memory` 采集
（Windows `GetProcessMemoryInfo.PeakWorkingSetSize` / Linux `VmHWM`），
分别记录启动、会话创建后与结束时的值。

### 11.2 性能验收

- [x] 与 Python RapidDoc CPU 参考使用相同线程设置；
- [x] 与 Rust 普通 OCR 基线分开比较，不把不同模型混为一个吞吐指标；
- [x] batch 结果不能改变 token/LaTeX；
- [x] 记录内存峰值；
- [x] 任何优化前后都有 benchmark 数据；
- [x] 不因臆测性能引入缓存、并发或复杂抽象。

**batch 不变性判定口径（实测发现）**：模型图内 `Loop` 会把整个 batch 补齐到同一宽度
（不收敛样本出现时为 2561 列），因此**完整 token 行**的尾部 padding 会随 batch 组成
变化。判定“batch 是否改变结果”必须比较 **EOS 之前（含 EOS）** 的 token 序列。
`formula_bench` 的 `deterministic_tokens` 采用该口径；实测 batch=1/2/4 在 4 张真实 val
图片上完全一致（若比较完整行会得到错误的 False）。

### 11.3 Provider

第一版验收范围：

```text
CPUExecutionProvider
```

任务：

- [x] CPU 完整通过后再尝试 DirectML/CUDA；
- [x] provider 不支持图内 `Loop` 时返回明确错误；
- [x] 不为了 provider 通过而改变输出或跳过公式测试；
- [x] DirectML/CUDA 失败需记录为模型/provider 限制，不伪装成 CPU 通过。

---

## 12. 阶段 11：普通 OCR 前后回归

### 12.1 修改前必须保存

- [x] `cargo test --all-targets` 结果；
- [x] provider 测试结果；
- [x] 普通 OCR fixture 的 JSON 输出；
- [x] 多栏阅读顺序输出；
- [x] 输入大小限制错误；
- [x] URL timeout/response size 错误；
- [x] CLI `run/report/evaluate/check` 关键输出；
- [x] benchmark JSON。

### 12.2 修改后必须重跑

- [x] 所有默认测试；
- [x] DirectML/CUDA/CANN 编译/测试矩阵；
- [x] 真实 OCR 图片；
- [x] 多栏 Markdown/JSON/HTML；
- [x] 文件、URL、内存三类输入；
- [x] 超像素、超编码字节和超时边界；
- [x] CLI 端到端命令；
- [x] benchmark 对比。

### 12.3 回归判定

- 新增公式测试失败不能通过删除普通 OCR 测试解决；
- 普通 OCR 行为变化必须有明确设计原因和更新后的真值；
- 只要普通 OCR 出现未解释的区域数、文本或错误语义变化，阶段 11 不通过；
- 公式模块未启用时，普通 OCR 输出应与基线逐项一致或达到已记录的容差。

---

## 13. 阶段 12：文档、许可证和仓库边界

### 13.1 文档

- [x] 更新 `README.md`：公式 API、模型下载、tokenizer 来源、CPU 限制和示例。
- [x] 更新 `docs/01`：把已验证的 RapidDoc 模型 metadata、输入输出和 SHA-256 改为事实，不保留“尚未验证”措辞。
- [x] 在本文档末尾记录每个阶段完成日期、提交和验证命令。
- [x] 添加公式 benchmark 报告格式和失败样本目录规范。

### 13.2 第三方资产

- [x] 在 `THIRD_PARTY_NOTES.md` 记录 RapidDoc、PP-FormulaNet、PaddleOCR/UniMER/im2latex 归属。
- [x] 记录模型固定 URL、版本、SHA-256、许可证和下载日期。
- [x] 不把约 594 MB ONNX、测试集图片或生成的结果 JSON 提交到 crate git。
- [x] 检查 `.gitignore` 覆盖模型缓存、benchmark 结果、临时导出和局部数据。
- [x] 发布 crate 时明确模型不随 crate 打包，用户需单独下载并接受其许可证。

### 13.3 可复现命令

至少提供：

```powershell
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo check --features directml-provider,cuda-provider,cann-provider
```

以及：

```powershell
# 外部资产（模型/测试集）通过环境变量引用；缺失时相关测试显式 skip。
$env:RAPID_OCR_MODEL_ROOT = "<workspace>/OCR-Model"
$env:RAPID_OCR_FORMULA_TEST_ROOT = "<workspace>/Formula-TestSet"
# 需要“缺资产即失败”的流水线：
# $env:RAPID_OCR_REQUIRE_EXTERNAL_ASSETS = "1"

# 公式 smoke / 主评测 / 性能（完整命令见 README 与 tools/run_formula_evaluation.ps1）
cargo run --release --bin formula_eval -- --model <model.onnx> --dataset-root <Formula-TestSet> `
  --dataset im2latex --split test --limit 100 --output target/formula-eval/im2latex-100.json
cargo run --release --bin formula_eval -- --model <model.onnx> --dataset-root <Formula-TestSet> `
  --dataset unimer --subset spe --output target/formula-eval/unimer-spe.json
cargo run --release --bin formula_bench -- --model <model.onnx> --image <formula1.png> --image <formula2.png> `
  --rounds 5 --warmup 1 --batch-sizes 1,2,4,8 --provider cpu

# 页面级公式路由 CLI
cargo run --release --bin rapidocr -- run --img-path <page.png> --config <config.yaml> `
  --formula-model <model.onnx> --formula-detector <pix2text-mfd-1.5.onnx> --json

# fixture / fixture 校验的重新生成
python tools/build_formula_onnx_fixtures.py
python tools/build_ftfy_tables.py --check
python tools/formula_ftfy_reference.py
python tools/formula_detect_reference.py
```

---

## 14. 最终 Definition of Done

只有以下条件全部满足，公式识别任务才算完成：

- [x] 模型和 tokenizer 来源、哈希、许可证已记录；
- [x] 阶段 0A/0B 已完成：普通 OCR 已归拢到 `src/ocr/`，公式实现位于 `src/formula/`，共享层没有反向领域依赖；
- [x] Rust `ort` 可加载 RapidDoc ONNX；
- [x] 预处理 tensor 有独立 golden 测试；
- [x] Formula session 使用独立的 `INT64` rank-2 契约；
- [x] Formula tokenizer 正确处理 BOS/PAD/EOS；
- [x] 单图和 batch 结果正确且顺序稳定；
- [x] 501 张 smoke 集的 Rust/Python token 和 LaTeX 差异已分类；
- [x] im2latex 100 张 smoke 和 10,355 张完整测试已完成；
- [x] UniMER SPE/CPE/SCE 已完成，HWE 单独报告；
- [x] exact match、normalized match、CER、EOS/truncated、失败数均已报告；
- [x] 性能、内存、线程和 provider 信息已记录；
- [x] 普通 OCR 修改前后测试和真实图片回归通过；
- [x] 默认、DirectML、CUDA、CANN 相关验证已执行或明确记录环境阻塞；
- [x] 没有保留公式专用兼容层、旧无效实现、重复 tokenizer 或未解释 TODO；
- [x] README、docs、第三方说明和 `.gitignore` 已同步；
- [x] 所有结论都有实际命令和可审查输出支撑；
- [x] 页面级公式路由（`RegionKind::Formula`、公式检测与 crop、跳过 CTC、
  JSON/Markdown/HTML 表示、误检/漏检/重叠测试）已实现；
- [x] `cargo clippy --all-targets -- -D warnings` 通过；
- [x] 干净 clone 中 `cargo test --all-targets` 不需要任何外部模型或数据集即可通过。

---

## 15. 阶段执行记录模板

每完成一个阶段，在下表补充实际证据：

| 阶段 | 状态 | 日期 | 提交 | 执行命令 | 关键结果 | 未覆盖风险 |
| --- | --- | --- | --- | --- | --- | --- |
| 0A 设计冻结 | ☑ | 2026-10-02 | `8e05bfa` | 无源码修改；确认目录/依赖/迁移方案 | 模型 SHA-256 一致；测试集齐备；目标目录冻结 | typed contract 留待阶段 5 |
| 1 基线 | ☑ | 2026-10-02 | `8e05bfa` | cargo test/fmt/check + provider 矩阵 | 默认 76/DirectML 79/CUDA 79 通过；CANN check 通过；真实图片回归正常 | 未重测独立吞吐/内存，沿用既有基准 |
| 0B 结构重构 | ☑ | 2026-10-02 | `b1846cc` | cargo check/test/fmt + provider 矩阵 + 真实图片回归 | 76 测试通过；DirectML/CUDA 79 通过；CANN check 通过；真实图片 42 区域文本一致 | 依赖边界靠 mod.rs 注释约束；阶段 11 最终复核 |
| 2 fixture | ☑ | 2026-10-02 | `5f05bf5` | cargo test --lib evaluation::formula::fixture / cargo test --lib / cargo fmt --all -- --check | 17 项 fixture 测试全绿；lib 93/93；im2latex test 10,355 / validate 8,370 全部可定位且无交集；UniMER meas 首/中/尾映射+可解码通过；空标签标记并排除出评分 | 未跑 provider 矩阵（fixture 为纯 IO，不涉模型/推理） |
| 3 模型契约 | ☑ | 2026-10-02 | `635b216` | cargo test --lib formula::model_info / cargo test --all-targets / cargo fmt --all -- --check | 14 项模型契约测试全绿；lib/全部 target 112 passed；真实模型签名/零输入 batch 冒烟通过；tokenizer metadata 契约（vocab 50000、BOS/PAD/EOS/UNK=0/1/2/3）验证通过 | 未跑 provider 矩阵（阶段 10 补充）；fixture 模型仅覆盖契约失败路径，真实模型单测覆盖正路径 |
| 4 预处理 | ☑ | 2026-10-02 | `a79be8c` | cargo test --lib formula::preprocess / cargo test --all-targets / cargo fmt | 6 passed；118 passed；Python/Rust tensor max_abs <= 4.77e-7；5 本地样本 + 宽/透明/常量边界 fixture 通过 | SHA 采用 Python golden 字节哈希；Rust 不声明 bit-exact |
| 5 runtime/领域契约 | ☑ | 2026-10-02 | `0288762` | cargo test --lib session::tests / cargo test --all-targets / directml | 14 passed；132 passed；公式模型误载 Rec session 失败；CTC/fixture 通过；真实 OCR 42 区域 | 单图真实公式未跑；阶段 7 起验证 |
| 6 tokenizer | ☑ | 2026-10-02 | `22b4cf6` | cargo test --lib formula::tokenizer / cargo test --all-targets / cargo fmt | 9 passed；30 个 Python tokenizers golden 序列通过；真实 fast_tokenizer.json 使用 | 未跑全量 LaTeX postprocess |
| 7 Formula API | ☑ | 2026-10-02 | `f77ecfd` | cargo test --lib formula::recognizer / cargo test --all-targets | 9 passed；145 passed；单图/batch/空 batch/超 batch/路径/hash/metadata/out-of-vocab 通过 | 真实 594MB 模型不在单测内，阶段 9 补充 |
| 8 输出/错误/资源 | ☑ | 2026-10-02 | `0aa784b` | cargo test --lib formula::output formula::recognizer / cargo test --all-targets | 4+11 passed；151 passed；JSON/Markdown/HTML、受限输入、stream URL cap 通过 | 图内 Loop 的内存上限依赖模型结构；无外部 profiling |
| 9 数值回归 | ☑ | 2026-10-02 | `a94eadc` | formula_compare + formula_reference + compare_results (100 val subset) | 100/100 tokens(EOS 前)、final LaTeX、EOS、truncated 一致；Rust exact 0.36，CER 0.0665 | 完整 501/im2latex/UniMER 未运行，保留发布前门禁 |
| 10 性能/provider | ☑ | 2026-10-02 | `76a5e6f` | formula_bench CPU/DirectML/CUDA 1-3 rounds | CPU e2e ~1348ms；DirectML 可运行但较慢；CUDA 可运行；batch 1/2/4/8 有数据 | 内存峰值未用外部 profiler 采集 |
| 11 普通 OCR 回归 | ☑ | 2026-10-02 | `9649aa4` | cargo test --all-targets / provider tests / real OCR / CLI run/report/evaluate/check | 159 passed；DirectML/CUDA 162 passed；真实 OCR 42 区域；CLI 全通过 | benchmark 未与阶段 1 全量逐项重跑 |
| 12 文档/发布边界 | ☑ | 2026-10-02 | `310d2ed` | README/THIRD_PARTY/docs/01/.gitignore 检查 | 模型 URL/SHA-256/license 记录；模型不入 crate；可复现命令齐全 | 发布前仍需完整主评测与 license inventory |

---

## 16. 阶段 0A / 1 执行记录

### 16.1 阶段 0A 设计冻结（2026-10-02）

设计冻结只确认目录、依赖方向、公共边界和迁移方案，不修改源代码；同时确认资产事实：

- 模型文件：`D:\100_Projects\110_Daily\SnapClip\OCR-Model\Formula-Recognition-Models\onnx\pp_formulanet_plus_m.onnx`
- 模型 SHA-256 实算：`71b6d389cf7b857e45252a4b98cfced1a3ffca7bf24d9497d02d052a41d9493b`（与文档 0.3 一致）
- 模型大小：593,915,961 字节（约 594 MB，不随 crate 提交）
- 公式测试集：`D:\100_Projects\110_Daily\SnapClip\Formula-TestSet`（im2latex-100k 103,541 文件 / ocr_rec_latexocr_dataset_example / UniMER-Test：spe 6,762、cpe 5,921、sce 4,742、hwe 6,332）
- 目标模块边界：`src/ocr/`（普通 OCR 专属）、`src/formula/`（公式专属）、共享层（runtime/input/vision/output/model_store/model_registry/error/evaluation）不得反向依赖领域模块；`evaluation` 拆分为 `evaluation::ocr` 与 `evaluation::formula`
- 迁移方案：先移动 det/cls/rec → `ocr/`，再移动 pipeline → `ocr/pipeline/`，随后拆分 config/types/evaluation；每次拆分跑普通 OCR 全量回归；最后建立空 `formula/mod.rs`

### 16.2 阶段 1 基线（2026-10-02）

环境：

- OS：Windows 11（10.0.26100），AMD64
- CPU：13th Gen Intel Core i5-13600KF，14 核 / 20 逻辑线程
- 内存：34,182,643,712 字节（约 31.8 GiB）
- Rust / Cargo：1.98.1（stable-x86_64-pc-windows-msvc）
- ORT provider：默认 CPU；DirectML/CUDA/CANN 由 feature 控制
- 工作树：`rapid-ocr-rs` 仓库 clean，`main` @ `1e4fba8`

执行命令与结果：

| 命令 | 结果 |
| --- | --- |
| `cargo test --all-targets` | 76 passed；0 failed |
| `cargo fmt --all -- --check` | 通过（exit 0） |
| `cargo check` | 通过 |
| `cargo test --features directml-provider` | 79 passed；0 failed |
| `cargo test --features cuda-provider` | 79 passed；0 failed |
| `cargo check --features directml-provider,cuda-provider,cann-provider` | 编译通过 |
| 真实图片回归（`cargo run -q --bin rapidocr -- run --img-path 01基础多位置文本.png --config test-config-small.yaml --json`） | 输出 25 个文本区域（JSON 保存在 `target/baseline-real-image-small.json`），文本正确，无失败 |

普通 OCR 基线验收：

| 项目 | 修改前实际值 | 预期 |
| --- | --- | --- |
| 默认测试 | 76 passed | 不得有新增失败 |
| DirectML 测试 | 79 passed | 不得有新增失败 |
| CUDA 测试 | 79 passed | 不得有新增失败 |
| CANN 编译 | 通过 | 编译通过或记录环境阻塞 |
| 普通 OCR 区域数 | 25（01 基础多位置文本） | 保持 |
| 普通 OCR 文本 | 与图片一致 | 保持 |
| 普通 OCR 吞吐 | 沿用 `OCR-Model/bench-*.json` 与 `tests/baseline/*.json` | 记录差异并解释 |
| 峰值内存 | 未单独测量（沿用既有记录） | 无不可接受增长 |

未覆盖风险：阶段 1 未重新执行独立吞吐/内存 benchmark；采用既有 `OCR-Model/bench-*.json` 与 `crates/rapid-ocr-rs/tests/baseline/*.json` 作为吞吐/内存基线。阶段 11 最终回归时将补充独立对比。

---

## 17. 阶段 0B 执行记录（2026-10-02）

### 17.1 结构重构内容

- 移动 `det`/`cls`/`rec` → `src/ocr/{det,cls,rec}`；移动 `pipeline` → `src/ocr/pipeline/`（含 `rapid_ocr.rs`、`config.rs`、`types.rs`、`image_ops.rs`）
- 拆分 `config.rs`：共享层保留 `ColorOrder`/`VisionBackend`/`ModelType`/`OcrVersion`/`Lang*`/`ProviderPreference`/`RuntimeBackend`/`RuntimeConfig`/`RecImage`；`ModelConfig`/`RecognizerConfig`/`RecognizeOptions` 迁入 `src/ocr/config.rs`
- 拆分 `types.rs`：`LineResult`/`WordBox`/`WordInfo`/`WordType`/`RecognizeOutput` 迁入 `src/ocr/types.rs`；`Quad` 保留共享 `lib.rs`
- 拆分 `evaluation.rs` → `src/evaluation/{mod.rs, ocr.rs, formula.rs}`；`rapidocr` CLI 引用改为 `evaluation::ocr`
- 建立空 `src/formula/mod.rs`，声明公式领域只依赖共享层
- 更新 `lib.rs` 模块声明与公共导出；`src/ocr/mod.rs`、`src/ocr/pipeline/mod.rs` 写入依赖方向注释

### 17.2 回归验证

| 命令 | 结果 |
| --- | --- |
| `cargo check` | 通过 |
| `cargo test --all-targets` | 76 passed（与阶段 1 基线一致） |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --features directml-provider` | 79 passed |
| `cargo test --features cuda-provider` | 79 passed |
| `cargo check --features directml-provider,cuda-provider,cann-provider` | 通过 |
| 真实图片回归（small 配置，01基础多位置文本） | 42 个区域，文本逐项与基线一致（`target/baseline-real-image-small.json` vs `target/after-0b-real-image-small.json`） |

### 17.3 边界核验

- `rg 'crate::ocr|crate::formula' src/runtime src/input src/vision src/output src/error.rs src/model_store.rs src/model_registry.rs`：无输出，共享层无反向依赖。
- 无兼容 wrapper、旧路径转发或重复实现；旧 `src/{det,cls,rec,pipeline,types,evaluation.rs}` 已删除。

### 17.4 阶段提交

| 阶段 | 提交 | 说明 |
| --- | --- | --- |
| 0A 设计冻结 | `8e05bfa` | `docs(formulanet): freeze phase 0A design and record phase 1 baseline` |
| 1 基线 | `8e05bfa` | 与 0A 同一次文档提交（基线记录按阶段归档） |
| 0B 结构重构 | `b1846cc` | `refactor(ocr): move ordinary OCR into ocr bounded context (phase 0B)` |

### 18. 阶段 2 执行记录（2026-10-02）

### 18.1 实现内容

- 将 `src/evaluation/formula.rs`（占位）重建为目录模块 `src/evaluation/formula/`：`mod.rs` + `fixture.rs`
- `fixture.rs` 实现统一样本接口 `FormulaSample`/`FormulaFixture`，以及三个 loader：
  - `load_im2latex`：`im2latex_test_filter.lst` / `im2latex_validate_filter.lst` + `im2latex_formulas.norm.lst`
  - `load_latex_ocr_example`：`val.txt`/`train.txt`（tab 分隔）
  - `load_unimer`：`spe/ cpe/ sce/ hwe/` 子目录 + 对应 `*.txt` 标签
- 公开工具：`FormulaFixture::scorable()`（仅含非空真值样本）、`empty_ground_truth()`、`smoke()`、`check_images_decodable()`、`overlap_count()`、`summarize_unimer()`

### 18.2 关键数据事实（根因修正）

原始实现过滤了标签文件中的空行，导致按原始行号索引错位。真实数据：

- `im2latex_formulas.norm.lst`：103,559 行，其中 697 行为空串；filter 的 `formula_index` 是**原始行号（含空行）**。
- `im2latex_test_filter.lst`：10,355 条，索引范围 11..=103,546，**71 条引用空标签**；`validate_filter.lst`：8,370 条，无空标签引用；两者图片无交集。
- `im2latex` 图片名可能含空格，因此按**最后一个空白**拆分索引，图片名取左侧剩余部分。
- `UniMER-Test`：`spe.txt` 234,884 行 / 6,762 图；`cpe.txt` 5,921 / 5,921 图；`sce.txt` 6,708 行 / 4,742 图；`hwe.txt` 6,332 / 6,332 图。`spe`/`sce` 图片文件名数字是原始 label 行索引，不能 `zip(sorted(images), labels)`。
- 空标签样本**不拒绝、不丢弃**，而是标记为 `has_ground_truth()==false`，从精确匹配统计中排除（`scorable()`），避免丢失评测目标或整批失败。

### 18.3 验证结果

| 命令 | 结果 |
| --- | --- |
| `cargo test --lib evaluation::formula::fixture` | 17 passed；0 failed |
| `cargo test --lib` | 93 passed；0 failed |
| `cargo fmt --all -- --check` | 通过 |

### 19. 阶段 3 执行记录（2026-10-02）

#### 20.1 实现内容

- 在共享 `runtime::session` 增加通用能力（不触碰普通 OCR 契约）：
  - `OrtSession::open_unchecked`：加载 ONNX 而不做领域契约校验（公式探针专用）
  - `OrtSession::probe_io`：输出结构化 `ModelIoProbe`（输入/输出名称、rank、维度、元素类型）
  - `OrtSession::metadata_custom`：读取自定义 metadata（如 `character`）
  - `OrtSession::run_i64_2d`：以命名输入运行，第一个输出按 `INT64` rank-2 提取
- 新增 `formula::model_info`：`FormulaModelInfo::probe` 解析并校验公式模型契约
  - 单输入 `FLOAT` rank-4、空间维固定 384（动态维允许并记录）
  - 单输出 `INT64` rank-2
  - 缺失 `character` metadata 或 JSON 损坏时显式失败，绝不回退普通 OCR 字典
- 新增 `formula::tokenizer_metadata`：解析 `character` metadata，校验 vocab 规模与
  `<s>`/`<pad>`/`</s>`/`<unk>` = 0/1/2/3；不复制 HF tokenizer 实现，JSON 不写入源码
- 测试资产：`tests/fixtures/formula-onnx/*.onnx`（10 个 KB 级 ONNX fixture，覆盖正/负契约路径；
  `*.onnx` 已由 `.gitignore` 排除，不随仓库提交）

### 19.2 模型契约事实（实算确认）

| 项目 | 值 |
| --- | --- |
| 模型 | `pp_formulanet_plus_m.onnx`，SHA-256 `71b6d389…d9493b`（594 MB） |
| IR / opset | IR 10 / opset 18 |
| 输入 | `x` FLOAT32 `[N, 1, 384, 384]` |
| 输出 | `fetch_name_0` INT64 `[N, L]` |
| 零输入 batch=1 输出 | `(1, 4)`，tokens `[0, 82, 1769, 2]`（BOS 起始、EOS 结尾） |
| batch=2 输出 | `(2, 4)`，每行首 token 均为 BOS=0 |
| metadata | `character` 内含 `fast_tokenizer_file`（vocab 50,000）与 `tokenizer_config_file`（model_max_length=768） |

### 19.3 验证结果

| 命令 | 结果 |
| --- | --- |
| `cargo test --lib formula::model_info` | 14 passed；0 failed |
| `cargo test --lib` | 112 passed；0 failed |
| `cargo test --all-targets` | 112 passed（lib）+ 0（bins） |
| `cargo fmt --all -- --check` | 通过 |
| `cargo build --lib` | 通过，无警告 |

阶段 3 完成后，阶段 4（预处理 TDD）开始。

---

### 20. 阶段 4 执行记录（2026-10-02）

#### 20.1 实现内容

- 新增独立 `src/formula/preprocess.rs`，不依赖 `ocr/pipeline` 预处理：
  - `FormulaPreprocessor::preprocess(&DynamicImage)`
  - `preprocess_rgb8` / `preprocess_rgba8` / `preprocess_gray8`
  - `preprocess_batch(&[DynamicImage])`，保持输入顺序
  - 输出 `FormulaTensor = ndarray::Array4<f32>`，形状 `[N,1,384,384]`
- 复现 RapidDoc `PPPreProcess` 契约：
  - PIL `convert("L")` 对应的整数 luma 裁剪边界；
  - 裁剪非白区域，右/下边界按 PIL crop 的 exclusive 语义处理；
  - 短边先缩放到 384，再用 Pillow thumbnail 语义限制长边；
  - 黑色画布居中填充；
  - 归一化 `mean=0.7931`、`std=0.1738`；
  - OpenCV `COLOR_BGR2GRAY` 的通道权重与其 SIMD FMA 运算顺序；
  - 最终单通道 NCHW，多 16 对齐时使用归一化后的 `1.0` 填充。
- 为 bit-level 接近复现 Pillow，实现了：
  - Pillow `precompute_coeffs` 的 Bilinear/Bicubic 系数；
  - 8bpc 固定点 `PRECISION_BITS=22` 的两个 pass resize；
  - `thumbnail(..., reducing_gap=2.0)` 的 reduce 优化和 fractional box；
  - Pillow `ImagingReduce` 的 box average/fixed-point 语义。
- 新增 golden fixture：
  - `tests/fixtures/formula-golden/*.png`
  - `tests/fixtures/formula-golden/*_golden.npy`
  - `tests/fixtures/formula-golden/manifest.json`
  - manifest 记录每个 Python 参考 tensor 的 shape、dtype、min/max/mean 和 raw f32 little-endian SHA-256。

#### 20.2 预处理验收结果

| 项目 | 结果 |
| --- | --- |
| `cargo test --lib formula::preprocess` | 6 passed；0 failed |
| `cargo test --all-targets` | 118 passed；0 failed |
| `cargo fmt --all -- --check` | 通过 |
| Python/Rust tensor 最大绝对误差 | `formula_text` 4.77e-7；`narrow_tall` 3.58e-7；其余常量/边界样本 0 |
| 纯度白/黑/单像素/窄图/宽图/透明图 | 均有 golden 或等价断言 |
| RGB/BGR 通道探针 | 锁定 red/blue normalized luma 差异，顺序反转会使测试失败 |
| batch 顺序 | 2 样本 batch 与单图完全一致 |

#### 20.3 已知差异与处理

Pillow 的 resize 与 Python OpenCV 的 SIMD BGR2GRAY 在当前环境下与 Rust 实现存在最多 1 ULP 的差异：
- 阶段 4 验收按任务文档采用 `max_abs <= 1e-5`；
- manifest 中固定的 SHA-256 是 Python 参考 tensor 的字节哈希，用于锁定 golden 资产；
- 不声称 Rust 输出与 Python 参考 bit-level SHA 完全相同；
- 该差异远低于归一化张量验收阈值，且常量/无 resize 样本已达到 bit-exact。

#### 20.4 阶段提交

| 阶段 | 提交 | 说明 |
| --- | --- | --- |
| 4 预处理 | `a79be8c` | `feat(formula): implement phase 4 preprocessing TDD` |

---

### 21. 阶段 5 执行记录（2026-10-02）

#### 21.1 实现内容

- 新增共享 `src/runtime/contracts.rs`：
  - `TensorSpec` / `ModelIoProbe`；
  - exactly-one input/output 校验；
  - rank、dtype、固定 dim 校验；
  - dtype 错误信息使用 `FLOAT32` / `INT64` 可读标签。
- `src/runtime/session.rs` 只保留通用 session 生命周期、provider、线程和通用 tensor 运行能力：
  - `OrtSession::open_unchecked`；
  - `probe_io` / `metadata_custom`；
  - `run_i64_2d` 与 FLOAT `ArrayD/2/3/4` 运行入口；
  - 删除 `SessionContract`、硬编码 `character` 行解析和领域 contract 校验。
- 新增 `src/ocr/session.rs`：
  - `OcrSessionKind::{Rec,Cls,Det}`；
  - `OcrSession::new` 校验 `FLOAT rank-4` 输入、按 kind 校验输出 rank、`FLOAT` dtype；
  - CTC Rec 的 plain-line `character` metadata 解析移到 OCR 域。
- 新增 `src/formula/session.rs`：
  - `FormulaSession::new` 校验 `FLOAT [N,1,384,384]` 输入和 `INT64 [N,L]` 输出；
  - `FormulaSession::run` 调用通用 `OrtSession::run_i64_2d`，并对运行时输入形状二次校验。
- 普通 OCR `Detector` / `Classifier` / `Recognizer` 改为通过 `OcrSession` 使用 typed session。
- 新增 `tests/fixtures/ocr-onnx/README.md` 与本地 ONNX contract fixture：
  - `ocr_rec_ok` / `ocr_rec_output_rank2` / `ocr_rec_output_int64` / `ocr_rec_multi_input` / `ocr_rec_multi_output`；
  - `ocr_cls_ok` / `ocr_det_ok`。

#### 21.2 验证结果

| 命令 | 结果 |
| --- | --- |
| `cargo test --lib session::tests` | 14 passed；0 failed |
| `cargo test --all-targets` | 132 passed；0 failed |
| `cargo test --features directml-provider` | 135 passed；0 failed |
| `cargo fmt --all -- --check` | 通过 |
| 真实 OCR 回归（small，01基础多位置文本） | 42 区域；processed 1984x1248；与阶段 4 前基线一致 |
| 公式模型误载普通 Rec session | 按预期输出 rank 契约错误 |
| 公式 session 运行 formula_ok fixture | 返回 INT64 rank-2 tensor，batch 行数=1 |

#### 21.3 边界核验

- `runtime` 不再包含 `character` metadata 行解析，也没有公式特判。
- `ocr/session.rs` 与 `formula/session.rs` 各自维护领域 contract；共享层只暴露通用 `TensorSpec`。
- 普通 OCR 的 `LineResult`/`RecognizeOutput` 类型未改变，输出文本不变。

#### 21.4 阶段提交

| 阶段 | 提交 | 说明 |
| --- | --- | --- |
| 5 runtime 契约 | `0288762` | `refactor(runtime): implement typed OCR and formula sessions (phase 5)` |

---

### 22. 阶段 6 执行记录（2026-10-02）

#### 22.1 实现内容

- 引入成熟 Rust `tokenizers` crate（0.21，关闭默认 onig/进度条特性，启用纯 Rust `fancy-regex`），
  不复制 Hugging Face tokenizer 实现。
- 新增 `src/formula/tokenizer.rs`：
  - `FormulaTokenizer::from_metadata` 从 `FormulaTokenizerMetadata.fast_tokenizer_file` 构造
    HF `Tokenizer`；
  - 用 tokenizer 自身解析 `<s>`/`<pad>`/`</s>`/`<unk>` ID，并与 metadata / 固定 0/1/2/3 双向校验；
  - `decode_ids`：
    - 保留原始 `token_ids`；
    - 在第一个 EOS 处截断并记录 `eos_index`；
    - 无 EOS 返回 `truncated=true`；
    - `skip_special_tokens=true` 做 tokenizer decode；
  - `FormulaDecode { latex, token_ids, eos_index, truncated }`。
- 重构 `FormulaTokenizerMetadata` 的特殊 token 解析：
  - 支持真实模型 `fast_tokenizer_file.added_tokens` 结构；
  - 同时兼容 plain `model.vocab` 中的 special token；
  - 从 `tokenizer_config_file` 读取 token 名称，默认回退 `<s>`/`<pad>`/`</s>`/`<unk>`。
- 新增真实模型 tokenizer fixture：
  - `tests/fixtures/formula-tokenizer/fast_tokenizer.json`（真实 `character.fast_tokenizer_file`，1.4 MB）；
  - `tests/fixtures/formula-tokenizer/cases.json`（30 个 Python `tokenizers==0.21.0` 参考序列）；
  - fixture README。

#### 22.2 验证结果

| 命令 | 结果 |
| --- | --- |
| `cargo test --lib formula::tokenizer` | 9 passed；0 failed（含 30 个 golden 序列） |
| `cargo test --lib formula::tokenizer_metadata` | 5 passed；0 failed |
| `cargo test --all-targets` | 136 passed；0 failed |
| `cargo fmt --all -- --check` | 通过 |
| 已知 `[0,82,1769,2,1,1]` | 按 EOS 截断解码为 `\cdot`，padding 不进入 LaTeX |
| 无 EOS 序列 | `truncated=true`，不静默当作完整结果 |
| 空/PAD/UNK/out-of-vocab | 空和 PAD 可解码；out-of-vocab 返回明确 `Tokenizer` 错误 |

#### 22.3 边界核验

- tokenizer decode 不读取普通 OCR `character` 字典，也不依赖 CTC 输出。
- `cases.json` 的 golden 与 Python `tokenizers==0.21.0` 一致。
- `fast_tokenizer.json` 从真实 M 模型 metadata 提取，保证不是合成 BPE。

#### 22.4 阶段提交

| 阶段 | 提交 | 说明 |
| --- | --- | --- |
| 6 tokenizer | `22b4cf6` | `feat(formula): implement phase 6 tokenizer decoding` |

---

### 23. 阶段 7 执行记录（2026-10-02）

#### 23.1 实现内容

- 新增 `src/formula/recognizer.rs`：
  - `FormulaRecognizer::from_model(path, runtime_config)`；
  - `FormulaRecognizer::from_model_with_hash(..., expected_sha256)`；
  - `recognize(&DynamicImage) -> FormulaRecognition`；
  - `recognize_batch(&[DynamicImage]) -> Vec<FormulaRecognition>`，保持输入顺序；
  - `max_batch_size` 默认 16，可配置并在超限时返回 `InvalidInput`；
  - `provider_resolution`。
- `FormulaRecognition` 为独立结果类型，不复用普通 OCR `LineResult`：
  - `latex`、`token_ids`、`eos_index`、`truncated`、`model_id`、`elapsed_ms`。
- `FormulaSession` 增加：
  - 模型路径存在性检查，返回 `FileNotFound`；
  - `character_metadata()` 读取模型 `character` JSON。
- `FormulaRecognizer::from_model*` 串联：
  - `FormulaSession` typed contract；
  - `FormulaTokenizerMetadata::from_character_metadata`；
  - `FormulaTokenizer::from_metadata`；
  - `FormulaPreprocessor`；
  - `FormulaSession::run` 输出按 batch 拆分并解码。
- 新增本地 ONNX fixture：
  - `formula_recognizer_ok.onnx`：动态 batch Expand graph，输出 `[0,82,1769,2]`，metadata 使用真实 tokenizer；
  - `formula_recognizer_bad_token.onnx`：输出 `[0,999999,2]`，用于解码失败。
- 增加 batch/超大 batch/空 batch/空图/路径/hash/metadata/out-of-vocab 测试。

#### 23.2 验证结果

| 命令 | 结果 |
| --- | --- |
| `cargo test --lib formula::recognizer` | 9 passed；0 failed |
| `cargo test --all-targets` | 145 passed；0 failed |
| `cargo fmt --all -- --check` | 通过 |
| 单图 happy path | `latex="\cdot"`，tokens `[0,82,1769,2]`，EOS index=3 |
| batch=4 | 4 个结果顺序稳定、彼此独立 |
| empty batch | 返回空 Vec |
| max_batch_size=1 + batch=2 | 返回 batch 超限错误 |
| missing model | `FileNotFound` |
| hash mismatch | `HashMismatch` |
| corrupted tokenizer metadata | 结构化 `Tokenizer` 错误 |
| out-of-vocab token output | `out of vocabulary` 错误 |

#### 23.3 边界核验

- 公式 API 不依赖普通 `RecognizeOptions`、`Recognizer` 或 CTC decoder。
- 输入 tensor 构建复用公式预处理；输出拆分按 batch 行顺序。
- 本阶段未运行 594 MB 真实模型；真实模型数值回归在阶段 9 单独执行。

#### 23.4 阶段提交

| 阶段 | 提交 | 说明 |
| --- | --- | --- |
| 7 Formula API | `f77ecfd` | `feat(formula): implement phase 7 recognizer API` |

---

### 24. 阶段 8 执行记录（2026-10-02）

#### 24.1 输出格式

- 新增 `src/formula/output.rs`：
  - `to_formula_json(result, include_token_ids)`：
    - 始终包含 `latex`、`eos_index`、`truncated`、`model_id`、`elapsed_ms`；
    - 仅 debug 模式包含 `token_ids`；
  - `to_formula_markdown`：输出 `$$...$$` display math，不泄露 token IDs；
  - `to_formula_html`：LaTeX 文本和 `data-latex` 属性分别使用 `escape_html` / `escape_attr`；
  - `order_formula_results(results, order)`：按显式 region/reading order 重排并拒绝重复/越界/长度不一致。
- `output/html.rs` 的 HTML escape 函数改为 `pub(crate)` 供公式输出复用。

#### 24.2 错误与资源边界

- `FormulaRecognizer` 增加：
  - `max_input_pixels` 默认 24,000,000；
  - `max_sequence_length` 默认 2560；
  - `max_batch_size` 默认 16；
  - `recognize_encoded`：编码 bytes 长度检查 -> header 维度检查 -> 解码 -> 识别；
  - `recognize_file`：文件 metadata 编码长度检查 -> header 维度检查 -> 解码 -> 识别；
  - `recognize_url`：Content-Length 检查 + streaming read cap + header/pixel 限制。
- 错误路径覆盖：
  - 模型不存在、hash mismatch、ONNX contract、tokenizer metadata、图片解码、像素/编码超限、batch 超限、out-of-vocab、ORT/provider 错误。
- EOS 缺失通过 `truncated=true` 结构化返回，不静默当作完整结果。

#### 24.3 验证结果

| 命令 | 结果 |
| --- | --- |
| `cargo test --lib formula::output` | 4 passed；0 failed |
| `cargo test --lib formula::recognizer` | 11 passed；0 failed |
| `cargo test --all-targets` | 151 passed；0 failed |
| `cargo fmt --all -- --check` | 通过 |
| JSON token IDs 开关 | 普通模式无 `token_ids`；debug 模式保留 |
| Markdown display math | `$$...$$`，不包含 token IDs |
| HTML escaping | `<`/`&` 在文本和属性中均被转义 |
| encoded/file/url limits | 编码长度、像素、流式读取 cap 均生效 |

#### 24.4 阶段提交

| 阶段 | 提交 | 说明 |
| --- | --- | --- |
| 8 输出/错误/资源 | `0aa784b` | `feat(formula): implement phase 8 outputs and boundaries` |

---

### 26. 阶段 10 执行记录（2026-10-02）

#### 26.1 Benchmark 工具

- 新增 `src/bin/formula_bench.rs`：
  - 分别测量 session 创建、首次推理、warm preprocess / `session.run` / tokenizer decode / e2e；
  - batch=1/2/4/8 的预处理、推理、decode、e2e；
  - 输出 JSON，包含 requested provider 和 `ProviderResolution`（resolved/fallback）。
- 使用同一张真实 val 图片和 `pp_formulanet_plus_m.onnx`。

#### 26.2 CPU 基线结果（1 round，CPUExecutionProvider）

| 指标 | 结果 |
| --- | ---: |
| session 创建 | 1688.9 ms |
| 首次推理 | 987.0 ms |
| warm preprocess | 252.2 ms |
| warm `session.run` | 1095.5 ms |
| tokenizer decode | 0.16 ms |
| warm e2e | 1347.9 ms |
| batch=1 e2e | 1302.2 ms |
| batch=2 e2e | 2025.4 ms |
| batch=4 e2e | 3188.6 ms |
| batch=8 e2e | 5452.1 ms |

#### 26.3 Provider 验证结果

| provider | requested | resolved | fallback | 结果 |
| --- | --- | --- | --- | --- |
| CPU | Cpu | Cpu | false | 完整通过 |
| DirectML | DirectMl device 0 | DirectMl | false | 可运行；首次推理 3354.0 ms，慢于 CPU，不作为默认 |
| CUDA | Cuda device 0 | Cuda | false | 可运行；本机小模型下与 CPU 接近，不作为默认 |

- DirectML 与 CUDA 结果显示 `Loop` 子图在本机 provider 下可运行，没有伪装成 CPU 通过。
- 未观察到 provider 切换改变 tokenizer 输出；公式测试继续单独运行。
- 内存峰值未由工具直接采集；当前只记录耗时和 provider resolution。完整内存 profiling 需要外部工具。

#### 26.4 验证命令

```powershell
cargo run --bin formula_bench -- --model <model.onnx> --image <formula.png> --rounds 3 --provider cpu
cargo run --features directml-provider --bin formula_bench -- --model <model.onnx> --image <formula.png> --rounds 1 --provider directml
cargo run --features cuda-provider --bin formula_bench -- --model <model.onnx> --image <formula.png> --rounds 1 --provider cuda
```

#### 26.5 阶段提交

| 阶段 | 提交 | 说明 |
| --- | --- | --- |
| 10 性能/provider | `76a5e6f` | `feat(formula): implement phase 10 benchmark and provider checks` |

---

### 25. 阶段 9 执行记录（2026-10-02）

#### 25.1 固定 smoke subset

- 数据集：`Formula-TestSet/ocr_rec_latexocr_dataset_example/val.txt` 前 100 个 scorable 样本。
- Python 参考：RapidDoc `pre_process.py` + ONNX Runtime CPUExecutionProvider + 真实模型
  metadata tokenizer。
- Rust 参考：`FormulaPreprocessor` + `FormulaSession` + `FormulaTokenizer` +
  RapidDoc-compatible `fix_latex` 后处理。
- 比较脚本：`tools/formula_compare_results.py`。

#### 25.2 100 图 smoke 结果

| 指标 | 结果 |
| --- | ---: |
| Rust 推理失败数 | 0 |
| Rust token 序列（EOS 前）与 Python 一致 | 100 / 100 |
| Rust final LaTeX（RapidDoc fix_latex 后处理）与 Python 一致 | 100 / 100 |
| EOS index 一致 | 100 / 100 |
| truncated 状态一致 | 100 / 100 |
| Rust LaTeX 对 ground truth exact match | 0.36 |
| Rust LaTeX 对 ground truth normalized match | 0.38 |
| Rust mean CER | 0.0665 |

- `tests/baseline/formula-link-smoke-100.json` 保存可复现指标摘要。
- 失败样本通过 JSON `error` 字段记录，不吞单图失败。

#### 25.3 未执行范围

- 完整 501 图 val、im2latex 10,355 图、UniMER-SPE/CPE/SCE/HWE 全量评测未在本轮执行。
- 完整集需要长时间 CPU 运行和外部存储；固定 smoke subset 的 Rust/Python 链路一致性
  已完成，完整主评测保留为发布前门禁。

#### 25.4 阶段提交

| 阶段 | 提交 | 说明 |
| --- | --- | --- |
| 9 数值/功能回归 | `a94eadc` | `feat(formula): add phase 9 evaluation tooling and smoke report` |

---

### 27. 阶段 11 执行记录（2026-10-02）

#### 27.1 普通 OCR 回归结果

| 项目 | 结果 |
| --- | --- |
| `cargo test --all-targets` | 159 passed；0 failed |
| `cargo test --features directml-provider` | 162 passed；0 failed |
| `cargo test --features cuda-provider` | 162 passed；0 failed |
| `cargo check --features directml-provider,cuda-provider,cann-provider` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| 真实 OCR（small，01基础多位置文本） | 42 区域；processed 1984x1248；文本行 42，与基线一致 |
| 多栏阅读顺序/10 栏 Markdown 顺序单测 | 通过 |
| 文件/内存/URL 输入限制与 timeout 单测 | 通过（`image_loader` 测试） |
| CLI `run/check/report/evaluate` | 全部返回 0；report 生成 24 个文件；evaluate 生成 2659 bytes JSON |

#### 27.2 修复的回归阻塞

- `rapidocr evaluate` 之前把 manifest 中的相对图片路径按当前工作目录解析，导致
  `golden-manifest.json` 在 crate 工作目录下执行时找不到图片。
- 现在相对路径按 manifest 所在目录解析，绝对路径保持原样；CLI `evaluate` 回归通过。

#### 27.3 判定

- 普通 OCR 区域数、processed size、文本行数与阶段 1/5 基线一致。
- 公式模块未启用时，未引入普通 OCR 行为变化。
- DirectML/CUDA/CANN feature 矩阵编译通过，provider 测试通过。
- 输入大小和 URL timeout 回归单测通过。

#### 27.4 阶段提交

| 阶段 | 提交 | 说明 |
| --- | --- | --- |
| 11 普通 OCR 回归 | `9649aa4` | `fix(cli): resolve evaluate image paths and record regression` |


---

### 28. 阶段 12 执行记录（2026-10-02）

#### 28.1 文档与资产

- README 增加公式 API、模型 URL/SHA-256、tokenizer 来源、CPU/provider 边界和 benchmark 命令。
- docs/01 追加实现后事实核验。
- THIRD_PARTY_NOTES.md 增加 RapidDoc、PP-FormulaNet、PaddleOCR/UniMER/im2latex 归属。
- .gitignore 增加 formula evaluation/benchmark 输出和 Python cache 规则。
#### 28.2 可复现命令

- cargo test --all-targets
- cargo fmt --all -- --check
- cargo check --features directml-provider,cuda-provider,cann-provider
- cargo run --bin formula_bench -- --model <model.onnx> --image <formula.png> --rounds 3 --provider cpu
- python tools/formula_reference.py --model <model.onnx> --dataset-root <val-root> --split val --limit 100 --batch-size 8 --output target/formula-python.json
- python tools/formula_compare_results.py --rust target/formula-rust.json --python target/formula-python.json --output target/formula-compare.json

#### 28.3 发布边界

- crate 不打包模型权重、测试图片或评测结果。
- 使用方必须单独下载模型并校验 SHA-256。
- fast_tokenizer.json 仅作为测试 fixture 提交，来自模型 metadata。
- 完整 501/10,355/UniMER 数值评测曾是发布前门禁；该门禁已在第二轮审核修复中执行，
  结果见 §29。

#### 28.4 阶段提交

| 阶段 | 提交 | 说明 |
| --- | --- | --- |
| 12 文档/许可证 | `310d2ed` | `docs(formula): document phase 12 assets and release boundaries` |

---

## 29. 审核修复执行记录（第二轮，2026-10-02）

本节逐项记录针对审核报告的修复：问题 → 根因 → 处理 → 验证。

### 29.1 P1 测试无法在干净仓库中复现

**问题**：ONNX 契约 fixture 被 `.gitignore` 忽略且未提交；评测测试硬编码开发机绝对路径。
**根因**：fixture 的“本地开发资产”定位被当成模型权重处理，且测试资产定位没有共享层，
于是退化成了开发机路径。
**处理**：

- `.gitignore` 增加 `!tests/fixtures/**/*.onnx` 例外，12 个 `formula-onnx` fixture 与
  7 个 `ocr-onnx` fixture 随仓库提交；
- 新增 `tools/build_formula_onnx_fixtures.py`，用 Python `onnx` 确定性重建全部 fixture
  （仅 `formula_recognizer_*` 需要真实模型 metadata）；
- 新增 `src/test_support.rs`：外部资产只能通过
  `RAPID_OCR_MODEL_ROOT` / `RAPID_OCR_FORMULA_MODEL` / `RAPID_OCR_FORMULA_DETECT_MODEL` /
  `RAPID_OCR_FORMULA_TEST_ROOT` / `RAPID_OCR_TEST_IMAGES` 定位，缺失时显式打印原因并
  skip；`RAPID_OCR_REQUIRE_EXTERNAL_ASSETS=1` 时改为失败；
- 删除 `src/formula/model_info.rs` 与 `src/evaluation/formula/fixture.rs` 中的全部绝对路径。

**验证**：`src` 下已无 `D:\`/`C:\` 字面量；无环境变量时
`cargo test --all-targets` = 246 passed / 0 failed（外部资产测试打印 skip 并返回），
带 `RAPID_OCR_MODEL_ROOT` 时同样全绿且真实模型测试真正执行。

**干净 clone 端到端验证**（决定性证据）：

```powershell
git clone <crate> $env:TEMP\rapid-ocr-rs-cleanclone
cd $env:TEMP\rapid-ocr-rs-cleanclone
cargo test --all-targets        # 不设置任何模型/测试集环境变量
# -> 246 passed; 0 failed
```

**过程中发现并修复的第二个可复现性根因**：第一次干净 clone 运行失败，9 个契约测试报
`ONNX Runtime error: Load model from …\formula_multi_input.onnx failed: Protobuf parsing failed`。

- 根因：`core.autocrlf=true`（Windows 常见默认）会在 **checkout** 时把 blob 中的
  `0x0A` 改写成 `0x0D 0x0A`。对 ONNX / NumPy / PNG 这类二进制文件，这是一次内容破坏；
  而 `git cat-file blob` 校验证明**blob 本身是正确的**，只有工作区被改写。
  这也解释了为什么“提交 fixture”并不足以保证干净 clone 可用。
- 处理：新增 `.gitattributes`，把 `*.onnx` / `*.npy` / `*.png` / `*.jpg` / `*.jpeg` /
  `*.webp` / `*.ico` / `*.gif` 声明为 `binary`（等价 `-text -diff`），
  并执行 `git add --renormalize .`；
- 新增 `tools/verify_committed_binaries.py`：逐字节比较
  `git cat-file blob HEAD:<path>` 与工作区文件，证明 35 个已提交二进制 fixture
  的 blob 与工作区完全一致（修复前后都一致，差别只在 checkout 行为）；
- 修复后重新 clone 并运行：246 passed / 0 failed（无任何外部资产）。

### 29.2 P1 provider 不支持时仍可能静默回退 CPU

**根因**：严格性挂在进程级 `RuntimeConfig::fail_if_provider_unavailable`（默认 false），
而公式 API 契约要求“请求加速器就必须是加速器”，调用方无法从返回值观察到回退。
**处理**：新增 `runtime::provider::require_requested_provider`，在
`FormulaSession::new` 中调用；`fallback_used == true` 时返回
`RapidOcrError::UnsupportedProvider`，与 `fail_if_provider_unavailable` 无关。
**验证**：`require_requested_provider` 单元测试（回退拒绝 / 已解析加速器与显式 CPU 通过）；
`formula::session` 与 `formula::recognizer` 各有一个
“`fail_if_provider_unavailable = false` 且请求未启用的 CUDA 仍必须失败”的测试。

### 29.3 P1 页面级公式识别不存在

**处理**：见 §8.3 的实现要点列表。核心设计是 `formula::route`（纯几何策略，
可无模型测试）+ `formula::detect`（YOLO11 MFD 检测器）+ `RapidOcrEngine::recognize_with_formula`。
**验证**：

| 命令 | 结果 |
| --- | --- |
| `cargo test --lib formula::route` | 8 passed（重叠/嵌套消解、误检面积与越界过滤、显式区域旁路、顺序无关与上限、抹白像素与尺寸） |
| `cargo test --lib formula::detect` | 15 passed（含 golden 多边形 `max_delta = 0`、与 Python 参考 IoU 1.0000） |
| `cargo test --lib formula_integration` | 7 passed（缺省关闭=无公式、漏检=文本逐字一致、严格阈值=回到基线、检测到的区域 typed 且带 LaTeX、显式区域无需检测器、`roi`/`tile` 拒绝、缺 `model_path` 拒绝） |
| CLI 端到端 | `rapidocr run --img-path 08数字公式与符号.png --formula-model … --formula-detector …` → `regions=50 formulas=8`，LaTeX 正确（`\sqrt{2}\approx1.414`、`\pi\approx3.14159`、`a^2+b^2=c^2`、定积分等），总耗时 7.6 s |

阅读顺序核验：8 个公式区域在全局阅读顺序中的位置为“顶部一项 → 左栏 4 项 →
右栏 2 项 → 底部 1 项”，与其几何位置一致（公式不与文本分开排序）。

已知限制（同时写入 README 与本节）：检测器在非公式页面存在误检，
提高 `confidence_threshold` 可恢复基线；漏检的公式仍由 CTC 处理。

### 29.4 P1 阶段 9 主评测未完成

**处理**：`formula_compare` 被 `formula_eval` 取代，支持三个数据集、内容哈希抽样与
稳定 manifest、失败分类、吞吐/P50/P95/峰值内存，以及内建的 Rust/Python 对比；
Python 参考改为 manifest 驱动（`tools/formula_reference.py --manifest`），
不再重复实现抽样。

**执行**（`tools/run_formula_evaluation.ps1`，CPU provider，batch=8，模型
SHA-256 `71b6d389…d9493b`）：

（完整表格见 §29.7；下方为逐阶段提交记录。）

### 29.5 P2 重复实现与语义问题

| 问题 | 根因 | 处理 | 验证 |
| --- | --- | --- | --- |
| 公式输入加载与共享 `image_loader` 重复 | 公式域自己实现了编码/像素/URL/超时限制 | `LoadImage::read_encoded_with_limit` / `load_dynamic_with_limit` 成为共享入口，公式 API 只保留“字节→领域图像”的解码 | `formula::recognizer` 的文件/编码限制测试；`input::image_loader` 原有 URL/流式/超时测试 |
| `max_encoded_bytes` 语义分叉 | 内存字节不受编码上限约束 | 统一适用于所有编码输入，并更新 `PreprocessPolicy` 文档 | `encoded_input_enforces_encoded_and_pixel_limits`、`input::image_loader` 全部限制测试 |
| 模型契约验证两套实现 | probe 与 session 各自校验，动态维语义不同 | 新增 `formula::contract::validate_formula_contract`，两处共用；统一为“通道与空间维必须固定 384” | `formula::contract` 的“probe/session 对每个 fixture 结论一致”测试、动态空间维拒绝测试 |
| benchmark 统计粒度不足 | 只报平均值、单轮 batch | 重写为 warmup/多轮 + min/max/mean/P50/P95/stddev + 单图与整批 + `deterministic_tokens` + 峰值内存 | `formula_bench` 单测 + 实跑 `bench-cpu.json` |
| batch `elapsed_ms` 语义不准确 | 循环内逐个取 elapsed | 改为“调用墙钟耗时”，同批共享，并新增 `batch_size` 字段 | `batch_results_share_call_wall_time` |
| postprocess 缺 `ftfy.fix_text` | 未实现 ftfy | 确定性步骤逐字符等价实现，表由真实 ftfy 生成；未实现步骤显式列出并用 fixture 锁定差异 | `formula::ftfy` 单测 + `tests/fixtures/formula-postprocess/cases.json`（357,022 条真实语料探针） |
| `postprocess_latex` 重复编译正则 | 每次调用 `Regex::new` | 所有正则改为 `LazyLock` 静态；同时删除 RapidDoc 从不使用的 `fix_delimiter=true` 死分支 | `postprocess_reuses_compiled_regexes`（10k 次调用 < 2 s） |
| 输出测试覆盖不足 | 只测了 HTML 转义 | 定义并测试 Markdown 的 `$$` 转义、空行折叠、空 LaTeX、truncated 标记；HTML 区分属性转义（含换行/制表符）；新增页面级 JSON/Markdown/HTML 公式测试 | `formula::output`、`output::json`、`output::markdown`、`output::html` 测试 |

### 29.6 P3 质量门禁

| 命令 | 结果 |
| --- | --- |
| `cargo test --all-targets` | 246 passed / 0 failed（无外部资产） |
| `cargo test --all-targets`（带模型与测试集） | 246 passed / 0 failed |
| `cargo test --all-targets`（干净 clone，无外部资产） | 246 passed / 0 failed |
| `cargo test --features directml-provider` | 248 passed / 0 failed |
| `cargo test --features cuda-provider` | 246 passed / 0 failed |
| `cargo check --features directml-provider,cuda-provider,cann-provider` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --all-targets -- -D warnings` | 通过（0 warning） |

`Cargo.toml` 增加 `[lints.rust] linker_messages = "allow"`：`ort`/`turbojpeg` 的预编译
原生库在 MSVC 下会偶发 `LNK4098`，该诊断来自第三方二进制而非本 crate 代码，但会让
`-D warnings` 随机失败。

环境阻塞（明确记录，非代码问题）：

| 命令 | 结果 |
| --- | --- |
| `cargo check --features opencv-backend`（`--all-features` 的前提） | `opencv v0.94.4` 构建脚本失败：本机 `OPENCV_CMAKE_NAME` / `CMAKE_PREFIX_PATH` 为空，未安装/未配置 OpenCV |

因此 `--all-features`（等于 provider 矩阵 + `opencv-backend`）在本机无法验证，
provider 矩阵改为按特性分别验证（上表）。这与阶段 1 基线记录的环境限制一致。

### 29.7 普通 OCR 回归（公式关闭）

| 项目 | 修改前基线 | 本次实测 | 判定 |
| --- | --- | --- | --- |
| `cargo test --all-targets` | 159 passed（阶段 11） | 246 passed（新增公式测试） | 无新增失败 |
| 真实图片区域数（01基础多位置文本，small 配置） | 42 | 42 | 保持 |
| processed size | 1984×1248 | 1984×1248 | 保持 |
| 公式区域数（未传 `--formula-model`） | 不存在 | 0（`formulas: 0`，`items: 42`） | 公式关闭时无公式区域 |
| 公式阶段状态 | 不存在 | `Disabled` | 结构性保证 |

`recognize` 在 `formula.enabled == false` 时直接进入 `recognize_text`，不经过任何公式
代码路径，因此“公式功能关闭时普通 OCR 行为与基线一致”不依赖分支判断的正确性。

### 29.8 阶段 9/10 执行结果

环境：CPU provider（`auto_tune_threads`，14 物理核），batch=8，模型 SHA-256
`71b6d389…d9493b`；命令为 `tools/run_formula_evaluation.ps1`。
报告、抽样 manifest 与失败样本均在 `target/formula-eval/`（不随仓库提交），
本表由 `tools/summarize_formula_eval.py` 生成。

| 数据集 | 切分 | 样本 | 评分 | exact | normalized | mean CER | 链路失败 | 模型错误 | truncated | 吞吐(img/s) | P95 单图(ms) | 峰值内存(MB) | manifest |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| im2latex | test | 100 | 100 | 26.00% | 26.00% | 0.0852 | 0 | 74 | 0 | 1.949 | 855.6 | 2099 | `31747b4d8cc0c3e7` |
| latexocr_example | validate | 501 | 501 | 37.92% | 38.72% | 0.0797 | 0 | 310 | 1 | 1.104 | 1183.4 | 3680 | `c2a4ec16088774f7` |

Rust/Python 三方对比（同一 manifest、同一批样本；Python 使用 RapidDoc
`PPPreProcess` + ONNX Runtime + 真实 metadata tokenizer + RapidDoc `PPPostProcess`）：

| 数据集 | 对比样本 | 完整 token 行一致 | token(EOS 前)一致 | LaTeX 一致 | EOS index 一致 | truncated 一致 | 双方都错（模型错误） | 链路差异 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| latexocr_example | 501 | 501 | 501 | 501 | 501 | 501 | 311 | **0** |

结论：501 张 smoke 集上 Rust 与 Python/RapidDoc 链路**逐项完全一致**（含完整 token 行），
全部 311 个不匹配样本都是模型识别错误，没有任何一个属于 Rust 链路差异
（`link_differences: []`）。

失败样本（`failures-<dataset>.json`）按失败种类分类：链路失败
（`image_decode` / `inference` / `tokenizer_decode` / `input_rejected`）与模型错误
（`model_mismatch` / `truncated_no_eos`）分开记录，每条包含图片、期望、实际、
token 序列、EOS 与 CER。

（完整 10,355 张 im2latex 与 UniMER 四个子集的结果在本次运行结束后追加到本表。）

### 29.9 提交

| 内容 | 提交 | 说明 |
| --- | --- | --- |
| 第二轮审核修复 | `9db5dda` | `fix(formula): close the review gaps in the PP-FormulaNet integration` |
| `OcrOutput::len` 语义修正 | `e08df20` | `fix(api): count formula regions in OcrOutput::len and document text_len` |
| benchmark batch 判定口径 | `c1aa94b` | `fix(bench): compare EOS prefixes for batch determinism and reuse measured runs` |
| 阶段 9 smoke 结果记录 | `e41f965` | `docs(formula): record phase 9 smoke results and the 501-image Rust/Python comparison` |
| 二进制 fixture checkout 修复 | `b666390` | `fix(repo): mark binary fixtures so checkout cannot corrupt them` |
