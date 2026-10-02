# PP-FormulaNet_plus-M ONNX 转换、部署与 rapid-ocr-rs 集成方案

> 目标：把 `PP-FormulaNet_plus-M` 公式识别模型接入 `rapid-ocr-rs`。  
> 原则：不重新训练；不把公式模型塞进现有 CTC OCR 流程；优先复用有明确来源、固定哈希和工程实现的 ONNX 模型，并对本地模型重新回归。

---

## 一、最终结论与推荐路线

### 1.1 核心结论

可以转换，但 `PP-FormulaNet_plus-M` 不是普通 CTC OCR 模型，不能只执行一条 `paddle2onnx` 命令后就直接接入当前 `Recognizer`。

转换 ONNX 不需要重新训练。它本质上是：

```text
已有模型权重
  -> 导出静态计算图
  -> 转换算子和权重格式
  -> ONNX
```

真正困难的地方不在权重，而在：

- decoder 的循环生成；
- KV cache 或单步推理；
- 动态控制流；
- tokenizer；
- Paddle 与 ONNX 的算子数值差异；
- 输出是 `INT64` token 序列，而不是普通 OCR 的 `FLOAT` 特征图。

### 1.2 对 rapid-ocr-rs 的最终推荐

推荐采用“三层次方案”：

| 层次       | 方案                                                   | 用途                               |
| ---------- | ------------------------------------------------------ | ---------------------------------- |
| 第一版实现 | RapidDoc 的 `PP-FormulaNet_plus-M` ONNX                | 首选模型与算法参考                 |
| 交叉验证   | MinerU 单文件 `PP-FormulaNet_plus-M` ONNX              | 验证 token / LaTeX 一致性          |
| 长期优化   | docparser-rs 的 encoder / prep / decoder_step 拆分模型 | 支持取消、流式解码、细粒度性能控制 |

自行转换，例如 `FahNos/pp_formula_to_onnx` 或 `p2o`，适合研究、诊断和验证，不建议直接作为生产首选。

### 1.3 推荐实施顺序

1. 使用本地 Paddle 模型作为基准：
   ```text
   OCR-Model/Formula-Recognition-Models/PP-FormulaNet_plus-M_infer.tar
   ```
2. 优先下载并评估 RapidDoc 的 `PP-FormulaNet_plus-M` ONNX。
3. 同时下载 MinerU M ONNX，作为交叉验证模型。
4. 补齐并验证 tokenizer。
5. 用相同公式图片比较 Paddle 与 ONNX 的：
   - token 序列；
   - EOS 位置；
   - LaTeX；
   - 空白/裁剪预处理。
6. 首版只支持 CPU。
7. 验证通过后新增 `FormulaRecognizer`。
8. 后续需要取消、流式解码或更细粒度性能控制时，再切换 docparser 拆分模型。

Paddle 官方已记录过 PP-FormulaNet 直接用 Paddle2ONNX 转换后输出不一致的问题：

- [PaddleOCR Issue #15231](https://github.com/PaddlePaddle/PaddleOCR/issues/15231)

因此，不建议当前直接把本地 `.tar` 通过 Paddle2ONNX 转换后作为正式模型。

---

## 二、关键认知：为什么不能直接接入现有 Recognizer

### 2.1 不需要重新训练

你本地有两个关键文件：

```text
PP-FormulaNet_plus-M_pretrained.pdparams
```

这是训练得到的权重。使用专用导出脚本时，需要根据模型结构重新构建网络并加载它，但这只是导出，不是训练。

```text
PP-FormulaNet_plus-M_infer.tar
```

这是已经导出的 Paddle 推理模型，可以尝试直接用 `paddle2onnx` 转换。

不需要：

- 训练数据；
- 重新跑 epoch；
- 重新优化权重；
- 重新标注公式。

只有以下情况才需要训练或微调：

- 公式领域与原模型差异很大；
- 手写公式、低清截图效果不足；
- 需要特定符号或中文公式；
- 转换后的模型确实需要重新训练以适应新结构。

当前任务属于“模型部署转换”，不是“模型训练”。

### 2.2 公式模型与普通 CTC OCR 的差异

当前普通 OCR 的 `OrtSession` 不能直接加载公式模型：

| 项目     | 普通 OCR            | PP-FormulaNet_plus-M           |
| -------- | ------------------- | ------------------------------ |
| 输出类型 | `FLOAT`，rank 3     | `INT64`，rank 2                |
| 解码方式 | CTC                 | MBart 自回归解码               |
| 解码器   | `CtcLabelDecoder`   | tokenizer + 循环生成           |
| 输入形状 | 常见 `[B, 3, H, W]` | 常见 `[B, 1, 384, 384]`        |
| 控制流   | 前向一次            | 图内 `Loop` 或外部逐步 decoder |
| 结果     | 文本行              | LaTeX 公式                     |

因此必须新增独立模块：

```text
FormulaRecognizer
FormulaSession
FormulaTokenizer
```

不要把公式模型塞进现有 `Recognizer` 或 `CtcLabelDecoder`。

### 2.3 公式模型推理流程

单文件 ONNX 方案通常为：

```text
公式图片
  -> 预处理
  -> ONNX 推理
  -> token_ids
  -> tokenizer
  -> LaTeX
```

拆分 ONNX 方案通常为：

```text
公式图片
  -> backbone.onnx
  -> encoder features
  -> head_fixed.onnx，循环生成 token
  -> tokenizer
  -> LaTeX
```

`head_fixed.onnx` 每次只生成一步 token，Rust 端需要实现：

```text
tokens = [BOS]
循环:
    logits = head(decoder_input_ids, encoder_output)
    token = argmax(logits)
    追加 token
    token == EOS 时停止
```

当前模型的特殊 token 通常为：

```text
BOS = 0
PAD = 1
EOS = 2
```

最终还要根据 `inference.yml` 或模型 metadata 中的 tokenizer 将 token ID 解码为 LaTeX。当前 `CtcLabelDecoder` 不能复用。

---

## 三、模型来源与项目评估

评估模型来源时，重点看：

- 是否有明确许可证；
- 是否有 SHA-256；
- 是否有可复现的 token / LaTeX 验证；
- 是否提供 tokenizer；
- 输入输出签名是否清晰；
- 是否适合 Rust + ONNX Runtime；
- GPU 支持是否存在已知问题。

### 3.1 首选：RapidDoc

项目地址：

- [https://github.com/RapidAI/RapidDoc](https://github.com/RapidAI/RapidDoc)

你的判断基本正确：如果目标是把 `PP-FormulaNet_plus-M` 接入 `rapid-ocr-rs`，RapidDoc 比 MinerU 更适合作为第一实现参考。

但应复用它的公式模型、预处理和后处理设计，而不是把整个 Python RapidDoc 引入 Rust。

#### RapidDoc 的完整链路

RapidDoc 已经提供：

```text
公式图像
→ 裁剪空白边缘
→ resize / padding 到 384×384
→ 归一化后灰度化；处理中间会复制为 3 通道
→ 最终取单通道并形成 [N, 1, 384, 384]
→ ONNX 推理
→ token ID
→ tokenizer 解码
→ LaTeX 清理
```

相关源码：

- [公式模型配置](https://github.com/RapidAI/RapidDoc/blob/main/rapid_doc/model/formula/rapid_formula_self/configs/default_models.yaml)
- [ONNX Runtime 后端](https://github.com/RapidAI/RapidDoc/blob/main/rapid_doc/model/formula/rapid_formula_self/inference_engine/onnxruntime/main.py)
- [公式预处理](https://github.com/RapidAI/RapidDoc/blob/main/rapid_doc/model/formula/rapid_formula_self/model_handler/pp_formulanet_plus/pre_process.py)
- [公式后处理](https://github.com/RapidAI/RapidDoc/blob/main/rapid_doc/model/formula/rapid_formula_self/model_handler/pp_formulanet_plus/post_process.py)

#### RapidDoc 模型信息

其 M 版 ONNX 模型信息为：

```text
大小：593,915,961 bytes
SHA-256：
71b6d389cf7b857e45252a4b98cfced1a3ffca7bf24d9497d02d052a41d9493b
```

下载地址：

```text
https://www.modelscope.cn/models/RapidAI/RapidDoc/resolve/v1.0.0/formula/PP-FormulaNet_plus-M/pp_formulanet_plus_m.onnx
```

RapidDoc 的模型配置还明确记录了 SHA-256，并在下载时进行校验，这比很多只提供一个匿名 ONNX 文件的仓库更适合工程集成。

RapidDoc 的 `v1.0.0` 模型目录主要提供 ONNX 文件；ModelScope `master` 分支后来补充了
`pp_formulanet_plus_m_inference.yml`，其中包含 `PostProcess.character_dict` 和
`fast_tokenizer_file`。如果 ONNX metadata 中没有 `character`，应固定下载这个侧车文件，不能依赖
运行时自动从 GitHub 或 PaddleOCR 工作区寻找词表。

当前已核验的侧车文件信息：

```text
revision: 04e3ca19276e0fbffbb855610a99530fdc04586f
sha256:   87b5f3d7f2b2fe553627d77b37f496608ca150ebd0ef62d362591edca47b5538
```

固定 revision 的下载地址：

```text
https://www.modelscope.cn/models/RapidAI/RapidDoc/resolve/04e3ca19276e0fbffbb855610a99530fdc04586f/formula/PP-FormulaNet_plus-M/pp_formulanet_plus_m_inference.yml
```

#### RapidDoc 与 MinerU 模型的区别

RapidDoc 使用的是：

```text
SHA-256: 71b6d389cf7b857e45252a4b98cfced1a3ffca7bf24d9497d02d052a41d9493b
大小: 593,915,961 bytes
```

这与 `jinzhenj/PP-FormulaNet_plus-M_onnx` 的 ONNX 文件一致，也与 MinerU 旧版来源记录一致。

MinerU 当前已经替换成了另一份模型：

```text
SHA-256: 20a32595c2b30282dcd069e6295e9fad96115697beb66acb9b681b814006d193
大小: 591,263,297 bytes
来源: MinerU-4_models_torch 的 PTH 转换
```

因此：

- RapidDoc 不是 MinerU 当前的新 Torch 导出；
- 两者不能简单认为是同一个 ONNX；
- RapidDoc 模型更适合复用其既有 Python 预处理和 tokenizer 逻辑；
- MinerU 模型拥有更完整的 153 张图片逐 token / LaTeX 验证报告。

#### RapidDoc 对 rapid-ocr-rs 的适配性

RapidDoc 的公式 ONNX 很适合单文件 Rust 接入：

```text
输入：公式图像
输出：生成 token 序列
```

RapidDoc 代码要求从 ONNX 模型 metadata 的 `character` 字段读取 tokenizer 配置，而不是额外依赖 Paddle 推理运行时。这一点对 Rust 很有利；但当前尚未在本机下载的 RapidDoc 模型上完成 metadata probe，因此不能把它写成已经完成的本地验证结论。

但当前仍需在本地正式确认：

1. ONNX 输入名称、dtype、shape；
2. 输出是否确实为 `INT64 [batch, length]`；
3. `character` metadata 是否存在；
4. Rust `ort` 是否能成功加载；
5. CPU 下是否与本地 Paddle M 模型逐 token 一致。

#### RapidDoc 的限制

RapidDoc 自己明确说明：

- CPU 使用 ONNX；
- CUDA 环境默认使用 Torch；
- 公式 ONNX 在 GPU 上存在报错问题；
- 相关问题仍未完全解决。

所以它适合先作为：

```text
CPUExecutionProvider + PP-FormulaNet_plus-M
```

不要一开始就承诺 CUDA、DirectML 或 CANN 支持。

另外，RapidDoc 仓库本身没有看到专门的公式识别回归测试集。它的模型可用性主要由实际项目使用和配置保证，不能替代我们自己的 Paddle-vs-ONNX 回归测试。

RapidDoc 软件仓库使用 Apache-2.0，ModelScope 的 `RapidAI/RapidDoc` 模型仓库元数据也标注 Apache License 2.0。PP-FormulaNet 原始 Paddle 模型同样标注 Apache-2.0。正式发布时仍应在 `THIRD_PARTY_NOTES.md` 中记录 RapidDoc、ModelScope、PaddleOCR 归属、固定模型 URL 和 SHA-256；这不等于可以省略第三方模型归属审查。

### 3.2 交叉验证：MinerU 单文件 ONNX

模型地址：

- [MinerU-4_models_onnx](https://huggingface.co/opendatalab/MinerU-4_models_onnx)
- 文件：`MFR/pp_formulanet_plus_m/PP-FormulaNet_plus-M.onnx`

已核验信息：

| 项目     | 内容                                                         |
| -------- | ------------------------------------------------------------ |
| 大小     | 591,263,297 bytes                                            |
| SHA-256  | `20a32595c2b30282dcd069e6295e9fad96115697beb66acb9b681b814006d193` |
| 输入     | `encoder/image`，`FLOAT [batch, 1, 384, 384]`                |
| 输出     | `token_ids`，`INT64 [batch, length]`                         |
| ONNX     | opset 17，IR 8                                               |
| 解码方式 | 图内部 `Loop` 自回归解码                                     |
| 运行时   | ONNX Runtime，要求 >= 1.20.1                                 |
| 验证     | 153 张真实图片，token 和 LaTeX 均 153/153 匹配               |
| 限制     | 生成到第 1536 个 token 时强制 EOS                            |

它最适合先接入 `rapid-ocr-rs` 的单文件方案，因为 Rust 只需要：

```text
图像预处理 -> ONNX 推理 -> token_ids -> tokenizer -> LaTeX
```

MinerU 仓库没有单独提供 `tokenizer.json`，但提供了 `PP-FormulaNet_plus-M_inference.yml` 侧车文件；该文件来自对应的 `jinzhenj/PP-FormulaNet_plus-M_onnx` 推理资源，包含公式后处理所需的配置/词表信息。正式接入前仍需确认 Rust tokenizer 能否按该侧车文件复现 Paddle 的 token ID 解码，不能简单假定 docparser 的 `tokenizer.json` 可以直接替代。

因此，MinerU 更适合作为“有强验证证据的交叉验证模型”，而不是唯一来源。

### 3.3 长期优化：docparser-rs 拆分模型

模型地址：

- [docparser-models](https://huggingface.co/docparser-rs/docparser-models)

M 版包含：

```text
PP-FormulaNet_plus-M_encoder.onnx       约 277 MB
PP-FormulaNet_plus-M_prep.onnx          约 16.8 MB
PP-FormulaNet_plus-M_decoder_step.onnx  约 298 MB
PP-FormulaNet_plus-M_tokenizer.json     约 2.14 MB
```

特点：

- Apache-2.0；
- 有 `MANIFEST.txt` 和 SHA-256；
- encoder 只执行一次；
- prep 预计算 cross-attention K/V；
- `decoder_step` 每次生成一个 token；
- Rust 主程序控制 EOS、最大长度、取消和超时；
- 不依赖 ONNX 图内部 `Loop`。

它与 `rapid-ocr-rs` 的长期架构更匹配，但实现量明显大于 MinerU 单文件方案。

### 3.4 其他模型与项目评价

#### `jinzhenj/PP-FormulaNet_plus-M_onnx`

地址：

- [https://huggingface.co/jinzhenj/PP-FormulaNet_plus-M_onnx](https://huggingface.co/jinzhenj/PP-FormulaNet_plus-M_onnx)

信息：

- `inference.onnx` 约 593.9 MB；
- 元数据标注 Apache-2.0；
- 可作为 MinerU 模型的交叉验证来源。

但它缺少 MinerU 那种清晰的 153 图验证报告，因此更适合对照，不建议优先采用。

#### `x3zvawq/paddleocr-js-onnx`

地址：

- [https://huggingface.co/x3zvawq/paddleocr-js-onnx](https://huggingface.co/x3zvawq/paddleocr-js-onnx)

M 版文件约 720 MB，仓库声称来自官方 Paddle 静态模型转换。

问题：

- Hugging Face 元数据没有明确许可证；
- 没有 MinerU 那样完整的 token/LaTeX 回归证据；
- 需要下载后检查真实输入输出和 Loop 支持；
- 不建议直接作为 crate 默认模型或发布包内容。

#### `oar-ocr`

地址：

- [https://github.com/GreatV/oar-ocr](https://github.com/GreatV/oar-ocr)

这是目前最值得参考的 Rust 实现之一。它已经处理了：

- PP-FormulaNet 输入预处理；
- `INT64 [B, L]` token 输出；
- BOS/EOS/pad 过滤；
- tokenizer 解码；
- ONNX 输出不按顺序假设，而是主动寻找唯一的二维 INT64 输出。

不过它的实现和模型来源仍需逐项审计，适合作为工程参考，不应直接等同于已经验证的依赖。

#### `FahNos/pp_formula_to_onnx`

地址：

- [https://github.com/FahNos/pp_formula_to_onnx](https://github.com/FahNos/pp_formula_to_onnx)

优点是提供了完整转换脚本：

```text
export_onnx.py
fix_head_onnx.py
onnx_predict_pp_formualnet_plus_M.py
```

但它属于社区转换方案，没有 MinerU 或 docparser 的完整模型校验和发布清单。适合研究转换过程，不建议作为首选生产模型。

#### `p2o`

地址：

- [https://github.com/GreatV/p2o](https://github.com/GreatV/p2o)

这是 Paddle PIR 到 ONNX 的 Rust 转换器：

```bash
p2o inference.json inference.pdiparams output.onnx --opset 17
```

适合未来自行转换 L/M 模型，但 PP-FormulaNet 是自回归模型，转换成功不代表输出语义正确，仍必须做 Paddle 与 ONNX 的逐 token 对比。

### 3.5 模型 SHA-256 对比速查

| 来源        |              大小 | SHA-256                                                      | 备注                     |
| ----------- | ----------------: | ------------------------------------------------------------ | ------------------------ |
| RapidDoc    | 593,915,961 bytes | `71b6d389cf7b857e45252a4b98cfced1a3ffca7bf24d9497d02d052a41d9493b` | 首选第一实现参考         |
| MinerU 当前 | 591,263,297 bytes | `20a32595c2b30282dcd069e6295e9fad96115697beb66acb9b681b814006d193` | 153 图验证充分           |
| jinzhenj    |       约 593.9 MB | 与 RapidDoc 一致                                             | 可交叉验证               |
| x3zvawq     |         约 720 MB | 未核验                                                       | 许可证/回归证据仍需单独核验 |

---

## 四、rapid-ocr-rs 集成设计

### 4.1 目标架构

建议新增独立模块：

```text
rapid-ocr-rs
└── FormulaRecognizer
    ├── FormulaSession
    ├── FormulaTokenizer
    ├── 预处理：裁剪、resize、padding、归一化
    └── 后处理：token 过滤、LaTeX 清理
```

第一阶段可加载：

```text
RapidDoc PP-FormulaNet_plus-M ONNX
模型 metadata 中的 `character` 配置
```

若实际模型没有 `character` metadata，再将 `inference.yml` / `tokenizer.json` 作为显式外部 tokenizer 资产；两种来源必须通过 token ID 和最终 LaTeX 回归确认一致。

长期可切换为：

```text
PP-FormulaNet_plus-M_encoder.onnx
PP-FormulaNet_plus-M_prep.onnx
PP-FormulaNet_plus-M_decoder_step.onnx
PP-FormulaNet_plus-M_tokenizer.json
```

### 4.2 第一阶段：RapidDoc 单文件接入

Rust 侧只需要：

```text
图像预处理 -> ONNX 推理 -> token_ids -> tokenizer -> LaTeX
```

需要确认：

1. ONNX 输入名称、dtype、shape；
2. 输出是否确实为 `INT64 [batch, length]`；
3. `character` metadata 是否存在；
4. Rust `ort` 是否能成功加载；
5. CPU 下是否与本地 Paddle M 模型逐 token 一致；
6. tokenizer 特殊 token 是否与 Paddle 一致；
7. 预处理是否与 RapidDoc 完全一致。

注意：RapidDoc 预处理代码中确实有“灰度化并复制为 3 通道”的中间步骤，但 `LatexImageFormat` 随后只取第一个通道，最终张量是 `[N, 1, 384, 384]`。本地 Paddle `inference.yml` 与 MinerU 也显示 `[N, 1, 384, 384]`。因此 Rust 应复现最终单通道张量，并以实际 ONNX session 的输入签名作最后校验。

### 4.3 第二阶段：docparser 拆分模型

当需要以下能力时，切换到 docparser 拆分模型：

- 取消推理；
- 流式解码；
- 超时控制；
- 最大长度控制；
- 更细粒度性能控制；
- 不依赖 ONNX 图内部 `Loop`。

拆分模型下，Rust 主程序控制：

```text
tokens = [BOS]
循环:
    logits = decoder_step(decoder_input_ids, encoder_output, cross_kv)
    token = argmax(logits)
    追加 token
    token == EOS 时停止
```

### 4.4 不要修改现有 CTC 模块

保持现有普通 OCR 的 `Recognizer` 与 `CtcLabelDecoder` 不变。公式识别作为独立能力接入：

```text
普通 OCR -> Recognizer / CtcLabelDecoder
公式识别 -> FormulaRecognizer / FormulaSession / FormulaTokenizer
```

---

## 五、备选：自行转换 ONNX

### 5.1 使用 FahNos 项目拆分 Backbone + Decoder

项目：

- [https://github.com/FahNos/pp_formula_to_onnx](https://github.com/FahNos/pp_formula_to_onnx)

它解决了几个关键问题：

- 将公式模型改成单步 decoder；
- 分离视觉 Backbone 和 Transformer Head；
- 修复 ONNX 中的 `float64`；
- 支持 ONNX Runtime 推理。

#### 准备环境

建议在 WSL2 / Ubuntu 中执行转换，Windows 原生 Paddle2ONNX 的兼容性相对差。

```bash
git clone https://github.com/FahNos/pp_formula_to_onnx.git
cd pp_formula_to_onnx

python3 -m venv .venv
source .venv/bin/activate

pip install -r requirements_onnx.txt
```

该项目当前要求大致为：

```text
paddlepaddle==3.1.0
paddle2onnx
onnx
onnxruntime
numpy==1.26.4
```

#### 准备权重

你本地已经有：

```text
OCR-Model/formula_recognition_models/
└── PP-FormulaNet_plus-M_pretrained.pdparams
```

复制到转换项目的：

```text
pretrained_model/PP-FormulaNet_plus-M_pretrained.pdparams
```

#### 执行转换

```bash
python tools/export_onnx.py \
  --config configs/rec/PP-FormulaNet_plus-M_ONNX.yaml
```

然后修复 Head：

```bash
python tools/fix_head_onnx.py
```

通常会得到类似：

```text
output/onnx_models/backbone.onnx
output/onnx_models/head_fixed.onnx
```

#### 输出不是一个完整公式识别模型

转换结果是两部分：

```text
公式图片
  -> backbone.onnx
  -> encoder features
  -> head_fixed.onnx，循环生成 token
  -> tokenizer
  -> LaTeX
```

`head_fixed.onnx` 每次只生成一步 token，Rust 端需要实现循环生成。特殊 token 通常为：

```text
BOS = 0
PAD = 1
EOS = 2
```

最终还要根据 `inference.yml` 中的 tokenizer 将 token ID 解码为 LaTeX。当前 `CtcLabelDecoder` 不能复用。

### 5.2 直接使用 paddle2onnx 的实验路径

你也可以先解压现有推理包：

```bash
mkdir PP-FormulaNet_plus-M_infer
tar -xf PP-FormulaNet_plus-M_infer.tar \
  -C PP-FormulaNet_plus-M_infer
```

然后尝试：

```bash
paddle2onnx \
  --model_dir PP-FormulaNet_plus-M_infer/PP-FormulaNet_plus-M_infer \
  --model_filename inference.json \
  --params_filename inference.pdiparams \
  --save_file PP-FormulaNet_plus-M.onnx \
  --opset_version 17 \
  --enable_onnx_checker True
```

但这条路径存在两个问题：

1. 官方 PaddleOCR 已有人报告 PP-FormulaNet 转换后 Paddle 与 ONNX 输出 token 不一致；
2. 即使转换成功，也可能无法正确处理自回归解码。

参考：

- [PaddleOCR Issue #15231](https://github.com/PaddlePaddle/PaddleOCR/issues/15231)
- [PaddleOCR Paddle2ONNX 文档](https://github.com/PaddlePaddle/PaddleOCR/blob/main/deploy/paddle2onnx/readme.md)

因此，直接转换只能用于诊断，不建议直接用于生产。

### 5.3 何时需要训练或微调

只有以下情况才需要训练或微调：

- 公式领域与原模型差异很大；
- 手写公式、低清截图效果不足；
- 需要特定符号或中文公式；
- 转换后的模型确实需要重新训练以适应新结构。

当前任务属于“模型部署转换”，不是“模型训练”。

---

## 六、数值回归与验收

至少验证同一张公式图像：

```text
Paddle 原生输出 token
ONNX 输出 token
最终 LaTeX
```

验收顺序应为：

1. 预处理 tensor 完全一致；
2. Backbone 输出误差可接受；
3. Head 单步 logits 的误差可接受；
4. token 序列一致或语义等价；
5. LaTeX 渲染结果一致；
6. EOS 位置一致；
7. 空白/裁剪预处理一致；
8. ONNX session 输入签名与 `[N, 1, 384, 384]` 一致；
9. tokenizer 与 Paddle 端 token ID 一致；
10. Rust `ort` 能成功加载模型。

### 关键验收清单

- [x] 预处理 tensor 完全一致（归一化张量 `max_abs <= 1e-5`，常量样本 bit-exact；见 `docs/02` 阶段 4）；
- [x] Backbone 输出误差可接受（由端到端 token 一致性间接覆盖；未单独导出 backbone 张量）；
- [x] Head 单步 logits 的误差可接受（由端到端 token 一致性间接覆盖；未单独导出 logits）；
- [x] token 序列一致或语义等价（501 张 val 的 Rust/Python 逐项对比）；
- [x] LaTeX 渲染结果一致（同上，且差异按链路/模型两类统计）；
- [x] EOS 位置一致（同上）；
- [x] 空白/裁剪预处理一致（纯白/全黑/单像素/窄/宽/透明样本 + Python golden）；
- [x] ONNX session 输入签名正确（`FormulaModelInfo::probe` 与 `FormulaSession::new` 共用同一份契约校验）；
- [x] tokenizer 与 Paddle 端 token ID 一致（真实 metadata + 30 个 Python golden 序列）；
- [x] Rust `ort` 能成功加载模型；
- [x] 新增 `FormulaRecognizer`，不复用 CTC `Recognizer`；
- [x] 新增 `FormulaSession`，独立处理 `INT64` rank 2 输出；
- [x] 新增 `FormulaTokenizer`，独立解码公式 token；
- [x] 不修改现有 `CtcLabelDecoder`；
- [x] 首版仅支持 CPU `CPUExecutionProvider`（DirectML/CUDA 为可选特性，公式域拒绝静默回退）；
- [x] 后续再评估 docparser 拆分模型与流式解码（未实施，保留为后续方案）。

---

## 七、风险与限制

1. **RapidDoc GPU 问题**：CPU 使用 ONNX，CUDA 环境默认使用 Torch，公式 ONNX 在 GPU 上存在报错问题。
2. **RapidDoc 回归测试不足**：仓库本身没有看到专门的公式识别回归测试集，不能替代自己的 Paddle-vs-ONNX 回归。
3. **MinerU 解码资产不是 tokenizer.json**：MinerU 没有单独提供 `tokenizer.json`，但有 `inference.yml` 侧车；需要验证 Rust 是否能据此复现 token/LaTeX 解码，不能直接拿 docparser tokenizer 替换。
4. **docparser 实现量大**：拆分模型更适合长期架构，但 Rust 侧实现量明显大于单文件方案。
5. **直接转换不一致**：Paddle2ONNX 直接转换 PP-FormulaNet 已有输出不一致报告。
6. **输入形状易错**：本地 Paddle `inference.yml` 显示 `[N, 1, 384, 384]`，RapidDoc 预处理描述为灰度化并复制为 3 通道，必须以实际 ONNX session 签名为准。
7. **许可证与归属记录**：RapidDoc 软件和 ModelScope 模型仓库均标注 Apache-2.0；正式发布仍需保留 RapidDoc、ModelScope、PaddleOCR 归属、固定模型 SHA-256 和第三方说明。`x3zvawq` 的许可证和回归证据仍需单独核验。

---

## 八、参考链接汇总

### 转换与工具

- [FahNos/pp_formula_to_onnx](https://github.com/FahNos/pp_formula_to_onnx)
- [PaddleOCR Issue #15231](https://github.com/PaddlePaddle/PaddleOCR/issues/15231)
- [PaddleOCR Paddle2ONNX 文档](https://github.com/PaddlePaddle/PaddleOCR/blob/main/deploy/paddle2onnx/readme.md)
- [GreatV/oar-ocr](https://github.com/GreatV/oar-ocr)
- [GreatV/p2o](https://github.com/GreatV/p2o)

### ONNX 模型来源

- [MinerU-4_models_onnx](https://huggingface.co/opendatalab/MinerU-4_models_onnx)
- [docparser-models](https://huggingface.co/docparser-rs/docparser-models)
- [x3zvawq/paddleocr-js-onnx](https://huggingface.co/x3zvawq/paddleocr-js-onnx)
- [jinzhenj/PP-FormulaNet_plus-M_onnx](https://huggingface.co/jinzhenj/PP-FormulaNet_plus-M_onnx)

### RapidDoc

- [RapidAI/RapidDoc](https://github.com/RapidAI/RapidDoc)
- [公式模型配置](https://github.com/RapidAI/RapidDoc/blob/main/rapid_doc/model/formula/rapid_formula_self/configs/default_models.yaml)
- [ONNX Runtime 后端](https://github.com/RapidAI/RapidDoc/blob/main/rapid_doc/model/formula/rapid_formula_self/inference_engine/onnxruntime/main.py)
- [公式预处理](https://github.com/RapidAI/RapidDoc/blob/main/rapid_doc/model/formula/rapid_formula_self/model_handler/pp_formulanet_plus/pre_process.py)
- [公式后处理](https://github.com/RapidAI/RapidDoc/blob/main/rapid_doc/model/formula/rapid_formula_self/model_handler/pp_formulanet_plus/post_process.py)
- 模型下载：
  ```text
  https://www.modelscope.cn/models/RapidAI/RapidDoc/resolve/v1.0.0/formula/PP-FormulaNet_plus-M/pp_formulanet_plus_m.onnx
  ```

---

## 附录：关键信息速查

### 特殊 token

```text
BOS = 0
PAD = 1
EOS = 2
```

### rapid-ocr-rs 新增模块

```text
FormulaRecognizer
FormulaSession
FormulaTokenizer
```

### 最终推荐一句话

> **RapidDoc 更适合作为 `rapid-ocr-rs` 第一版公式识别的模型和算法参考；MinerU 更适合作为有强验证证据的交叉验证模型；docparser-rs 更适合作为后续性能优化的拆分解码方案。**

---

## 十、实现后事实核验（2026-10-02）

以下内容已由 Rust 实现与本地 fixture 验证，不再是待确认项：

- `pp_formulanet_plus_m.onnx` SHA-256：
  `71b6d389cf7b857e45252a4b98cfced1a3ffca7bf24d9497d02d052a41d9493b`；
- 输入：`x`，`FLOAT [N,1,384,384]`；
- 输出：`fetch_name_0`，`INT64 [N,L]`；
- IR 10 / opset 18，图内 `Loop`；
- metadata `character.fast_tokenizer_file` version 1.0，含 50,000 vocab、BPE merges、
  `<s>=0`、`<pad>=1`、`</s>=2`、`<unk>=3`；
- Rust `FormulaPreprocessor` 已复现 RapidDoc 裁剪、Bilinear/Bicubic resize、黑色画布
  居中、`mean=0.7931/std=0.1738` 与 BGR2GRAY 通道语义；
- Rust `FormulaTokenizer` 已用真实 metadata 和 30 个 Python `tokenizers` golden 序列
  验证；
- Rust `FormulaRecognizer` 已对真实 val 集固定子集与 Python ONNX Runtime 做
  token/latex/EOS 逐项比较；
- CPU 为第一版支持范围；DirectML/CUDA 已实测可加载并运行，但性能不保证优于 CPU。

---

## 十一、第二轮审核修复后的事实核验（2026-10-02）

### 11.1 新增能力

- **页面级公式路由**：`OcrRequest.formula: FormulaPolicy`（默认关闭）+
  `RegionKind::Formula` + `FormulaOutcome`。公式区域来自页面检测模型
  （`pix2text-mfd-1.5.onnx`，可选）与调用方显式声明的区域；公式像素在进入普通文本
  管线之前被抹白，因此 CTC 不会在公式上执行。
- **公式检测模型契约**（实测）：输入 `images` `FLOAT32 [N,3,H,W]`，输出 `output0`
  `FLOAT32 [N,6,A]`，IR 9 / opset 19，metadata `imgsz=[768,768]`、`stride=32`、
  `names={0:'embedding',1:'isolated'}`；导出的图已包含 DFL/dist2bbox/sigmoid，
  因此 6 个通道是 `[cx,cy,w,h,score0,score1]`（输入像素单位）。
  768×768 输入的 `A = 96² + 48² + 24² = 12096`。
- **评测工具**：`src/bin/formula_eval.rs`（三个数据集 + 稳定 manifest +
  失败分类 + 吞吐/P50/P95/峰值内存 + Rust/Python 对比）、
  `tools/formula_reference.py`（manifest 驱动）、
  `tools/summarize_formula_eval.py`、`tools/run_formula_evaluation.ps1`。
- **ftfy 等价边界**：确定性步骤已逐字符等价实现（表由真实 `ftfy` 生成），
  启发式 mojibake 修复与 `unescape_html` 显式未实现，并由
  `tests/fixtures/formula-postprocess/cases.json` 固化差异。

### 11.2 根因修正

- **序列长度上限**：模型图内 `Loop` 的输出宽度上限为 2561；当 batch 中任一 样本在
  Loop 预算内没有 EOS 时，ONNX Runtime 把整个 batch 补齐到 2561 列。原默认上限
  2560 会拒绝整批（实测 501 张 val 中 8 个 batch），连带丢掉同批识别正确的样本。
  现在默认 4096，并加入编译期断言
  `DEFAULT_MAX_FORMULA_SEQUENCE_LENGTH > FORMULA_MODEL_LOOP_BOUND`。
- **输入限制语义分叉**：公式 API 不再自己实现编码/像素/URL 限制，统一走共享
  `input::image_loader`；`max_encoded_bytes` 统一适用于所有编码输入。
- **契约校验重复**：`FormulaModelInfo::probe` 与 `FormulaSession::new` 共用
  `formula::contract::validate_formula_contract`，不再存在“探针成功但 session 失败”。
- **`sha256_file` 栈溢出**：1 MiB 栈缓冲区改为堆分配（Windows 主线程默认仅 1 MiB 栈）。
- **provider 静默回退**：公式域通过
  `runtime::provider::require_requested_provider` 拒绝 `fallback_used`，与
  `RuntimeConfig::fail_if_provider_unavailable` 无关。
- **batch 耗时语义**：`FormulaRecognition.elapsed_ms` 明确为“产生该结果的调用耗时”，
  同批结果共享同一值，并新增 `batch_size` 供折算。

### 11.3 测试资产与可复现性

- `tests/fixtures/**/*.onnx` 随仓库提交（`.gitignore` 例外
  `!tests/fixtures/**/*.onnx`），干净 clone 可运行全部契约测试；
- 所有开发机绝对路径已删除，外部模型/测试集通过
  `RAPID_OCR_MODEL_ROOT` / `RAPID_OCR_FORMULA_TEST_ROOT` /
  `RAPID_OCR_FORMULA_MODEL` / `RAPID_OCR_FORMULA_DETECT_MODEL` /
  `RAPID_OCR_TEST_IMAGES` 引用，缺失时显式 skip；
- `RAPID_OCR_REQUIRE_EXTERNAL_ASSETS=1` 可把 skip 变为失败，供已准备资产的环境使用。
