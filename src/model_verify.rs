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
//! （路径 + 体积 + mtime + 首尾各 64 KiB 的局部摘要），值是上一次真正算出来的 SHA-256。
//! 同一份身份只算一次，身份变了（文件被替换、被截断、被改写）就重新计算。因此：
//!
//! - 冷验证（第一次见到某个身份）真的读盘并哈希，成本如实记账（[`VerificationStats`]）；
//! - 命中只花一次 `stat` + 一次 128 KiB 局部读 + 一次内存比较，与文件大小无关；
//! - 每一次调用都能回答"**这一次**我到底算没算"（[`VerificationOutcome::computed`]），
//!   以及"为什么算"（[`VerificationOutcome::cause`]）；
//! - 需要"忽略缓存、现在就读盘"时用 [`force_verify_file`]（`--reverify-models` 与
//!   `POST /api/models/reverify` 的语义），它不留任何"其实还是命中"的余地。
//!
//! # 为什么是进程级而不是"传一个缓存句柄"
//!
//! 调用点分布在三处、其中两处在库的深层加载路径里，而 [`crate::api::FormulaPolicy`]
//! 是进 `OcrRequest` 的**可序列化协议类型**：把一个缓存句柄塞进协议类型会让它无法
//! 序列化，也会让"要不要校验"变成请求方可选的行为。进程级缓存让"同一进程看到的同一个
//! 文件只有一种结论"成为一个结构性事实，而不是各调用点各自记得传参。
//!
//! # 身份的第二半：首尾各 64 KiB 的局部摘要（B）
//!
//! `(size, mtime)` 单独作为身份有一个实测过代价的盲区：**同体积、同 mtime** 的内容替换
//! （`SetFileTime` 把时间戳写回原值，或在同一时间戳粒度内原地改写）不会被识别为"变了"，
//! 缓存会继续返回旧摘要。加一次 **128 KiB** 的局部读（首 64 KiB + 尾 64 KiB，各算一次
//! SHA-256）把窗口收窄到：
//!
//! > **同体积、同 mtime，且首尾各 64 KiB 逐字节相同**的替换才不被发现。
//!
//! 这是**启发式收窄，不是安全边界**：能写这个文件的攻击者同样能把首尾保持原样
//! （把改动放在中间即可）。它降低的是"误把改过的文件当成没改"的概率，不是"防住能写文件的人"。
//!
//! ## 小于 128 KiB 的文件（首尾重叠，规则明确）
//!
//! 首块 = 前 `min(size, 64 KiB)` 字节，尾块 = 后 `min(size, 64 KiB)` 字节。因此：
//!
//! - `size <= 128 KiB` 时两个切片**重叠甚至重合**，局部摘要实际覆盖了**整个文件内容**
//!   （此时"首尾相同"就等于"内容相同"，盲区对这类文件不存在）；
//! - `size == 0` 时两块都退化为 64 字节的零哈希域分隔编码，规则仍然唯一确定。
//!
//! ## 成本
//!
//! 每个文件每次检查多读 128 KiB：对本项目的模型集（5 个文件、其中 566 MB 一个）是
//! 0.6 MiB 量级的顺序读，命中路径仍然是毫秒级；**完整重哈希只在局部摘要或
//! stat 身份变化时发生**，且这一次重哈希的原因是可见的（[`ReverifyCause`]）。
//!
//! # 如实的盲区（**必须与缓存一起被理解**）
//!
//! 局部摘要把盲区收窄成"同体积 + 同 mtime + 首尾 64 KiB 相同"，但它**不是**安全边界：
//! 能写文件的人可以保留首尾、只改中间；`mtime` 不可得（文件系统不提供）时身份里是
//! `None`，这类文件之间的同体积替换同样落在盲区里。
//! 缓存**只**用来省掉重复的完整读取；它不改变任何一个调用点的判定语义。
//! 需要**确定性地**排除缓存影响时用 [`force_verify_file`]。

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::error::{RapidOcrError, Result};
use crate::model_store::sha256_file;

/// 局部摘要覆盖的文件头/尾长度（各 64 KiB；合起来 128 KiB）。
///
/// 它是"每次检查多读多少"的唯一旋钮：调大收窄盲区、调大也提高轮询成本。
/// 128 KiB 相对于本项目 5 个模型文件（含 566 MB 那个）是可以忽略的顺序读。
pub const PARTIAL_DIGEST_WINDOW_BYTES: u64 = 64 * 1024;

/// 局部摘要：文件的**首 64 KiB** 与**尾 64 KiB**各一次 SHA-256。
///
/// 与 [`FileIdentity`] 一起构成"命中还需要内容对上"的第二半判据。它**不是**内容摘要：
/// 中段没有任何一个字节进入这里（见模块文档的盲区一节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialDigest {
    size: u64,
    head: [u8; 32],
    tail: [u8; 32],
}

impl PartialDigest {
    /// 一次 `open` + 两次定位读（首 64 KiB、尾 64 KiB）。
    ///
    /// `size` 由调用方的 [`FileIdentity::of`] 提供，因此这里**不**再 `stat` 一次：
    /// 身份与局部摘要必须描述同一次观察到的文件。
    pub fn compute(path: &Path, size: u64) -> Result<Self> {
        let mut file = std::fs::File::open(path)?;
        let head = read_window(&mut file, 0, size)?;
        let tail_start = size.saturating_sub(PARTIAL_DIGEST_WINDOW_BYTES);
        let tail = read_window(&mut file, tail_start, size)?;
        Ok(Self {
            size,
            head: Sha256::digest(&head).into(),
            tail: Sha256::digest(&tail).into(),
        })
    }

    /// 文件体积（参与编码：两个不同体积、相同首尾内容各自有不同摘要）。
    pub fn size(&self) -> u64 {
        self.size
    }

    fn head(&self) -> &[u8; 32] {
        &self.head
    }

    fn tail(&self) -> &[u8; 32] {
        &self.tail
    }

    /// 日志/错误文案用的简述（体积 + 首尾摘要的前 16 位十六进制）。
    pub fn describe(&self) -> String {
        format!(
            "size={} B, head={}, tail={}",
            self.size,
            hex16(self.head()),
            hex16(self.tail())
        )
    }
}

/// 从 `offset` 起最多读 `PARTIAL_DIGEST_WINDOW_BYTES` 字节，且**不越过** `size`。
///
/// 显式限制在 `size` 内（而不是"读到 EOF"）：`FileIdentity::of` 的 `size` 是这次观察
/// 到的体积，局部摘要必须描述同一个快照；文件在两次读之间被别的进程追加时，
/// 尾窗口不会因此带上新字节（那会让摘要描述一个从未存在过的文件）。
fn read_window(file: &mut std::fs::File, offset: u64, size: u64) -> Result<Vec<u8>> {
    if size == 0 || offset >= size {
        return Ok(Vec::new());
    }
    let wanted = (size - offset).min(PARTIAL_DIGEST_WINDOW_BYTES);
    file.seek(SeekFrom::Start(offset))?;
    let mut window = Vec::with_capacity(wanted as usize);
    file.take(wanted).read_to_end(&mut window)?;
    Ok(window)
}

/// 一个文件的**身份**：路径 + 体积 + mtime + 首尾各 64 KiB 的局部摘要。
///
/// 相等即"缓存里的摘要仍然描述这个文件"；任何一项变化都会让身份不等，
/// 于是下一次校验重新读盘。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
    /// 首尾各 64 KiB 的局部摘要。`size <= 128 KiB` 时它覆盖**整个内容**（见模块文档）。
    partial: PartialDigest,
}

impl FileIdentity {
    /// 读取当前身份（一次 `metadata` + 128 KiB 局部读，不做完整哈希）。
    pub fn of(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)?;
        let size = metadata.len();
        Ok(Self {
            path: path.to_path_buf(),
            size,
            modified: metadata.modified().ok(),
            partial: PartialDigest::compute(path, size)?,
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

    /// 首尾 64 KiB 的局部摘要（见 [`PartialDigest`]）。
    pub fn partial(&self) -> &PartialDigest {
        &self.partial
    }

    /// 日志/错误文案用的简述（不含绝对路径）。
    pub fn describe(&self) -> String {
        format!(
            "size={} B, mtime={}, {}",
            self.size,
            match self.modified {
                Some(value) => format!("{value:?}"),
                None => "<unavailable>".to_string(),
            },
            self.partial.describe()
        )
    }
}

/// **这一次**为什么重新计算了完整摘要（或为什么命中）。
///
/// 它是"为什么又读了一遍 566 MB"的唯一证据来源：运维从启动日志与
/// `POST /api/models/reverify` 的响应里直接读到它，而不是靠计数器差值猜。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReverifyCause {
    /// 缓存里没有这个身份（第一次见到这个文件，或路径此前没被校验过）。
    FirstSight,
    /// 体积或 mtime 变了：文件被替换/改写，身份不再是同一个。
    StatChanged,
    /// **stat 身份完全相同**（体积 + mtime），但首尾 64 KiB 的局部摘要变了。
    ///
    /// 这正是"同体积、同 mtime 的内容替换"被局部摘要抓住的那一半：缓存必须作废并重算。
    ContentChanged,
    /// 命中：身份与局部摘要都与上一次真正验过的那一份相同，没有重新哈希。
    CacheHit,
}

impl ReverifyCause {
    /// 稳定的机器可读取值（进 `/api/models/reverify` 的响应与日志）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FirstSight => "first_sight",
            Self::StatChanged => "stat_changed",
            Self::ContentChanged => "content_changed",
            Self::CacheHit => "cache_hit",
        }
    }

    /// 这一次是否真的读了整个文件并算了摘要。
    pub const fn computed(self) -> bool {
        !matches!(self, Self::CacheHit)
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
    /// 本次校验针对的文件身份（路径 + 体积 + mtime + 首尾 64 KiB 局部摘要）。
    pub identity: FileIdentity,
    /// 为什么会这样（`computed = cause.computed()`，两者由同一次判定同时给出）。
    pub cause: ReverifyCause,
}

/// 一条缓存记录。
#[derive(Debug, Clone)]
struct CachedDigest {
    size: u64,
    modified: Option<SystemTime>,
    /// 算这份摘要时文件的局部摘要：stat 相同但局部摘要不同 = 内容变了。
    partial: PartialDigest,
    sha256: String,
}

/// 校验缓存的成本账（`/api/models` 的 `verification` 块与测试的计数器）。
///
/// `cache_hits` / `cold_verifications` / `partial_mismatches` / `partial_reads` 是**累计**值
/// （进程启动以来）：它们回答"这台机器到目前为止为校验花了多少次完整读盘、多少次局部读"。
/// 而"**这一次**算没算、为什么算"由 [`VerificationOutcome::computed`] /
/// [`VerificationOutcome::cause`] 回答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerificationStats {
    /// 真正读盘并计算完整 SHA-256 的次数（累计）。
    pub cold_verifications: u64,
    /// 由缓存直接回答的次数（累计；做了一次 `stat` + 一次 128 KiB 局部读）。
    pub cache_hits: u64,
    /// stat 身份相同、局部摘要不同的次数（累计）——"内容变了"被抓住的次数。
    pub partial_mismatches: u64,
    /// 局部摘要（首尾各 64 KiB）的读取次数（累计；每次校验恰好一次）。
    pub partial_reads: u64,
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
static PARTIAL_MISMATCHES: AtomicU64 = AtomicU64::new(0);
static PARTIAL_READS: AtomicU64 = AtomicU64::new(0);
static LAST_COLD_MICROS: AtomicU64 = AtomicU64::new(0);
static LAST_COLD_BYTES: AtomicU64 = AtomicU64::new(0);
static HAVE_COLD: AtomicU64 = AtomicU64::new(0);

/// 文件的 SHA-256；**同一个身份只计算一次**。
///
/// 语义与 [`sha256_file`] 完全一致（同一个摘要、同一套 IO 错误），区别只是重复询问
/// 同一个身份时复用上一次的结果，并回答"这一次算没算、为什么算"。文件不存在/读不出来时
/// 返回错误，**不**缓存失败。
///
/// 命中需要**两个**条件同时成立：stat 身份（体积 + mtime）相同 **且** 首尾各 64 KiB 的
/// 局部摘要相同。stat 相同而局部摘要不同时会重新哈希，并如实留下
/// [`ReverifyCause::ContentChanged`]（见 [`VerificationStats::partial_mismatches`]）。
pub fn verify_file(path: &Path) -> Result<VerificationOutcome> {
    let identity = FileIdentity::of(path)?;
    return_with_cache(path, identity, false)
}

/// **冷验证**：忽略缓存，现在就读盘重算摘要（`--reverify-models` 与
/// `POST /api/models/reverify` 的语义）。
///
/// 与 [`verify_file`] 的区别只有一条：它**不**查缓存。命中的记录会被这次的新结论覆盖，
/// 因此它同时是"清掉这条记录的陈旧结论"的动作；没有任何调用点可以因此绕过哈希。
pub fn force_verify_file(path: &Path) -> Result<VerificationOutcome> {
    let identity = FileIdentity::of(path)?;
    return_with_cache(path, identity, true)
}

/// [`verify_file`] / [`force_verify_file`] 的唯一实现：
/// `force = false` 时先查缓存，`force = true` 时无条件重算。
fn return_with_cache(
    path: &Path,
    identity: FileIdentity,
    force: bool,
) -> Result<VerificationOutcome> {
    PARTIAL_READS.fetch_add(1, Ordering::Relaxed);
    if !force {
        match lookup(&identity) {
            Lookup::Hit(sha256) => {
                CACHE_HITS.fetch_add(1, Ordering::Relaxed);
                refresh_stat_identity(&identity);
                return Ok(VerificationOutcome {
                    sha256,
                    computed: false,
                    identity,
                    cause: ReverifyCause::CacheHit,
                });
            }
            Lookup::Miss(cause) => {
                if cause == ReverifyCause::ContentChanged {
                    PARTIAL_MISMATCHES.fetch_add(1, Ordering::Relaxed);
                }
                return hashed(path, identity, cause);
            }
        }
    }
    hashed(path, identity, ReverifyCause::FirstSight)
}

/// 真正读盘并记录成本账（冷验证的**唯一**出口）。
fn hashed(
    path: &Path,
    identity: FileIdentity,
    cause: ReverifyCause,
) -> Result<VerificationOutcome> {
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
        cause,
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
        partial_mismatches: PARTIAL_MISMATCHES.load(Ordering::Relaxed),
        partial_reads: PARTIAL_READS.load(Ordering::Relaxed),
        last_cold_micros: (HAVE_COLD.load(Ordering::Relaxed) == 1)
            .then(|| LAST_COLD_MICROS.load(Ordering::Relaxed)),
        last_cold_bytes: LAST_COLD_BYTES.load(Ordering::Relaxed),
        entries,
    }
}

/// 清空缓存与累计账（诊断与测试用）。
///
/// 生产路径里它的语义是"下一次校验必须重新读盘"（`POST /api/models/reverify` 的第一步）；
/// 它本身不改变任何结论，只改变"要不要重新读盘"。
pub fn clear_verification_cache() {
    if let Ok(mut guard) = cache().lock() {
        guard.clear();
    }
    COLD_VERIFICATIONS.store(0, Ordering::Relaxed);
    CACHE_HITS.store(0, Ordering::Relaxed);
    PARTIAL_MISMATCHES.store(0, Ordering::Relaxed);
    PARTIAL_READS.store(0, Ordering::Relaxed);
    LAST_COLD_MICROS.store(0, Ordering::Relaxed);
    LAST_COLD_BYTES.store(0, Ordering::Relaxed);
    HAVE_COLD.store(0, Ordering::Relaxed);
}

/// 查缓存的结论：命中给摘要，未命中给**原因**（`FirstSight` / `StatChanged` /
/// `ContentChanged`）。
enum Lookup {
    Hit(String),
    Miss(ReverifyCause),
}

fn lookup(identity: &FileIdentity) -> Lookup {
    let Ok(guard) = cache().lock() else {
        return Lookup::Miss(ReverifyCause::FirstSight);
    };
    let Some(entry) = guard.get(&identity.path) else {
        return Lookup::Miss(ReverifyCause::FirstSight);
    };
    if entry.size != identity.size {
        // 体积变了：文件被整体换掉（旧摘要描述的是另一个长度的文件）。
        return Lookup::Miss(ReverifyCause::StatChanged);
    }
    if entry.partial != identity.partial {
        // **stat 身份相同而首尾 64 KiB 变了**：内容换了。这一条就是局部摘要的全部价值，
        // 也是"缓存不会拿旧摘要回答一个改过的文件"的证据（见模块文档的盲区一节）。
        // `mtime` 只是元数据（复制文件、`SetFileTime` 都能改它），因此它单独变化**不**算
        // 内容变化——那种情况是缓存命中，但下面会如实升级本地那份 stat 身份。
        return Lookup::Miss(ReverifyCause::ContentChanged);
    }
    Lookup::Hit(entry.sha256.clone())
}

/// 命中时把本地那份 stat 身份对齐到当前观察值。
///
/// 存在的理由：命中判据是 `(size, 首尾摘要)`，而 `mtime` 只是元数据。文件被**原样**
/// 复制（体积与内容都不变、mtime 变新）时命中是正确的结论，但如果不同步，下一次
/// stat 身份变化就会得到 [`ReverifyCause::StatChanged`] 这个**错误的原因**
/// （内容一个字节都没变）。这里用一次加锁把身份对齐，代价与一次比较同量级。
fn refresh_stat_identity(identity: &FileIdentity) {
    let Ok(mut guard) = cache().lock() else {
        return;
    };
    if let Some(entry) = guard.get_mut(&identity.path) {
        entry.size = identity.size;
        entry.modified = identity.modified;
    }
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
            partial: identity.partial.clone(),
            sha256: sha256.to_string(),
        },
    );
}

/// 摘要前 8 字节的十六进制（日志用；完整摘要太长）。
fn hex16(bytes: &[u8; 32]) -> String {
    bytes[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 测试专用的**观察**入口：直接问缓存"现在会命中吗、为什么"。
///
/// `verify_file` 的返回值已经带了 `cause`，但"不调用校验函数也能问出原因"这一点让
/// "`mtime` 变了但内容没变仍然命中（且不重新哈希）"这类断言不必依赖计数器
/// （进程级计数器会被同一进程里其它并行测试的校验干扰，见 `tests::serial` 的说明）。
#[cfg(test)]
fn lookup_for_test(path: &Path) -> std::result::Result<ReverifyCause, RapidOcrError> {
    let identity = FileIdentity::of(path)?;
    Ok(match lookup(&identity) {
        Lookup::Hit(_) => ReverifyCause::CacheHit,
        Lookup::Miss(cause) => cause,
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};
    use std::path::Path;

    use super::{
        FileIdentity, PARTIAL_DIGEST_WINDOW_BYTES, PartialDigest, ReverifyCause,
        clear_verification_cache, force_verify_file, lookup_for_test, sha256_file_cached,
        verification_stats, verify_file, verify_sha256,
    };
    use crate::error::RapidOcrError;
    use crate::model_store::sha256_file;
    use crate::test_support::TempDir;

    /// "hello" 的 SHA-256（与 `model_store`/`model_set` 的既有测试同值）。
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    /// 这组用例共享**库级的单一缓存与累计账**（生产路径刻意如此：同一进程看到的同一个
    /// 文件只有一种结论），而 `cargo test` 默认并行跑测试。因此每个用例在自己的进程锁里
    /// 清一次账，让"这一轮算了几个摘要"这类断言不会被别的用例的校验计数污染。
    ///
    /// **只能锁一个**：`std::sync::Mutex` 不可重入，被锁住的用例里再调一次就自锁。
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        clear_verification_cache();
        guard
    }

    /// 一个内容由 `(位置, 体积)` 唯一决定的字节串：任何位置被改写都会改变完整摘要。
    fn pattern(len: usize, tag: u8) -> Vec<u8> {
        (0..len)
            .map(|index| index.wrapping_mul(7).wrapping_add(tag as usize) as u8)
            .collect()
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).expect("write the fixture");
    }

    /// 改写 `[offset, offset + replacement.len())` 并把 mtime **写回原值**，
    /// 于是身份里的 `(size, mtime)` 完全不变——这正是局部摘要存在的理由。
    fn rewrite_same_identity(
        path: &Path,
        offset: usize,
        replacement: &[u8],
    ) -> std::time::SystemTime {
        let before = std::fs::metadata(path)
            .expect("metadata")
            .modified()
            .expect("mtime");
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("open for rewrite");
            file.seek(SeekFrom::Start(offset as u64))
                .expect("seek to the edit");
            file.write_all(replacement).expect("rewrite in place");
            file.flush().expect("flush");
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open to restore the timestamp");
        file.set_modified(before).expect("write the old mtime back");
        drop(file);
        std::fs::metadata(path)
            .expect("metadata")
            .modified()
            .expect("mtime")
    }

    /// 第一次真的算，之后**同一个身份**只从缓存回答——证据是**单次调用**的
    /// `computed` 标志（进程级累计计数器会被并行测试干扰，因此不作为判据）。
    #[test]
    fn the_cached_digest_equals_the_uncached_one_and_is_only_computed_once() {
        let _serial = serial();
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
        assert_eq!(first.cause, ReverifyCause::FirstSight);
        assert_eq!(first.identity.size(), 5);
        // 小文件：首尾窗口重叠，局部摘要覆盖整个内容。
        assert_eq!(first.identity.partial().size(), 5);

        for _ in 0..5 {
            let again = verify_file(&path).expect("hash");
            assert_eq!(again.sha256, HELLO_SHA256);
            assert!(
                !again.computed,
                "a cache hit must not re-hash (the per-call flag is the instrumentation)"
            );
            assert_eq!(again.cause, ReverifyCause::CacheHit);
            assert_eq!(again.identity, first.identity);
        }
        assert_eq!(sha256_file_cached(&path).expect("hash"), HELLO_SHA256);
    }

    /// 体积变化（内容替换的常见形态）→ 身份不等 → 重新哈希，原因是 `stat_changed`。
    #[test]
    fn a_size_change_forces_a_reverification() {
        let _serial = serial();
        let dir = TempDir::new("verify-size");
        dir.write("model.onnx", b"hello");
        let path = dir.path().join("model.onnx");
        assert!(verify_file(&path).expect("hash").computed);

        std::fs::write(&path, b"hello world").expect("replace with a longer body");
        let replaced = verify_file(&path).expect("hash");
        assert_eq!(replaced.sha256, sha256_file(&path).expect("uncached"));
        assert_ne!(replaced.sha256, HELLO_SHA256);
        assert!(replaced.computed, "a new identity must re-hash");
        assert_eq!(replaced.cause, ReverifyCause::StatChanged);
    }

    /// **同体积**但内容与 mtime 都变了 → 重新哈希，且上报的原因必须是**真实原因**。
    ///
    /// 这里刻意用一个 5 字节的文件：小于 128 KiB 时首尾窗口覆盖整个内容（见模块文档），
    /// 因此"内容变了"这一条会先于 stat 判据成立，上报 `content_changed`。
    /// mtime 变化本身**不**使身份失效（见
    /// [`an_mtime_only_change_still_hits_and_is_not_reported_as_a_content_change`]），
    /// 它只是解释"为什么这个文件值得重新看一眼"的元数据之一。
    #[test]
    fn a_mtime_change_forces_a_reverification() {
        let _serial = serial();
        let dir = TempDir::new("verify-mtime");
        dir.write("model.onnx", b"hello");
        let path = dir.path().join("model.onnx");
        let before = FileIdentity::of(&path).expect("identity");
        assert!(verify_file(&path).expect("hash").computed);

        // 同一个体积、不同内容，mtime 也随之前进。
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
        assert_ne!(after, before, "the identity must differ");
        let digest = verify_file(&path).expect("hash");
        assert_eq!(digest.sha256, sha256_file(&path).expect("uncached"));
        assert!(digest.computed, "a changed identity must re-hash");
        assert_eq!(
            digest.cause,
            ReverifyCause::ContentChanged,
            "the content really changed, and that is the reason to report"
        );
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
        assert!(force_verify_file(&path).is_err());
        // 失败不进缓存：同一个路径被创建出来之后必须真的读它。
        dir.write("not-here.onnx", b"hello");
        let outcome = verify_file(&path).expect("hash after the file appears");
        assert_eq!(outcome.sha256, HELLO_SHA256);
        assert!(outcome.computed);
    }

    /// `clear_verification_cache` 只影响"要不要重新读盘"，不影响任何结论。
    #[test]
    fn clearing_the_cache_only_costs_another_read() {
        let _serial = serial();
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

    /// **B 的核心用例（1a）**：同体积 + 同 mtime，只有**头部** 64 KiB 里的一个字节变了。
    ///
    /// 旧身份 `(path, size, mtime)` 对这个文件判"没变"，缓存会返回旧摘要；局部摘要必须
    /// 抓住它，并给出 `content_changed` 这个原因。
    #[test]
    fn a_same_size_same_mtime_edit_in_the_head_is_detected_and_rehashed() {
        let _serial = serial();
        let dir = TempDir::new("verify-head-edit");
        let path = dir.path().join("model.onnx");
        let size = 256 * 1024;
        write(&path, &pattern(size, 1));
        let first = verify_file(&path).expect("hash");
        assert!(first.computed);
        let stale = first.sha256.clone();

        let _restored = rewrite_same_identity(&path, 4096, b"\xff\xff\xff\xff");
        let after = FileIdentity::of(&path).expect("identity");
        assert_eq!(after.size(), first.identity.size(), "the size is unchanged");
        assert_eq!(
            after.modified(),
            first.identity.modified(),
            "the mtime was written back: the stat identity is unchanged"
        );
        assert_ne!(
            after.partial(),
            first.identity.partial(),
            "the head window must notice the edit"
        );

        let second = verify_file(&path).expect("hash");
        assert!(second.computed, "the cache must be invalidated");
        assert_eq!(second.cause, ReverifyCause::ContentChanged);
        assert_ne!(second.sha256, stale, "the stale digest must not be reused");
        assert_eq!(
            second.sha256,
            sha256_file(&path).expect("uncached"),
            "the fresh digest must be the real one"
        );
    }

    /// **B 的核心用例（1b）**：同上，但改动落在**尾部** 64 KiB 里。
    #[test]
    fn a_same_size_same_mtime_edit_in_the_tail_is_detected_and_rehashed() {
        let _serial = serial();
        let dir = TempDir::new("verify-tail-edit");
        let path = dir.path().join("model.onnx");
        let size = 256 * 1024;
        write(&path, &pattern(size, 2));
        let first = verify_file(&path).expect("hash");
        assert!(first.computed);

        let restored = rewrite_same_identity(&path, size - 8192, b"\x00\x00\x00\x00");
        let after = FileIdentity::of(&path).expect("identity");
        assert_eq!(after.size(), first.identity.size());
        assert_eq!(
            after.modified(),
            first.identity.modified(),
            "the stat identity is unchanged"
        );
        assert_eq!(after.modified(), Some(restored));
        assert_ne!(
            after.partial(),
            first.identity.partial(),
            "the tail window must notice the edit"
        );

        let second = verify_file(&path).expect("hash");
        assert!(second.computed);
        assert_eq!(second.cause, ReverifyCause::ContentChanged);
        assert_eq!(second.sha256, sha256_file(&path).expect("uncached"));
    }

    /// **B 的核心用例（2）**：改动只落在**中段**（首尾 64 KiB 之外），同体积同 mtime。
    ///
    /// 这一条**刻意断言限制**：局部摘要是启发式，不是安全边界，也不是完整内容校验。
    /// 它被钉成测试（而不是只写在文档里），这样"把窗口当成保证"的改动会让它失败。
    #[test]
    fn a_same_size_same_mtime_edit_confined_to_the_middle_is_not_detected() {
        let _serial = serial();
        let dir = TempDir::new("verify-middle-edit");
        let path = dir.path().join("model.onnx");
        let size = 256 * 1024;
        write(&path, &pattern(size, 3));
        let first = verify_file(&path).expect("hash");
        assert!(first.computed);
        let stale = first.sha256.clone();

        // 中段：偏移 [64 KiB, size − 64 KiB) 之内。
        let offset = (PARTIAL_DIGEST_WINDOW_BYTES + 1024) as usize;
        rewrite_same_identity(&path, offset, b"\xaa\xbb\xcc\xdd");
        let after = FileIdentity::of(&path).expect("identity");
        assert_eq!(after.size(), first.identity.size());
        assert_eq!(after.modified(), first.identity.modified());
        assert_eq!(
            after.partial(),
            first.identity.partial(),
            "the head and tail windows are byte-identical by construction"
        );
        assert_ne!(
            sha256_file(&path).expect("uncached"),
            stale,
            "the file content really did change (the full digest differs)"
        );

        let second = verify_file(&path).expect("hash");
        assert!(
            !second.computed,
            "DOCUMENTED LIMITATION: a middle-only edit with an identical stat identity and \
             identical head/tail windows is deliberately not detected"
        );
        assert_eq!(second.cause, ReverifyCause::CacheHit);
        assert_eq!(
            second.sha256, stale,
            "the cache answers with the digest of the previous content: this is the narrowed \
             blind spot, not a bug"
        );

        // 确定性逃生口：强制冷验证必须给出真实摘要并纠正缓存。
        let forced = force_verify_file(&path).expect("hash");
        assert!(
            forced.computed,
            "force_verify_file never consults the cache"
        );
        assert_eq!(forced.cause, ReverifyCause::FirstSight);
        assert_eq!(forced.sha256, sha256_file(&path).expect("uncached"));
        assert_eq!(
            verify_file(&path).expect("hash").sha256,
            forced.sha256,
            "the forced re-verification corrected the cached conclusion"
        );
    }

    /// **B 的核心用例（3）**：小文件的规则是"首尾重叠 ⇒ 局部摘要覆盖整个内容"。
    ///
    /// 小于 128 KiB：任何改动都在首块或尾块里 → 必定被抓到（与中段用例形成对照）。
    #[test]
    fn a_file_smaller_than_the_window_is_fully_covered_by_the_partial_digest() {
        let _serial = serial();
        let dir = TempDir::new("verify-small-file");
        let path = dir.path().join("small.onnx");
        let size = 1024;
        write(&path, &pattern(size, 4));
        let first = verify_file(&path).expect("hash");
        assert!(first.computed);

        // 文件正中间——对 128 KiB 以上的文件正是盲区所在，对小文件却不是。
        rewrite_same_identity(&path, size / 2, b"\x11\x22");
        let after = FileIdentity::of(&path).expect("identity");
        assert_eq!(after.size(), first.identity.size());
        assert_eq!(after.modified(), first.identity.modified());
        assert_ne!(
            after.partial(),
            first.identity.partial(),
            "for size <= 128 KiB the head and tail windows overlap and cover the whole file"
        );

        let second = verify_file(&path).expect("hash");
        assert!(second.computed);
        assert_eq!(second.cause, ReverifyCause::ContentChanged);
        assert_eq!(second.sha256, sha256_file(&path).expect("uncached"));
    }

    /// 命中判据是 `(size, 首尾 64 KiB)`，`mtime` 只是元数据：**内容没变**时把 mtime 改掉
    /// 仍然是命中（不重新哈希整个文件），原因是 `cache_hit` 而不是"内容变了"。
    ///
    /// 这一条把"上报的原因必须是真实原因"钉住：`SetFileTime`、复制文件都会改 mtime，
    /// 把它们报成 `content_changed` 会误导运维（"磁盘上的模型被换过"是另一件事）。
    #[test]
    fn an_mtime_only_change_still_hits_and_is_not_reported_as_a_content_change() {
        let _serial = serial();
        let dir = TempDir::new("verify-mtime-only");
        let path = dir.path().join("model.onnx");
        write(&path, &pattern(4096, 8));
        let first = verify_file(&path).expect("hash");
        assert!(first.computed);
        let before = verification_stats();
        assert_eq!(before.partial_mismatches, 0);

        // 只碰时间戳：内容与体积一个字节都没动。
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open to touch the timestamp");
        file.set_modified(
            first.identity.modified().expect("mtime") + std::time::Duration::from_secs(7),
        )
        .expect("bump mtime");
        drop(file);

        assert_eq!(
            lookup_for_test(&path).expect("the cache answers"),
            ReverifyCause::CacheHit,
            "metadata alone must not invalidate a digest whose content windows are identical"
        );
        let second = verify_file(&path).expect("hash");
        assert!(!second.computed, "no full hash for a metadata-only change");
        assert_eq!(second.cause, ReverifyCause::CacheHit);
        assert_eq!(second.sha256, first.sha256);
        let after = verification_stats();
        assert_eq!(
            after.partial_mismatches, 0,
            "a metadata-only change is not a content change"
        );
        assert_eq!(
            after.cold_verifications, before.cold_verifications,
            "no full hash happened"
        );

        // 命中时本地那份 stat 身份被对齐到当前观察值，因此**下一次**真实的内容替换
        // 报的是"内容变了"，而不是被 mtime 的差异抢先报成"文件被换掉"。
        rewrite_same_identity(&path, 1024, b"\x99");
        assert_eq!(
            lookup_for_test(&path).expect("the cache answers"),
            ReverifyCause::ContentChanged,
            "the recorded cause must describe the real reason"
        );
    }

    /// 边界：空文件与正好等于窗口的长度都必须是**良定义**的。
    ///
    /// 与"小于窗口"那条用同一套规则，但这里断言的是**边界处没有空隙**：
    /// 正好 128 KiB 时首块覆盖 `[0,64K)`、尾块覆盖 `[64K,128K)`，合起来是整个文件。
    #[test]
    fn empty_and_exactly_window_sized_files_are_well_defined() {
        let _serial = serial();
        let dir = TempDir::new("verify-boundary");
        let empty = dir.path().join("empty.onnx");
        dir.write("empty.onnx", b"");
        let identity = FileIdentity::of(&empty).expect("identity of an empty file");
        assert_eq!(identity.size(), 0);
        assert_eq!(identity.partial().size(), 0);
        assert_eq!(
            identity.partial(),
            &PartialDigest::compute(&empty, 0).expect("again")
        );
        assert!(verify_file(&empty).expect("hash").computed);

        let exact = dir.path().join("exact.onnx");
        let size = (2 * PARTIAL_DIGEST_WINDOW_BYTES) as usize;
        write(&exact, &pattern(size, 5));
        let before = FileIdentity::of(&exact).expect("identity");
        assert_eq!(before.size(), size as u64);
        // 正好 128 KiB：首块 = 前 64 KiB，尾块 = 后 64 KiB，无重叠也无空隙。
        rewrite_same_identity(&exact, 0, b"\x01");
        let head_changed = FileIdentity::of(&exact).expect("identity");
        assert_ne!(head_changed.partial(), before.partial());
        rewrite_same_identity(&exact, size - 1, b"\x02");
        let tail_changed = FileIdentity::of(&exact).expect("identity");
        assert_ne!(tail_changed.partial(), head_changed.partial());
        // 中段（第 64 KiB 个字节起）对 128 KiB 的文件仍是首块/尾块之外吗？
        // 不是：`size == 128 KiB` 时首块覆盖 [0,64K)、尾块覆盖 [64K,128K)，合起来是全部。
        // 因此再改中段同样必须被发现——这正是"边界处无空隙"的断言。
        rewrite_same_identity(&exact, (PARTIAL_DIGEST_WINDOW_BYTES as usize) + 8, b"\x03");
        let middle_changed = FileIdentity::of(&exact).expect("identity");
        assert_ne!(middle_changed.partial(), tail_changed.partial());
    }

    /// **B 的核心用例（4）**：命中路径只花一次 `stat` + 一次 128 KiB 局部读，
    /// **不**完整哈希（`computed = false`，且摘要仍然是第一次算出来的那一份）。
    ///
    /// 证据必须是**单次调用**的属性：进程级的累计计数器会被**另一个测试二进制的并行测试**
    /// 干扰（`cargo test --all-targets` 同时跑 `--lib` 与 `serve` 的 bin 测试，而缓存与账本
    /// 都是进程级/静态的），因此这里不用"计数器没变"来论证，而是用
    /// [`VerificationOutcome::computed`]（"这一次算没算"）与摘要本身。
    #[test]
    fn a_cache_hit_reads_only_the_windows_and_never_rehashes() {
        let _serial = serial();
        let dir = TempDir::new("verify-hit-cost");
        let path = dir.path().join("model.onnx");
        write(&path, &pattern(64 * 1024, 6));

        let cold = verify_file(&path).expect("hash");
        assert!(cold.computed);
        assert_eq!(verification_stats().last_cold_bytes, 64 * 1024);

        for _ in 0..3 {
            let hit = verify_file(&path).expect("hash");
            assert!(
                !hit.computed,
                "DOCUMENTED COST: a hit must not perform a full hash"
            );
            assert_eq!(hit.cause, ReverifyCause::CacheHit);
            assert_eq!(
                hit.sha256, cold.sha256,
                "a hit must answer with the digest that was actually computed"
            );
            assert_eq!(hit.identity, cold.identity);
        }
    }

    /// 局部摘要本身的性质：首尾任一字节变化都改变它，中段不改变它（同体积）。
    #[test]
    fn the_partial_digest_binds_the_first_and_last_window_only() {
        let _serial = serial();
        let dir = TempDir::new("verify-partial-binds");
        let path = dir.path().join("model.onnx");
        let size = 200 * 1024;
        write(&path, &pattern(size, 7));
        let base = PartialDigest::compute(&path, size as u64).expect("partial");

        rewrite_same_identity(&path, 0, b"\x7f");
        let head = PartialDigest::compute(&path, size as u64).expect("partial");
        assert_ne!(head, base, "the first byte is inside the head window");

        rewrite_same_identity(&path, size - 1, b"\x7e");
        let tail = PartialDigest::compute(&path, size as u64).expect("partial");
        assert_ne!(tail, head, "the last byte is inside the tail window");

        let full_before_middle = sha256_file(&path).expect("uncached");
        rewrite_same_identity(&path, size / 2, b"\x7d");
        let middle = PartialDigest::compute(&path, size as u64).expect("partial");
        assert_eq!(
            middle, tail,
            "a middle-only edit does not move the partial digest (the documented heuristic)"
        );
        assert_ne!(
            sha256_file(&path).expect("uncached"),
            full_before_middle,
            "the full digest must have changed, otherwise the assertion above proves nothing"
        );
    }
}
