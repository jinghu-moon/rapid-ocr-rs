//! PP-FormulaNet `character` metadata 的 tokenizer 契约。
//!
//! 模型 metadata 的 `character` 字段是一个 JSON，内含 `fast_tokenizer_file`（HF
//! `tokenizer.json` 语义）与 `tokenizer_config_file`。本模块只解析并校验阶段 3/6
//! 需要的契约事实（特殊 token ID、vocab 规模、tokenizer 结构存在性），不复制
//! Hugging Face tokenizer 实现，也不把完整 JSON 写入源码或第二份真值。

use serde::{Deserialize, Serialize};

use crate::error::{RapidOcrError, Result};

/// `character` metadata 中固定的特殊 token ID。
pub const BOS_ID: i64 = 0;
pub const PAD_ID: i64 = 1;
pub const EOS_ID: i64 = 2;
pub const UNK_ID: i64 = 3;

pub const BOS_TOKEN: &str = "<s>";
pub const PAD_TOKEN: &str = "<pad>";
pub const EOS_TOKEN: &str = "</s>";
pub const UNK_TOKEN: &str = "<unk>";

/// 顶层 `character` JSON。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CharacterMetadata {
    pub fast_tokenizer_file: serde_json::Value,
    #[serde(default)]
    pub tokenizer_config_file: Option<serde_json::Value>,
}

/// 解析后的 tokenizer 契约事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaTokenizerMetadata {
    /// `fast_tokenizer_file.model.vocab` 的规模。
    pub vocab_size: usize,
    pub bos_id: i64,
    pub pad_id: i64,
    pub eos_id: i64,
    pub unk_id: i64,
    /// 原始 `fast_tokenizer_file`，供阶段 6 构造解码器。
    pub fast_tokenizer_file: serde_json::Value,
}

impl FormulaTokenizerMetadata {
    /// 从 metadata `character` 原始字符串解析。
    pub fn from_character_metadata(raw: &str) -> Result<Self> {
        let metadata: CharacterMetadata = serde_json::from_str(raw).map_err(|e| {
            RapidOcrError::Tokenizer(format!("`character` metadata is not valid JSON: {e}"))
        })?;

        let vocab = metadata
            .fast_tokenizer_file
            .get("model")
            .and_then(|model| model.get("vocab"))
            .ok_or_else(|| {
                RapidOcrError::Tokenizer(
                    "`character`.fast_tokenizer_file.model.vocab is missing".to_string(),
                )
            })?;
        let vocab_size = vocab.as_object().map(|o| o.len()).unwrap_or(0);
        if vocab_size == 0 {
            return Err(RapidOcrError::Tokenizer(
                "`character`.fast_tokenizer_file.model.vocab is empty".to_string(),
            ));
        }

        let special = |token: &str| -> Result<i64> {
            match vocab.get(token).and_then(|v| v.as_i64()) {
                Some(id) => Ok(id),
                None => Err(RapidOcrError::Tokenizer(format!(
                    "tokenizer vocab is missing special token `{token}`"
                ))),
            }
        };
        let bos_id = special(BOS_TOKEN)?;
        let pad_id = special(PAD_TOKEN)?;
        let eos_id = special(EOS_TOKEN)?;
        let unk_id = special(UNK_TOKEN)?;

        if (bos_id, pad_id, eos_id, unk_id) != (BOS_ID, PAD_ID, EOS_ID, UNK_ID) {
            return Err(RapidOcrError::Tokenizer(format!(
                "special token IDs mismatch: expected <s>={BOS_ID} <pad>={PAD_ID} </s>={EOS_ID} \
                 <unk>={UNK_ID}, got <s>={bos_id} <pad>={pad_id} </s>={eos_id} <unk>={unk_id}"
            )));
        }

        Ok(Self {
            vocab_size,
            bos_id,
            pad_id,
            eos_id,
            unk_id,
            fast_tokenizer_file: metadata.fast_tokenizer_file,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata_json(vocab_size: usize, bos: i64, pad: i64, eos: i64, unk: i64) -> String {
        let mut vocab = serde_json::Map::new();
        for i in 0..vocab_size {
            vocab.insert(format!("tok{i}"), serde_json::json!(i as i64));
        }
        vocab.insert(BOS_TOKEN.into(), serde_json::json!(bos));
        vocab.insert(PAD_TOKEN.into(), serde_json::json!(pad));
        vocab.insert(EOS_TOKEN.into(), serde_json::json!(eos));
        vocab.insert(UNK_TOKEN.into(), serde_json::json!(unk));
        serde_json::json!({
            "fast_tokenizer_file": { "model": { "type": "BPE", "vocab": vocab } },
            "tokenizer_config_file": { "model_max_length": 768 }
        })
        .to_string()
    }

    #[test]
    fn parses_and_validates_special_ids() {
        let meta =
            FormulaTokenizerMetadata::from_character_metadata(&metadata_json(50000, 0, 1, 2, 3))
                .expect("valid metadata");
        assert_eq!(meta.vocab_size, 50004); // 50000 + 4 inserted special tokens
        assert_eq!(
            (meta.bos_id, meta.pad_id, meta.eos_id, meta.unk_id),
            (0, 1, 2, 3)
        );
    }

    #[test]
    fn rejects_wrong_special_ids() {
        let err =
            FormulaTokenizerMetadata::from_character_metadata(&metadata_json(50000, 1, 0, 2, 3))
                .expect_err("must reject swapped bos/pad");
        assert!(matches!(err, RapidOcrError::Tokenizer(_)), "error: {err}");
    }

    #[test]
    fn rejects_broken_json() {
        let err = FormulaTokenizerMetadata::from_character_metadata("{ not json")
            .expect_err("must reject broken json");
        assert!(matches!(err, RapidOcrError::Tokenizer(_)), "error: {err}");
    }

    #[test]
    fn rejects_missing_vocab() {
        let raw = serde_json::json!({ "fast_tokenizer_file": { "model": {} } }).to_string();
        let err = FormulaTokenizerMetadata::from_character_metadata(&raw)
            .expect_err("must reject missing vocab");
        assert!(matches!(err, RapidOcrError::Tokenizer(_)), "error: {err}");
    }

    #[test]
    fn rejects_missing_special_token() {
        let raw = serde_json::json!({
            "fast_tokenizer_file": { "model": { "vocab": { "<s>": 0, "<pad>": 1 } } }
        })
        .to_string();
        let err = FormulaTokenizerMetadata::from_character_metadata(&raw)
            .expect_err("must reject missing special tokens");
        assert!(matches!(err, RapidOcrError::Tokenizer(_)), "error: {err}");
    }
}
