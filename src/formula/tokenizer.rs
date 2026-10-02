//! PP-FormulaNet_plus token 序列解码。
//!
//! 使用成熟 `tokenizers` crate 解析模型 metadata 中的 HF tokenizer JSON，
//! 不复制 tokenizer 实现。领域逻辑固定为：
//! `token_ids -> 第一个 EOS 截断 -> skip special decode -> LaTeX`。

use serde::{Deserialize, Serialize};
use tokenizers::Tokenizer as HfTokenizer;

use crate::{
    error::{RapidOcrError, Result},
    formula::tokenizer_metadata::{
        BOS_ID, BOS_TOKEN, EOS_ID, EOS_TOKEN, FormulaTokenizerMetadata, PAD_ID, PAD_TOKEN, UNK_ID,
        UNK_TOKEN,
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormulaDecode {
    pub latex: String,
    pub token_ids: Vec<i64>,
    pub eos_index: Option<usize>,
    pub truncated: bool,
}

pub struct FormulaTokenizer {
    tokenizer: HfTokenizer,
    eos_id: i64,
    vocab_size: usize,
}

impl std::fmt::Debug for FormulaTokenizer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FormulaTokenizer")
            .field("eos_id", &self.eos_id)
            .field("vocab_size", &self.vocab_size)
            .finish()
    }
}

impl FormulaTokenizer {
    pub fn from_metadata(metadata: &FormulaTokenizerMetadata) -> Result<Self> {
        let tokenizer_json =
            serde_json::to_vec(&metadata.fast_tokenizer_file).map_err(|error| {
                RapidOcrError::Tokenizer(format!(
                    "failed to serialize fast tokenizer JSON: {error}"
                ))
            })?;
        let tokenizer = HfTokenizer::from_bytes(tokenizer_json).map_err(|error| {
            RapidOcrError::Tokenizer(format!("invalid tokenizer JSON: {error}"))
        })?;

        let resolve = |token: &str| -> Result<i64> {
            tokenizer
                .token_to_id(token)
                .map(|id| id as i64)
                .ok_or_else(|| {
                    RapidOcrError::Tokenizer(format!(
                        "tokenizer vocab is missing special token `{token}`"
                    ))
                })
        };
        let bos_id = resolve(BOS_TOKEN)?;
        let pad_id = resolve(PAD_TOKEN)?;
        let eos_id = resolve(EOS_TOKEN)?;
        let unk_id = resolve(UNK_TOKEN)?;

        if (bos_id, pad_id, eos_id, unk_id) != (BOS_ID, PAD_ID, EOS_ID, UNK_ID) {
            return Err(RapidOcrError::Tokenizer(format!(
                "tokenizer special token IDs mismatch: expected <s>={BOS_ID} <pad>={PAD_ID} \
                 </s>={EOS_ID} <unk>={UNK_ID}, got <s>={bos_id} <pad>={pad_id} </s>={eos_id} \
                 <unk>={unk_id}"
            )));
        }
        if (
            metadata.bos_id,
            metadata.pad_id,
            metadata.eos_id,
            metadata.unk_id,
        ) != (bos_id, pad_id, eos_id, unk_id)
        {
            return Err(RapidOcrError::Tokenizer(format!(
                "metadata special token IDs are inconsistent with tokenizer JSON: \
                 metadata=({},{},{},{}), tokenizer=({bos_id},{pad_id},{eos_id},{unk_id})",
                metadata.bos_id, metadata.pad_id, metadata.eos_id, metadata.unk_id
            )));
        }

        let vocab_size = tokenizer.get_vocab_size(true);
        if vocab_size == 0 {
            return Err(RapidOcrError::Tokenizer(
                "tokenizer vocab is empty".to_string(),
            ));
        }

        Ok(Self {
            tokenizer,
            eos_id,
            vocab_size,
        })
    }

    pub fn decode_ids(&self, token_ids: &[i64]) -> Result<FormulaDecode> {
        for token_id in token_ids {
            if *token_id < 0 || *token_id as usize >= self.vocab_size {
                return Err(RapidOcrError::Tokenizer(format!(
                    "token id {token_id} is out of vocabulary (vocab_size={})",
                    self.vocab_size
                )));
            }
        }

        let eos_index = token_ids.iter().position(|id| *id == self.eos_id);
        let (decode_ids, truncated) = match eos_index {
            Some(index) => (&token_ids[..=index], false),
            None => (token_ids, true),
        };
        let decode_ids: Vec<u32> = decode_ids.iter().map(|id| *id as u32).collect();
        let latex = self.tokenizer.decode(&decode_ids, true).map_err(|error| {
            RapidOcrError::Tokenizer(format!("tokenizer decode failed: {error}"))
        })?;

        Ok(FormulaDecode {
            latex,
            token_ids: token_ids.to_vec(),
            eos_index,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct CaseFile {
        sequences: Vec<Case>,
    }

    #[derive(Debug, Deserialize)]
    struct Case {
        tokens: Vec<i64>,
        latex: String,
        eos_index: Option<usize>,
        truncated: bool,
    }

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/formula-tokenizer")
    }

    fn real_tokenizer() -> FormulaTokenizer {
        let fast = std::fs::read_to_string(fixture_dir().join("fast_tokenizer.json"))
            .expect("fast tokenizer fixture should exist");
        let raw = format!("{{\"fast_tokenizer_file\":{fast}}}");
        let metadata = FormulaTokenizerMetadata::from_character_metadata(&raw)
            .expect("real tokenizer metadata should parse");
        FormulaTokenizer::from_metadata(&metadata).expect("real tokenizer should build")
    }

    #[test]
    fn golden_sequences_match_python_tokenizers_reference() {
        let tokenizer = real_tokenizer();
        let cases: CaseFile = serde_json::from_str(
            &std::fs::read_to_string(fixture_dir().join("cases.json"))
                .expect("tokenizer cases should exist"),
        )
        .expect("cases should parse");
        assert!(cases.sequences.len() >= 20);

        for case in cases.sequences {
            let decoded = tokenizer
                .decode_ids(&case.tokens)
                .expect("golden sequence should decode");
            assert_eq!(decoded.latex, case.latex, "tokens={:?}", case.tokens);
            assert_eq!(decoded.token_ids, case.tokens);
            assert_eq!(
                decoded.eos_index, case.eos_index,
                "tokens={:?}",
                case.tokens
            );
            assert_eq!(
                decoded.truncated, case.truncated,
                "tokens={:?}",
                case.tokens
            );
        }
    }

    #[test]
    fn no_eos_is_marked_truncated() {
        let tokenizer = real_tokenizer();
        let decoded = tokenizer.decode_ids(&[0, 82, 1769]).expect("decode");
        assert!(decoded.truncated);
        assert_eq!(decoded.eos_index, None);
    }

    #[test]
    fn stale_metadata_special_ids_are_rejected() {
        let fast = std::fs::read_to_string(fixture_dir().join("fast_tokenizer.json")).unwrap();
        let raw = format!("{{\"fast_tokenizer_file\":{fast}}}");
        let mut metadata = FormulaTokenizerMetadata::from_character_metadata(&raw).unwrap();
        metadata.bos_id = 1;
        let error =
            FormulaTokenizer::from_metadata(&metadata).expect_err("stale metadata must fail");
        assert!(error.to_string().contains("inconsistent"), "error: {error}");
    }

    #[test]
    fn out_of_vocab_id_is_rejected() {
        let tokenizer = real_tokenizer();
        let error = tokenizer
            .decode_ids(&[0, 999_999, 2])
            .expect_err("out of vocab id must fail");
        assert!(
            error.to_string().contains("out of vocabulary"),
            "error: {error}"
        );
    }
}
