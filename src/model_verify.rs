//! 文件身份键控的 SHA-256 校验缓存（**唯一**实现）。
//!
//! # 为什么需要一个缓存（根因）
//!
//! `ModelFileSpec::state_in`（`/api/models` 的逐文件状态）与两条公式加载路径
//! （`FormulaRecognizer::from_model_with_hash`、`FormulaDetector::from_model_with_hash`）
//! 问的是**同一个问题**："这个文件的内容是不是声明的那一份"。它们各自直接调
//! [`crate::model_store::sha256_file`]，于是：
//!
//! 1. 566 MB 的公式识别模型在**每次 `/api/models`**（页面每 8 s 轮询一次就绪状态）
//!    都被完整重读一遍——持续磁盘 I/O 与秒级延迟；
//! 2. 更重要的是，"报告的状态"与"加载时校验的文件"是两次独立的读盘：判定的不是
//!    同一份证据。
//!
//! 这里把这三次调用收口到**一个**进程级缓存上：键是文件的**身份**
//! （路径 + 体积 + mtime），值是上一次真正算出来的 SHA-256。同一份身份只算一次，
//! 身份变了（文件被替换、被截断、被改写）就重新计算。因此：
//!
//! - 冷验证（第一次见到某个身份）真的读盘并哈希，成本如实记账（[`VerificationStats`]）；
//! - 命中只花一次 `stat` + 一次内存比较，与文件大小无关；
//! - 每一次调用都能回答"**这一次**我到底算没算"（[`VerificationOutcome::computed`]），
//!   因此"命中不重新哈希"是可以被单次调用直接断言的事实，而不是靠计数器差值推断。
//!
//! # 为什么是进程级而不是"传一个缓存句柄"
//!
//! 调用点分布在三处、其中两处在库的深层加载路径里，而 [`crate::api::FormulaPolicy`]
//! 是进 `OcrRequest` 的**可序列化协议类型**：把一个缓存句柄塞进协议类型会让它无法
//! 序列化，也会让"要不要校验"变成请求方可选的行为。进程级缓存让"同一进程看到的同一个
//! 文件只有一种结论"成为一个结构性事实，而不是各调用点各自记得传参。
//!
//! # 如实的盲区（**必须与缓存一起被理解**）
//!
//! 身份由 `(size, mtime)` 近似。一个**体积不变、mtime 也不变**的内容替换
//! （例如用 `SetFileTime` 把时间戳写回原值，或在同一时间戳粒度内原地改写）不会被
//! 识别为"变了"，缓存因此会返回旧摘要。缓存**只**用来省掉重复的完整读取；
//! 它不改变任何一个调用点的判定语义，也不声称能发现这种替换。
//! 另外 `mtime` 不可得（文件系统不提供）时身份里是 `None`，这类文件之间的
//! 同体积替换同样落在盲区里。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime};

use crate::error::{RapidOcrError, Result};
use crate::model_store::sha256_file;

/// 一个文件的**身份**：路径 + 体积 + mtime。
///
/// 相等即"缓存里的摘要仍然描述这个文件"；任何一项变化都会让身份不等，
/// 于是下一次校验重新读盘。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

impl FileIdentity {
    /// 读取当前身份（一次 `metadata` 调用，不读文件内容）。
    pub fn of(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            size: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn modified(&self) -> Option<SystemTime> {
        self.modified
    }

    /// 日志/错误文案用的简述（不含绝对路径）。
    pub fn describe(&self) -> String {
        format!(
            "size={} B, mtime={}",
            self.size,
            match self.modified {
                Some(value) => format!("{value:?}"),
                None => "<unavailable>".to_string(),
            }
        )
    }
}

/// 一次校验的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationOutcome {
    /// 文件的 SHA-256（小写十六进制）。
    pub sha256: String,
    /// `true` = **这一次**真的读了盘并计算；`false` = 由身份键控的缓存直接回答。
    ///
    /// 这是"命中不重新哈希"的唯一证据来源：它是**单次调用**的属性，不受同一进程里
    /// 其它线程/其它测试的并发影响（进程级累计计数器做不到这一点）。
    pub computed: bool,
    /// 本次校验针对的文件身份（路径 + 体积 + mtime）。
    pub identity: FileIdentity,
}

/// 一条缓存记录。
#[derive(Debug, Clone)]
struct CachedDigest {
    size: u64,
    modified: Option<SystemTime>,
    sha256: String,
}

/// 校验缓存的成本账（`/api/models` 的 `verification` 块与测试的计数器）。
///
/// `cache_hits` 与 `cold_verifications` 是**累计**值（进程启动以来）：
/// 它们回答"这台机器到目前为止为校验花了多少次完整读盘"，
/// 而"**这一次**算没算"由 [`VerificationOutcome::computed`] 回答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerificationStats {
    /// 真正读盘并计算 SHA-256 的次数（累计）。
    pub cold_verifications: u64,
    /// 由缓存直接回答的次数（累计；只做了一次 `stat`）。
    pub cache_hits: u64,
    /// 最近一次冷验证的耗时（微秒）；还没有冷验证时为 `None`。
    pub last_cold_micros: Option<u64>,
    /// 最近一次冷验证读取的字节数。
    pub last_cold_bytes: u64,
    /// 缓存里当前的记录数。
    pub entries: usize,
}

impl VerificationStats {
    /// 最近一次冷验证的毫秒数（报告用；保留小数）。
    pub fn last_cold_ms(&self) -> Option<f64> {
        self.last_cold_micros.map(|micros| micros as f64 / 1000.0)
    }
}

fn cache() -> &'static Mutex<HashMap<PathBuf, CachedDigest>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedDigest>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

static COLD_VERIFICATIONS: AtomicU64 = AtomicU64::new(0);
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static LAST_COLD_MICROS: AtomicU64 = AtomicU64::new(0);
static LAST_COLD_BYTES: AtomicU64 = AtomicU64::new(0);
static HAVE_COLD: AtomicU64 = AtomicU64::new(0);

/// 文件的 SHA-256；**同一个身份只计算一次**。
///
/// 语义与 [`sha256_file`] 完全一致（同一个摘要、同一套 IO 错误），区别只是重复询问
/// 同一个身份时复用上一次的结果，并回答"这一次算没算"。文件不存在/读不出来时返回错误，
/// **不**缓存失败。
pub fn verify_file(path: &Path) -> Result<VerificationOutcome> {
    let identity = FileIdentity::of(path)?;
    if let Some(sha256) = lookup(&identity) {
        CACHE_HITS.fetch_add(1, Ordering::Relaxed);
        return Ok(VerificationOutcome {
            sha256,
            computed: false,
            identity,
        });
    }

    let started = Instant::now();
    let sha256 = sha256_file(path)?;
    let elapsed = started.elapsed();

    store(&identity, &sha256);
    COLD_VERIFICATIONS.fetch_add(1, Ordering::Relaxed);
    LAST_COLD_MICROS.store(
        u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    LAST_COLD_BYTES.store(identity.size, Ordering::Relaxed);
    HAVE_COLD.store(1, Ordering::Relaxed);
    Ok(VerificationOutcome {
        sha256,
        computed: true,
        identity,
    })
}

/// [`verify_file`] 的摘要部分（最常见的调用形状）。
pub fn sha256_file_cached(path: &Path) -> Result<String> {
    Ok(verify_file(path)?.sha256)
}

/// 期望摘要存在时校验它，返回实际摘要（供调用方继续使用）。
///
/// 不匹配是**可定位**错误 [`RapidOcrError::HashMismatch`]（路径 + 期望 + 实际），
/// 绝不静默加载。
pub fn verify_sha256(path: &Path, expected: Option<&str>) -> Result<Option<String>> {
    let Some(expected) = expected else {
        return Ok(None);
    };
    let outcome = verify_file(path)?;
    if !outcome.sha256.eq_ignore_ascii_case(expected) {
        return Err(RapidOcrError::HashMismatch {
            path: path.to_path_buf(),
            expected: expected.to_string(),
            actual: outcome.sha256,
        });
    }
    Ok(Some(outcome.sha256))
}

/// 当前成本账（累计）。
pub fn verification_stats() -> VerificationStats {
    let entries = cache().lock().map(|guard| guard.len()).unwrap_or_default();
    VerificationStats {
        cold_verifications: COLD_VERIFICATIONS.load(Ordering::Relaxed),
        cache_hits: CACHE_HITS.load(Ordering::Relaxed),
        last_cold_micros: (HAVE_COLD.load(Ordering::Relaxed) == 1)
            .then(|| LAST_COLD_MICROS.load(Ordering::Relaxed)),
        last_cold_bytes: LAST_COLD_BYTES.load(Ordering::Relaxed),
        entries,
    }
}

/// 清空缓存与累计账（诊断与测试用）。
///
/// 生产路径没有理由调用它：清空只会让下一次校验重新读盘，不会改变任何结论。
pub fn clear_verification_cache() {
    if let Ok(mut guard) = cache().lock() {
        guard.clear();
    }
    COLD_VERIFICATIONS.store(0, Ordering::Relaxed);
    CACHE_HITS.store(0, Ordering::Relaxed);
    LAST_COLD_MICROS.store(0, Ordering::Relaxed);
    LAST_COLD_BYTES.store(0, Ordering::Relaxed);
    HAVE_COLD.store(0, Ordering::Relaxed);
}

fn lookup(identity: &FileIdentity) -> Option<String> {
    let guard = cache().lock().ok()?;
    let entry = guard.get(&identity.path)?;
    if entry.size == identity.size && entry.modified == identity.modified {
        return Some(entry.sha256.clone());
    }
    None
}

/// 写入缓存（哈希在锁外完成：一个 566 MB 的冷验证不允许挡住其它文件的查询）。
fn store(identity: &FileIdentity, sha256: &str) {
    let Ok(mut guard) = cache().lock() else {
        return;
    };
    guard.insert(
        identity.path.clone(),
        CachedDigest {
            size: identity.size,
            modified: identity.modified,
            sha256: sha256.to_string(),
        },
    );
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::{
        FileIdentity, clear_verification_cache, sha256_file_cached, verification_stats,
        verify_file, verify_sha256,
    };
    use crate::error::RapidOcrError;
    use crate::model_store::sha256_file;
    use crate::test_support::TempDir;

    /// "hello" 的 SHA-256（与 `model_store`/`model_set` 的既有测试同值）。
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    /// 第一次真的算，之后**同一个身份**只从缓存回答——证据是**单次调用**的
    /// `computed` 标志（进程级累计计数器会被并行测试干扰，因此不作为判据）。
    #[test]
    fn the_cached_digest_equals_the_uncached_one_and_is_only_computed_once() {
        let dir = TempDir::new("verify-once");
        dir.write("model.onnx", b"hello");
        let path = dir.path().join("model.onnx");

        let first = verify_file(&path).expect("hash");
        assert_eq!(first.sha256, HELLO_SHA256);
        assert_eq!(
            first.sha256,
            sha256_file(&path).expect("the uncached digest")
        );
        assert!(first.computed, "the first ask must hash");
        assert_eq!(first.identity.size(), 5);

        for _ in 0..5 {
            let again = verify_file(&path).expect("hash");
            assert_eq!(again.sha256, HELLO_SHA256);
            assert!(
                !again.computed,
                "a cache hit must not re-hash (the per-call flag is the instrumentation)"
            );
            assert_eq!(again.identity, first.identity);
        }
        assert_eq!(sha256_file_cached(&path).expect("hash"), HELLO_SHA256);
    }

    /// 体积变化（内容替换的常见形态）→ 身份不等 → 重新哈希。
    #[test]
    fn a_size_change_forces_a_reverification() {
        let dir = TempDir::new("verify-size");
        dir.write("model.onnx", b"hello");
        let path = dir.path().join("model.onnx");
        assert!(verify_file(&path).expect("hash").computed);

        std::fs::write(&path, b"hello world").expect("replace with a longer body");
        let replaced = verify_file(&path).expect("hash");
        assert_eq!(replaced.sha256, sha256_file(&path).expect("uncached"));
        assert_ne!(replaced.sha256, HELLO_SHA256);
        assert!(replaced.computed, "a new identity must re-hash");
    }

    /// **同体积**但 mtime 变了 → 身份不等 → 重新哈希。
    /// （没有这一条，`(size, mtime)` 里的 mtime 就是装饰品。）
    #[test]
    fn a_mtime_change_forces_a_reverification() {
        let dir = TempDir::new("verify-mtime");
        dir.write("model.onnx", b"hello");
        let path = dir.path().join("model.onnx");
        let before = FileIdentity::of(&path).expect("identity");
        assert!(verify_file(&path).expect("hash").computed);

        // 同一个体积、不同内容：只有 mtime 会变。
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open for rewrite");
        file.write_all(b"HELLO").expect("rewrite the same length");
        file.set_modified(before.modified().expect("mtime") + std::time::Duration::from_secs(5))
            .expect("bump mtime");
        drop(file);

        let after = FileIdentity::of(&path).expect("identity");
        assert_eq!(after.size(), before.size(), "the size must be unchanged");
        assert_ne!(after, before, "the identity must differ by mtime");
        let digest = verify_file(&path).expect("hash");
        assert_eq!(digest.sha256, sha256_file(&path).expect("uncached"));
        assert!(digest.computed, "a new identity must re-hash");
    }

    #[test]
    fn a_mismatch_is_a_locating_error_and_a_match_passes_through() {
        let dir = TempDir::new("verify-mismatch");
        dir.write("model.onnx", b"hello");
        let path = dir.path().join("model.onnx");

        verify_sha256(&path, None).expect("no expectation means no verification");
        assert_eq!(
            verify_sha256(&path, Some(HELLO_SHA256)).expect("match"),
            Some(HELLO_SHA256.to_string())
        );
        let error = verify_sha256(&path, Some("deadbeef")).expect_err("a mismatch must fail");
        match error {
            RapidOcrError::HashMismatch {
                path: reported,
                expected,
                actual,
            } => {
                assert_eq!(reported, path);
                assert_eq!(expected, "deadbeef");
                assert_eq!(actual, HELLO_SHA256);
            }
            other => panic!("expected HashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_file_is_an_error_and_is_not_cached() {
        let dir = TempDir::new("verify-missing");
        let path = dir.path().join("not-here.onnx");
        assert!(verify_file(&path).is_err());
        assert!(sha256_file_cached(&path).is_err());
        // 失败不进缓存：同一个路径被创建出来之后必须真的读它。
        dir.write("not-here.onnx", b"hello");
        let outcome = verify_file(&path).expect("hash after the file appears");
        assert_eq!(outcome.sha256, HELLO_SHA256);
        assert!(outcome.computed);
    }

    /// `clear_verification_cache` 只影响"要不要重新读盘"，不影响任何结论。
    #[test]
    fn clearing_the_cache_only_costs_another_read() {
        let dir = TempDir::new("verify-clear");
        dir.write("model.onnx", b"hello");
        let path = dir.path().join("model.onnx");
        assert!(verify_file(&path).expect("hash").computed);
        clear_verification_cache();
        let after = verify_file(&path).expect("hash");
        assert_eq!(after.sha256, HELLO_SHA256);
        assert!(after.computed, "a cleared cache must re-read");
        let stats = verification_stats();
        assert!(stats.last_cold_ms().is_some());
        assert_eq!(stats.last_cold_bytes, 5);
    }
}
