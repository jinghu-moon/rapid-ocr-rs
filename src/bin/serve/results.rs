//! 有界结果存储与有界序列化（§4.5、§4.6）。
//!
//! # 两个上限是**不同**的东西
//!
//! - **结果存储**（本模块的 [`ResultStore`]）：保留了多少份已完成的响应；
//!   它由 `JobStore` 的保留上限（数量 + 字节 + TTL）间接约束——每个终态任务最多有一份
//!   结果，任务被淘汰时结果一起被清除（[`ResultStore::retain_only`]）；
//! - **响应序列化**（[`serialize_bounded`]）：把 `OcrOutput` 写成 JSON 时**累计字节超限
//!   立即中止**，绝不"先拼出一个巨大 `String` 再判断长度"（§4.6 的硬要求）。
//!   超限本身是一个独立的 413 `result_too_large`，不是存储上限的表征。
//!
//! 序列化在 worker 里做一次（结果直接以字节保存），`/api/jobs/{id}/result` 只把字节
//! 原样写出，因此响应路径上不会出现第二份大分配。

use std::collections::HashMap;
use std::io::{self, Write};

use serde::Serialize;

use super::error::ErrorBody;

/// 一个任务的终态载荷。
#[derive(Debug, Clone)]
pub(super) enum Outcome {
    /// 已序列化的结果 JSON（已通过 [`serialize_bounded`] 的体积上限检查）。
    Succeeded(Vec<u8>),
    /// 失败：**保留原始的状态码与错误体**，这样 `/result` 能重放
    /// `422 unsupported_input` / `413 result_too_large` / `503 engine_unavailable`
    /// 这些真实原因，而不是把它们压成一句 `job_not_finished`。
    Failed(u16, ErrorBody),
}

impl Outcome {
    /// 该载荷占用的字节数（进 `JobStore` 的字节账本）。
    pub fn bytes(&self) -> u64 {
        match self {
            Self::Succeeded(bytes) => bytes.len() as u64,
            Self::Failed(_, body) => {
                (body.code.len() + body.message.len() + body.detail.to_string().len()) as u64
            }
        }
    }
}

/// 有界结果存储。
///
/// 上限由 `JobStore` 的保留上限给出（每个终态任务至多一份结果），因此正常情况下
/// [`Self::insert`] 不会拒绝；拒绝分支是**不变量被破坏**时的显式失败，而不是静默丢弃。
#[derive(Debug)]
pub(super) struct ResultStore {
    max_entries: usize,
    max_bytes: u64,
    bytes: u64,
    entries: HashMap<String, Outcome>,
}

impl ResultStore {
    pub fn new(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            max_entries,
            max_bytes,
            bytes: 0,
            entries: HashMap::new(),
        }
    }

    /// 登记一份终态载荷。返回 `false` 表示已满（调用方必须把它当成内部错误处理，
    /// 不允许静默覆盖或丢弃别的任务的结果）。
    pub fn insert(&mut self, id: &str, outcome: Outcome) -> bool {
        if self.entries.contains_key(id) {
            return false;
        }
        let size = outcome.bytes();
        if self.entries.len() >= self.max_entries
            || self.bytes.saturating_add(size) > self.max_bytes
        {
            return false;
        }
        self.bytes = self.bytes.saturating_add(size);
        self.entries.insert(id.to_string(), outcome);
        true
    }

    pub fn get(&self, id: &str) -> Option<&Outcome> {
        self.entries.get(id)
    }

    /// 只保留 `keep` 认可的任务（TTL 清理线程在 `JobStore::tick` 之后调用）。
    ///
    /// 返回被清除的条目数。
    pub fn retain_only(&mut self, keep: impl Fn(&str) -> bool) -> usize {
        let before = self.entries.len();
        let mut removed = 0_u64;
        self.entries.retain(|id, outcome| {
            if keep(id) {
                return true;
            }
            removed = removed.saturating_add(outcome.bytes());
            false
        });
        self.bytes = self.bytes.saturating_sub(removed);
        before - self.entries.len()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// 累计字节超限即中止的写入器（§4.6）。
struct BoundedWriter {
    buffer: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl BoundedWriter {
    fn new(limit: u64) -> Self {
        let reserve = usize::try_from(limit).unwrap_or(usize::MAX).min(64 * 1024);
        Self {
            buffer: Vec::with_capacity(reserve),
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
            exceeded: false,
        }
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buffer.len().saturating_add(data.len()) > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("serialized result exceeds the limit"));
        }
        self.buffer.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 序列化失败：要么超限（413 `result_too_large`），要么 serde 自身报错（内部错误）。
#[derive(Debug)]
pub(super) enum SerializeError {
    TooLarge,
    Internal(String),
}

/// 有界序列化：超限时**不会**分配超过 `limit` 的缓冲区。
pub(super) fn serialize_bounded<T: Serialize>(
    value: &T,
    limit: u64,
) -> Result<Vec<u8>, SerializeError> {
    let mut writer = BoundedWriter::new(limit);
    // 序列化器的可变借用必须在读取 `writer.buffer` 之前结束。
    let outcome = {
        let mut serializer = serde_json::Serializer::new(&mut writer);
        value.serialize(&mut serializer)
    };
    match outcome {
        Ok(()) => Ok(writer.buffer),
        Err(_) if writer.exceeded => Err(SerializeError::TooLarge),
        Err(error) => Err(SerializeError::Internal(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Outcome, ResultStore, SerializeError, serialize_bounded};

    #[test]
    fn a_result_over_the_limit_aborts_without_building_a_large_buffer() {
        let value = json!({ "text": "x".repeat(4096) });
        let error = serialize_bounded(&value, 512).expect_err("must abort");
        assert!(matches!(error, SerializeError::TooLarge), "{error:?}");
        // 恰好等于上限时通过（上限是"不得超过"，不是"必须小于"）。
        let small = json!({ "n": 1 });
        let exact = serialize_bounded(&small, 7).expect("fits exactly");
        assert_eq!(exact.len(), 7);
        assert!(serialize_bounded(&small, 6).is_err());
    }

    #[test]
    fn the_store_counts_bytes_and_refuses_to_overflow() {
        let mut store = ResultStore::new(2, 8);
        assert!(store.insert("a", Outcome::Succeeded(vec![0; 4])));
        assert!(store.insert("b", Outcome::Succeeded(vec![0; 4])));
        assert_eq!(store.len(), 2);
        assert_eq!(store.bytes(), 8);
        // 数量上限。
        assert!(!store.insert("c", Outcome::Succeeded(vec![0; 1])));
        // 字节上限（同一条目不能重复登记）。
        assert!(!store.insert("a", Outcome::Succeeded(vec![0; 1])));
        store.retain_only(|id| id == "a");
        assert_eq!(store.len(), 1);
        assert_eq!(store.bytes(), 4);
        assert!(store.get("b").is_none());
        assert!(store.insert("b", Outcome::Succeeded(vec![0; 4])));
    }

    #[test]
    fn failures_keep_their_status_and_code() {
        let mut store = ResultStore::new(4, 1024);
        let body = super::ErrorBody {
            code: "result_too_large",
            message: "too big".to_string(),
            detail: json!(null),
        };
        assert!(store.insert("a", Outcome::Failed(413, body.clone())));
        match store.get("a").expect("inserted") {
            Outcome::Failed(status, stored) => {
                assert_eq!(*status, 413);
                assert_eq!(stored.code, "result_too_large");
            }
            other => panic!("expected a failure outcome, got {other:?}"),
        }
        assert!(store.bytes() > 0);
    }
}
