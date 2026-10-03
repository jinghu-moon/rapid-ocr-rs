//! 模型文件的下载、完整性校验与原子落盘（`docs/05-local-web-demo-implementation.md` §6）。
//!
//! # 唯一入口
//!
//! 只有一个下载入口：[`download_verified`]。它把"从可信来源取一个文件、保证内容正确、
//! 原子地放进模型目录"这件事实现一次。旧入口
//! `ensure_downloaded(file_url, expected_sha256: Option<&str>, save_dir)` 允许传 `None`
//! 哈希（即"下载但不校验"），已按 §6.4 删除：**没有兼容层，也没有可传 `None` 哈希的旁路**。
//!
//! # §6.1 的硬性要求在本模块的落点
//!
//! | 要求 | 实现 | 测试 |
//! | --- | --- | --- |
//! | 1 仅 HTTPS | 非 `https` → [`DownloadError::SchemeRejected`]，且在任何文件系统/网络动作**之前**判定 | `an_http_url_is_rejected_by_the_public_entry_point` |
//! | 2 手工逐跳重定向 | `ClientBuilder::redirect(Policy::none())` + `follow_redirect`：最多 [`MAX_REDIRECT_HOPS`] 跳，**每一跳**都按"仅 https + 生效白名单"重验，相对 `Location` 按当前 URL 解析；越界/超限 → [`DownloadError::RedirectRejected`] | `a_redirect_to_an_allowed_host_is_followed_and_verified`、`a_redirect_to_a_host_outside_the_allow_list_is_rejected`、`a_redirect_chain_longer_than_the_hop_limit_is_rejected`、`a_redirect_that_downgrades_to_http_is_rejected`、`a_redirect_target_receives_no_credential_header`、`a_redirected_body_above_the_cap_is_still_rejected`、`a_cdn_style_redirect_with_a_hash_path_still_lands_under_the_original_name` |
//! | 3 host 白名单来自可信配置 | [`ALLOWED_DOWNLOAD_HOSTS`] 是编译期常量；本地 `manifest.json` **只能提供 URL，不能扩大它**；扩展必须经 [`DownloadRequest::allowed_hosts`] 这个**显式参数**传入 | `the_allowed_download_hosts_are_exactly_the_declared_set`、`a_local_manifest_cannot_widen_the_download_host_allow_list`、`an_explicit_host_allow_list_extends_the_compiled_in_one` |
//! | 4 `Content-Length` 预检 | 超过 `max_bytes` 在**创建临时文件之前**拒绝 | `a_declared_length_above_the_cap_is_rejected_before_anything_is_written` |
//! | 5 流式上限 | 无可用长度时 `take(max_bytes + 1)`；超限删除临时文件 | `a_chunked_body_above_the_cap_is_rejected_and_leaves_no_temp_file`、`a_close_delimited_body_above_the_cap_is_rejected` |
//! | 6 唯一临时文件名 | `.part-<pid>-<seq>`（[`unique_part_path`]），并发或残留的 `.part` 不会互撞 | `two_concurrent_downloads_of_one_target_fetch_exactly_once` |
//! | 7 Windows 原子替换 | `MoveFileExW(…, MOVEFILE_REPLACE_EXISTING \| MOVEFILE_WRITE_THROUGH)`（[`replace_file`]） | `an_existing_corrupt_target_is_replaced`、`a_failed_replace_keeps_the_original_file` |
//! | 8 同文件单飞 | 按目标路径的进程内锁（[`target_lock`]） | `two_concurrent_downloads_of_one_target_fetch_exactly_once` |
//! | 9 哈希必填 | `expected_sha256: &str`；不匹配删除临时文件并返回 [`DownloadError::HashMismatch`] | `a_hash_mismatch_deletes_the_temp_file_and_leaves_no_target` |
//! | 10 磁盘空间预检 | `GetDiskFreeSpaceExW`；空间来源可注入（[`FreeSpaceProbe`]） | `insufficient_disk_space_is_reported_before_anything_is_written` |
//! | 11 分项超时 | [`DownloadRequest::connect_timeout`] / [`DownloadRequest::read_timeout`] | `an_expired_connect_budget_is_a_connect_timeout`、`a_stalled_body_read_is_a_read_timeout` |
//! | 12 错误分类 | [`DownloadError`]（唯一的十二类实现，serve 侧直接复用） | `every_download_error_class_has_a_stable_kind` |
//!
//! # 进度、取消与空间核算（§6.5/§6.6）
//!
//! [`DownloadObserver`] 是集合下载的进度与取消接口：[`DownloadObserver::file_started`] 是
//! **唯一**的取消检查点（返回 `false` → 该文件不开始，立即以 [`DownloadError::Cancelled`]
//! 结束，已完成并校验过的文件保留）；[`DownloadObserver::bytes_written`] 报告当前文件的
//! 累计字节；[`DownloadObserver::file_finished`] 报告一个文件已落盘。
//! [`available_disk_bytes`] 把"任务级空间核算"（§6.5）需要的那个探测暴露出来，
//! 它与下载器内部用的是同一个 `GetDiskFreeSpaceExW` 实现。
//!
//! # 临时文件的生命周期
//!
//! 临时文件由 [`PartFile`] 持有：**任何**提前返回（拒绝、超限、超时、哈希不符、替换失败）
//! 都会在 `Drop` 里把它删掉，因此不存在"校验没过但文件还在"的路径。这是"不留可疑文件"
//! 的结构性保证，而不是在每个 `return` 上手写 `remove_file`。
//!
//! # 为什么生产代码里没有 HTTPS 的旁路
//!
//! [`DownloadPolicy`] 的白名单在生产路径上只有一份取值——[`ALLOWED_DOWNLOAD_HOSTS`]——
//! 并且只由 [`DownloadPolicy::production`] 构造。单测需要本机明文 fixture 服务器
//! （测试不得依赖公网，见 `src/test_support.rs` 的 `HttpFixture`），因此策略里那个
//! "放行 `http://`"的口子是 `#[cfg(test)]` 字段：**生产构建里它不存在**，
//! 也不可能被 `serve` 或清单打开。
//!
//! # 为什么手工逐跳跟随重定向（而不是 `Policy::limited(n)`）
//!
//! 默认表里的 ONNX 权重在 ModelScope 上不是直链：真实主机对 `onnx/**` 应答 **302** 到
//! `cdn-lfs-cn-1.modelscope.cn`（M2 实测，M2b 复核：1 跳、1,829,618 B、哈希与默认表一致）。
//! 因此"收到 3xx 就拒绝"会让**所有权重**都下载不了。两种做法里：
//!
//! - `reqwest` 的 `Policy::limited(n)` 会**盲目**跟随：它不检查下一跳的 scheme/host，
//!   因此一个被控的主机可以把下载器指到任意地址（OWASP SSRF：重定向是绕过白名单的经典
//!   路径），也可能把 `https` 降级成 `http`；
//! - [`follow_redirect`] 自己读 `Location`、自己校验、自己发下一个请求：**每一跳**都
//!   重跑与初始 URL 完全相同的那两条判定（仅 `https` + 生效白名单），相对 `Location` 按
//!   当前 URL 解析，跳数上界是 [`MAX_REDIRECT_HOPS`]。
//!
//! 白名单本身没有被放宽：跳转到白名单外的 host 仍然是 [`DownloadError::HostRejected`]
//! （指出那个 host），跳数超限或缺少 `Location` 是 [`DownloadError::RedirectRejected`]。
//! 重定向只改变"从哪里取字节"，不改变"字节必须匹配声明的 SHA-256"。
//!
//! # 为什么不用 `fs::rename`
//!
//! §6.7 要求覆盖语义明确、失败时可保留原文件。`fs::rename` 与
//! `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` 在 std 里目前走同一个 API，但 std 既不请求
//! `MOVEFILE_WRITE_THROUGH`，也不把 Win32 错误码交给调用方。因此这里只保留一条路径：
//! 显式调用 Win32；失败即返回带错误码的可定位错误（含"原文件未被改动"的说明），
//! 不做"先删后改名"——那会引入崩溃窗口，§6.7 明确禁止。

use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use reqwest::{blocking::Client, redirect::Policy};
use sha2::{Digest, Sha256};

use crate::{
    error::{RapidOcrError, Result},
    model_set::{ModelFileSpec, ModelFileState, ModelSet},
};

pub fn default_model_store_dir() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(local_app_data)
            .join("rapid-ocr-rs")
            .join("models");
    }

    PathBuf::from("models")
}

pub fn verify_existing_file(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref().to_path_buf();
    if !path.exists() {
        return Err(RapidOcrError::FileNotFound(path));
    }
    if !path.is_file() {
        return Err(RapidOcrError::Config(format!(
            "expected a file path, got directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

// ---------------------------------------------------------------------------
// 可信下载配置（编译期常量，唯一来源）
// ---------------------------------------------------------------------------

/// 允许下载模型文件的 host（**唯一**定义处，编译期常量）。
///
/// OWASP：SSRF 白名单必须来自**可信配置**，而不是资源描述自身。本 crate 的模型来源是
/// 本地 `manifest.json` 或 `assets/default_models.yaml`，两者都是"资源描述"，
/// 因此它们只能提供 URL，**不能扩大**这份白名单：越界的 host 一律
/// [`DownloadError::HostRejected`]。
///
/// # 为什么有第二项（ModelScope 的 LFS CDN）
///
/// 默认表里的 ONNX 权重在 ModelScope 上是 **302 → `cdn-lfs-cn-1.modelscope.cn`**（M2 与
/// M2b 两次实测：`…/onnx/PP-OCRv6/det/PP-OCRv6_det_tiny.onnx` 与另外四个权重全部 302 到
/// 同一个 host）。`§6.1` 第 2 条允许的"逐跳校验"因此必须真的有一份**可信**的 hop 白名单，
/// 否则跟随重定向只是把 URL 的判断权交给上游。这一项是**显式审查**的结果：
/// 它是 ModelScope 自己的对象存储域名，只作为 `Location` 目标出现，权重内容仍然必须匹配
/// 默认表声明的 SHA-256。
///
/// **扩大这份白名单必须改这一个常量**：`the_allowed_download_hosts_are_exactly_the_declared_set`
/// 把它逐项锁死，新增一项就会让测试失败，从而强制一次显式审查。serve 层的
/// `--allow-download-host`（§6.1 第 3 条、M2 接线）是**用户显式选择**的入口，
/// 不会让库放宽这里的常量：库永远只认这份声明。
pub const ALLOWED_DOWNLOAD_HOSTS: [&str; 2] = ["www.modelscope.cn", "cdn-lfs-cn-1.modelscope.cn"];

/// [`DownloadRequest::new`] 使用的默认允许列表：**就是**编译期白名单本身。
///
/// §6.1 第 3 条的 opt-in 是[`DownloadRequest::allowed_hosts`]这个**显式参数**：
/// 库**永不**修改 [`ALLOWED_DOWNLOAD_HOSTS`]，调用方只是把自己信任的列表传进来；
/// 不传时得到的就是这份编译期声明。因此"本地 manifest 自己扩大白名单"这条路径
/// 在类型上不存在——它只能提供一个 URL，不能提供一个参数。
pub const DEFAULT_ALLOWED_HOSTS: &[&str] = &ALLOWED_DOWNLOAD_HOSTS;

/// 库内默认的单文件下载上限（MiB），与 §3 的 `--max-download-mb` 默认值同源。
pub const DEFAULT_MAX_DOWNLOAD_MB: u64 = 1024;
/// [`DEFAULT_MAX_DOWNLOAD_MB`] 的字节形式（必须大于 §6.2 提到的 566 MB 公式模型）。
pub const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = DEFAULT_MAX_DOWNLOAD_MB * 1024 * 1024;
/// 库内默认的连接阶段超时。
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 库内默认的读取阶段超时。
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 一次下载**最多**跟随的重定向跳数（§6.1 第 2 条的"逐跳校验"）。
///
/// 取值理由（不是照抄某个默认值）：
///
/// - 真实来源只需要 **1 跳**（`www.modelscope.cn` → `cdn-lfs-cn-1.modelscope.cn`，M2/M2b
///   实测），5 跳留出了 CDN 换名/加一跳的余量；
/// - 每多一跳就多一次网络往返，而跳数的唯一作用是"到达最终响应"；把上界压到 5 意味着
///   一个恶意的 302 环最多只能让下载器多发 5 次请求（每次仍受
///   [`DownloadRequest::connect_timeout`] / [`DownloadRequest::read_timeout`] 约束），
///   同时超限本身就是可定位的 [`DownloadError::RedirectRejected`]；
/// - 与"仅 https + 白名单"一起，这构成 §6.1 第 2 条要求的完整跟随策略。
pub const MAX_REDIRECT_HOPS: usize = 5;

// ---------------------------------------------------------------------------
// 错误分类（唯一定义处）
// ---------------------------------------------------------------------------

/// 加固下载器的错误分类（§6.1 第 12 条的十二类）。
///
/// **这是唯一实现**：serve 侧（`src/bin/serve/error.rs`）不再定义自己的下载错误类型，
/// 它只在这十二类之上补一层 HTTP 状态码/`code`/`detail` 映射；因此同一个失败原因
/// 不可能出现两种表示。
///
/// `kind()` 是机器可读的稳定标签（进 `/api/*` 的 `detail.kind`），`Display` 是人类可读
/// 说明。两者都不含 HTTP 语义——库不引入 HTTP 概念（§2.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadError {
    /// 非 HTTPS（§6.1 第 1 条）。
    SchemeRejected { scheme: String },
    /// 重定向被拒绝：跳数超过 [`MAX_REDIRECT_HOPS`]，或 3xx 没有可解析的 `Location`
    /// （§6.1 第 2 条）。`location` 是**未经改动**的 `Location` 头（缺失时为 `None`），
    /// 因此错误里能看到"上游到底把我们指到哪里"。
    ///
    /// 跳转目标的 scheme 不合法是 [`Self::SchemeRejected`]，host 不在生效白名单内是
    /// [`Self::HostRejected`]——"超限/无目标"与"目标本身不可信"是两类不同的定位信息。
    RedirectRejected { location: Option<String> },
    /// host 不在 [`ALLOWED_DOWNLOAD_HOSTS`] 内（§6.1 第 3 条）。
    HostRejected { host: String },
    /// 超过 `max_bytes`（§6.1 第 4、5 条）。
    TooLarge {
        limit_bytes: u64,
        observed_bytes: Option<u64>,
    },
    /// URL 无法解析、传输失败，或 HTTP 状态码不是 2xx。
    Network { detail: String },
    /// 连接阶段超时（§6.1 第 11 条）。
    ConnectTimeout { timeout_ms: u64 },
    /// 读取阶段超时（§6.1 第 11 条）。
    ReadTimeout { timeout_ms: u64 },
    /// 可用磁盘空间不足（§6.5）。
    InsufficientSpace {
        required_bytes: u64,
        available_bytes: u64,
    },
    /// 下载内容与期望 SHA-256 不符（§6.1 第 9 条）。
    HashMismatch { expected: String, actual: String },
    /// 在**文件边界**被取消（§6.6）。产生于下载 worker；本模块只定义这一分类。
    Cancelled,
}

impl DownloadError {
    /// 机器可读的稳定标签（进 `detail.kind`）。
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::SchemeRejected { .. } => "scheme",
            Self::RedirectRejected { .. } => "redirect",
            Self::HostRejected { .. } => "host",
            Self::TooLarge { .. } => "too_large",
            Self::Network { .. } => "network",
            Self::ConnectTimeout { .. } => "connect_timeout",
            Self::ReadTimeout { .. } => "read_timeout",
            Self::InsufficientSpace { .. } => "insufficient_space",
            Self::HashMismatch { .. } => "hash_mismatch",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SchemeRejected { scheme } => {
                write!(f, "only https downloads are allowed, got scheme `{scheme}`")
            }
            Self::RedirectRejected { location } => match location {
                Some(location) => write!(
                    f,
                    "the model host answered with a redirect to `{location}` that would exceed the \
                     {MAX_REDIRECT_HOPS}-hop redirect limit"
                ),
                None => write!(
                    f,
                    "the model host answered with a redirect that carries no usable `Location` \
                     header"
                ),
            },
            Self::HostRejected { host } => {
                write!(f, "`{host}` is not in the trusted download host allow-list")
            }
            Self::TooLarge {
                limit_bytes,
                observed_bytes,
            } => match observed_bytes {
                Some(observed) => write!(
                    f,
                    "the download is {observed} bytes, which exceeds the {limit_bytes} byte limit"
                ),
                None => write!(f, "the download exceeds the {limit_bytes} byte limit"),
            },
            Self::Network { detail } => write!(f, "the download failed: {detail}"),
            Self::ConnectTimeout { timeout_ms } => {
                write!(f, "the download could not connect within {timeout_ms} ms")
            }
            Self::ReadTimeout { timeout_ms } => {
                write!(
                    f,
                    "the download stalled for more than {timeout_ms} ms while reading"
                )
            }
            Self::InsufficientSpace {
                required_bytes,
                available_bytes,
            } => write!(
                f,
                "insufficient disk space: {required_bytes} bytes required, {available_bytes} \
                 bytes available"
            ),
            Self::HashMismatch { expected, actual } => write!(
                f,
                "the downloaded file does not match the expected SHA-256 (expected {expected}, got \
                 {actual})"
            ),
            Self::Cancelled => write!(f, "the download was cancelled at a file boundary"),
        }
    }
}

impl std::error::Error for DownloadError {}

// ---------------------------------------------------------------------------
// 进度与取消：下载的观察者（§6.6）
// ---------------------------------------------------------------------------

/// 一次集合下载的进度与取消观察者（§6.6）。
///
/// # 为什么取消只在文件边界生效
///
/// `download_verified` 的读取循环不可中断：`reqwest` 的 blocking 读取一旦发起，就没有
/// 安全的"半途放弃"语义（放弃也只会留下半个临时文件）。因此 §6.6 把取消定义在**文件
/// 边界**：[`Self::file_started`] 是**唯一**的取消检查点，它的返回值决定是否开始**下一个**
/// 文件。已经通过 SHA-256 校验并原子替换的文件一律保留；当前文件的临时文件由
/// `PartFile` 的 `Drop` 保证不会残留。
///
/// **做不到的事（如实声明）**：`file_started` 返回 `false` 之后，下载器不会中断**正在
/// 进行**的那一次文件下载——它会在该文件结束（或失败）之后返回
/// [`DownloadError::Cancelled`]。因此"取消后立刻停止网络 I/O"不是本接口的语义。
///
/// 三个回调都在**下载线程**上同步调用，因此实现必须自己保证开销可控（serve 侧只更新
/// 任务存储里的几个计数器）。
pub trait DownloadObserver {
    /// 开始一个文件之前调用（**唯一的取消检查点**）。
    ///
    /// `index` 从 1 开始，`total` 是本次任务要下载的文件数（已 `Present` 的文件不计数），
    /// `declared_bytes` 是模型来源声明的体积（未知为 `None`）。
    ///
    /// 返回 `false` = 在文件边界取消：下载器**不开始**这个文件，立即返回
    /// [`DownloadError::Cancelled`]。
    fn file_started(
        &mut self,
        file: &ModelFileSpec,
        index: usize,
        total: usize,
        declared_bytes: Option<u64>,
    ) -> bool;

    /// 当前文件已写入的累计字节（每个读取块调用一次，单调不减）。
    fn bytes_written(&mut self, written_bytes: u64);

    /// 一个文件已通过 SHA-256 校验并落盘。
    fn file_finished(&mut self, file: &ModelFileSpec, index: usize, bytes: u64);
}

/// 不关心进度、从不取消的观察者（库内调用方的默认）。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoObserver;

impl DownloadObserver for NoObserver {
    fn file_started(
        &mut self,
        _file: &ModelFileSpec,
        _index: usize,
        _total: usize,
        _declared_bytes: Option<u64>,
    ) -> bool {
        true
    }

    fn bytes_written(&mut self, _written_bytes: u64) {}

    fn file_finished(&mut self, _file: &ModelFileSpec, _index: usize, _bytes: u64) {}
}

// ---------------------------------------------------------------------------
// 单文件下载
// ---------------------------------------------------------------------------

/// 一次经过加固的单文件下载请求（§6）。
#[derive(Debug, Clone, Copy)]
pub struct DownloadRequest<'a> {
    /// 下载 URL：必须 `https`，host 必须在 `allowed_hosts` 内。
    pub url: &'a str,
    /// 期望的 SHA-256，**必填**（§6.4：不接受 `None`）。
    pub expected_sha256: &'a str,
    /// 落盘目录；文件名取 URL 末段（[`extract_file_name`] 是唯一实现）。
    pub save_dir: &'a Path,
    /// 单文件上限，来自 `--max-download-mb`（§6.2）。
    pub max_bytes: u64,
    /// 连接阶段超时。
    pub connect_timeout: Duration,
    /// 读取阶段超时。
    ///
    /// reqwest 的 blocking 客户端按"每次阻塞等待"计时，因此这条超时同时覆盖
    /// "等待响应头"与"读取响应体"；它**不是**整个下载的总时长——一个持续推进的
    /// 600 MB 下载不会因为耗时超过它而失败。
    pub read_timeout: Duration,
    /// 可信的 host 允许列表（§6.1 第 3 条）。
    ///
    /// **这是一个显式参数，不是对 [`ALLOWED_DOWNLOAD_HOSTS`] 的修改**：库常量永远是
    /// "随本 crate 一起审查过的那一份"，`--allow-download-host`（或任何扩展）只能由
    /// 用户显式传进来。默认值见 [`DEFAULT_ALLOWED_HOSTS`]。
    pub allowed_hosts: &'a [&'a str],
}

impl<'a> DownloadRequest<'a> {
    /// 库内调用方（`EngineConfig::allow_download` 分支）使用的默认请求：
    /// 单文件上限 [`DEFAULT_MAX_DOWNLOAD_BYTES`]，超时用 [`DEFAULT_CONNECT_TIMEOUT`] /
    /// [`DEFAULT_READ_TIMEOUT`]，允许列表用编译期的 [`ALLOWED_DOWNLOAD_HOSTS`]。
    /// serve 侧用 CLI 上的显式取值覆盖它们。
    pub fn new(url: &'a str, expected_sha256: &'a str, save_dir: &'a Path) -> Self {
        Self {
            url,
            expected_sha256,
            save_dir,
            max_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
            allowed_hosts: DEFAULT_ALLOWED_HOSTS,
        }
    }
}

/// 下载 `req.url` 到 `req.save_dir/`，校验 SHA-256 后原子替换目标文件。
///
/// 目标文件已存在且哈希正确时**不发请求**（缓存命中）；已存在但哈希不符时按 §6.3
/// 走"重新下载并原子替换"，替换失败保留原文件。
///
/// 不接收观察者（因此没有进度、也不会在文件边界取消）：需要这两件事的调用方用
/// [`download_model_set_observed`]。
pub fn download_verified(req: &DownloadRequest<'_>) -> Result<PathBuf> {
    download_verified_with(
        req,
        &DownloadPolicy::production(&WindowsFreeSpace),
        &mut NoObserver,
    )
}

/// 默认表/清单解析出的可选哈希 → 必填哈希（§6.4）。
///
/// `ModelRegistry` 的解析结果里 `sha256` 是 `Option<String>`（默认表的 schema 允许缺项），
/// 但**下载**不允许无哈希：缺项在这里变成可定位错误，而不是"悄悄下载一个不校验的文件"。
/// 这条分支目前不会被任何随仓库提交的表触发（`model_registry` 的
/// `every_default_table_entry_carries_a_sha256` 锁住了 40 个权重与 30 个字典），
/// 它的存在是因为类型允许缺项，而不是因为现在真的缺。
pub fn require_model_hash<'a>(expected: Option<&'a str>, source: &str) -> Result<&'a str> {
    match expected {
        Some(hash) if !hash.trim().is_empty() => Ok(hash),
        _ => Err(RapidOcrError::ModelResolve(format!(
            "the model source records no SHA-256 for {source}; refusing to download without a hash \
             (docs/05 §6.4)"
        ))),
    }
}

/// 一次下载任务的预算：**单文件上限同时也是整批下载的总量额度**（§6.2）。
///
/// 语义来自 §6.2：`--max-download-mb` 对**每个文件**生效，并作为**整个下载任务**的
/// 总量上限（多文件集合按剩余额度递减）。因此下一个文件的可用上限是
/// "单文件上限"与"剩余额度"的较小者——因为两者是同一个数，[`Self::per_file_cap`]
/// 就是剩余额度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadBudget {
    total_bytes: u64,
    spent_bytes: u64,
}

impl DownloadBudget {
    /// 用 `--max-download-mb` 换算出的字节数初始化。
    pub const fn new(total_bytes: u64) -> Self {
        Self {
            total_bytes,
            spent_bytes: 0,
        }
    }

    /// 命令行给出的总量额度。
    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// 已经下载并记账的字节数。
    pub const fn spent_bytes(&self) -> u64 {
        self.spent_bytes
    }

    /// 剩余额度。
    pub const fn remaining_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.spent_bytes)
    }

    /// 下一个文件的可用上限（就是剩余额度，见类型文档）。
    pub const fn per_file_cap(&self) -> u64 {
        self.remaining_bytes()
    }

    /// 把一个文件的实际字节数计入已用额度。
    ///
    /// 超出剩余额度时返回 [`DownloadError::TooLarge`]，并且**不消耗**额度
    /// （失败不记账，避免预算被一次失败的下载吃掉）。
    pub fn charge(&mut self, bytes: u64) -> Result<()> {
        let remaining = self.remaining_bytes();
        if bytes > remaining {
            return Err(DownloadError::TooLarge {
                limit_bytes: remaining,
                observed_bytes: Some(bytes),
            }
            .into());
        }
        self.spent_bytes += bytes;
        Ok(())
    }
}

/// 按顺序补齐一个模型集合里缺失/损坏的文件，**共用一个递减的预算**（§6.2）。
///
/// 规则：
///
/// 1. 每个文件的名字必须等于其 URL 末段：下载器按 URL 末段落盘
///    （[`extract_file_name`] 是唯一实现），两者不一致会让下载产物与逐文件状态校验
///    指向不同路径，因此这里直接报可定位错误；
/// 2. 没有哈希的文件拒绝下载（§6.4 不允许无校验下载）；
/// 3. 没有可信来源的文件拒绝下载；
/// 4. `size_bytes` **已知**且超过剩余额度 → 在**发请求之前**拒绝，并说明两个数值；
/// 5. `size_bytes` 未知 → 仍然受剩余额度这条**流式**上限保护（不是跳过检查）。
///
/// 返回值与集合的 `files` 同序，且每一项都已在磁盘上通过哈希校验。
///
/// 这是**不带观察者**的入口：不允许取消、也不上报进度（库内调用方的默认）。
/// serve 侧的下载任务用 [`download_model_set_observed`]（它接受一个显式的 host 允许
/// 列表与一个 [`DownloadObserver`]，因此 §6.6 的"文件边界取消"与进度上报是真实存在的）。
pub fn download_model_set(
    set: &ModelSet,
    root: &Path,
    budget: &mut DownloadBudget,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<Vec<PathBuf>> {
    download_model_set_observed(
        set,
        root,
        budget,
        connect_timeout,
        read_timeout,
        DEFAULT_ALLOWED_HOSTS,
        &mut NoObserver,
    )
}

/// [`download_model_set`] 的完整入口：**显式** host 允许列表 + 进度/取消观察者。
///
/// `allowed_hosts` 是 §6.1 第 3 条的那个"必须来自可信配置"的列表：默认是编译期常量
/// （[`DEFAULT_ALLOWED_HOSTS`]），扩展只能由调用方**显式**传入（serve 侧的
/// `--allow-download-host`）。库不会因为清单里写了什么而改变它。
pub fn download_model_set_observed(
    set: &ModelSet,
    root: &Path,
    budget: &mut DownloadBudget,
    connect_timeout: Duration,
    read_timeout: Duration,
    allowed_hosts: &[&str],
    observer: &mut dyn DownloadObserver,
) -> Result<Vec<PathBuf>> {
    download_model_set_with(
        set,
        root,
        budget,
        connect_timeout,
        read_timeout,
        allowed_hosts,
        observer,
        &DownloadPolicy::production(&WindowsFreeSpace),
    )
}

/// [`download_model_set`] 的实现；策略可注入，因此单测可以指向本机 fixture 服务器。
#[allow(clippy::too_many_arguments)]
fn download_model_set_with(
    set: &ModelSet,
    root: &Path,
    budget: &mut DownloadBudget,
    connect_timeout: Duration,
    read_timeout: Duration,
    allowed_hosts: &[&str],
    observer: &mut dyn DownloadObserver,
    policy: &DownloadPolicy<'_>,
) -> Result<Vec<PathBuf>> {
    set.validate()?;
    fs::create_dir_all(root)?;

    let status = set.status(root);
    // 进度口径：`total` 是**需要下载**的文件数（缺失 ∪ 损坏）。已经 `Present`
    // 的文件不需要网络也不需要空间，因此既不计数也不触发取消检查点
    // （这与 §5.2 的 `download_bytes_total` 只统计 `Missing` 的口径不同：
    // 损坏文件会被重新下载，因此它必须进任务进度）。
    let total = status.files.iter().filter(|(_, s)| !s.is_present()).count();
    let mut index = 0_usize;

    let mut files = Vec::with_capacity(status.files.len());
    for (spec, state) in &status.files {
        if !spec.has_hash() {
            return Err(RapidOcrError::ModelResolve(format!(
                "`{}` has no SHA-256 in the model source, so it cannot be downloaded (docs/05 \
                 §6.4 refuses an unverified download)",
                spec.name
            )));
        }
        if *state == ModelFileState::Present {
            files.push(root.join(&spec.name));
            continue;
        }
        if !spec.has_source_url() {
            return Err(RapidOcrError::ModelResolve(format!(
                "`{}` is {} and the model source records no trusted download source for it",
                spec.name,
                state.as_str()
            )));
        }
        let from_url = extract_file_name(&spec.source_url)?;
        if from_url != spec.name {
            return Err(RapidOcrError::ModelResolve(format!(
                "the model source lists `{}` but its URL ends in `{from_url}`; the downloader \
                 writes the URL's last segment, so the two names must agree",
                spec.name
            )));
        }

        // §6.6：**唯一的取消检查点**。返回 `false` 时不开始这个文件，立即以
        // `Cancelled` 结束（已经下载并校验过的文件留在磁盘上）。
        index += 1;
        if !observer.file_started(spec, index, total, spec.size_bytes) {
            return Err(DownloadError::Cancelled.into());
        }

        let cap = budget.per_file_cap();
        // §6.2：声明体积已知且超过剩余额度 → 在**发请求之前**拒绝。
        if let Some(size) = spec.size_bytes
            && size > cap
        {
            return Err(DownloadError::TooLarge {
                limit_bytes: cap,
                observed_bytes: Some(size),
            }
            .into());
        }

        let request = DownloadRequest {
            url: &spec.source_url,
            expected_sha256: &spec.sha256,
            save_dir: root,
            max_bytes: cap,
            connect_timeout,
            read_timeout,
            allowed_hosts,
        };
        let downloaded = download_verified_with(&request, policy, observer)?;
        let written = fs::metadata(&downloaded)?.len();
        budget.charge(written)?;
        observer.file_finished(spec, index, written);
        files.push(downloaded);
    }
    Ok(files)
}

/// 下载策略：磁盘空间来源与（仅单测的）明文口子。
///
/// **白名单不在这里**：可信 host 列表是 [`DownloadRequest::allowed_hosts`] 的一部分，
/// 因为它是**请求的契约**（§6.1 第 3 条），不是运行环境。把它同时放在策略里会让同一个
/// 决定有两个来源——那正是本 crate 明确要避免的双权威。
///
/// 生产路径只有 [`Self::production`] 一个构造点。
struct DownloadPolicy<'a> {
    free_space: &'a dyn FreeSpaceProbe,
    /// **仅单测**：放行 `http://`，以便对 `127.0.0.1` 上的明文 fixture 服务器验证
    /// 传输/落盘/预算逻辑（测试不得依赖公网）。生产构建里这个字段不存在，
    /// 因此"仅 HTTPS"是编译期保证。
    #[cfg(test)]
    insecure_http: bool,
}

impl<'a> DownloadPolicy<'a> {
    fn production(free_space: &'a dyn FreeSpaceProbe) -> Self {
        Self {
            free_space,
            #[cfg(test)]
            insecure_http: false,
        }
    }

    fn scheme_is_acceptable(&self, scheme: &str) -> bool {
        if scheme == "https" {
            return true;
        }
        #[cfg(test)]
        if self.insecure_http && scheme == "http" {
            return true;
        }
        false
    }
}

/// 目标文件的进程内单飞锁（§6.1 第 8 条）。
///
/// 键是目标文件的完整路径。表项**不回收**：条目数与"进程内下载过的不同模型文件名"
/// 同阶（模型目录是扁平的，文件数是集合大小），不是无界增长。
fn target_lock(target: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let registry = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.entry(target.to_path_buf()).or_default().clone()
}

// ---------------------------------------------------------------------------
// 重定向：手工逐跳（§6.1 第 2 条）
// ---------------------------------------------------------------------------

/// 一次跳转的校验：scheme 必须是 `https`（测试策略下才允许明文），host 必须在**生效**
/// 白名单（编译期常量 ∪ 调用方显式传入的扩展）内。
///
/// **初始 URL 与每一跳都走这一条**（[`download_verified_with`] 先调它一次，再在
/// [`follow_redirect`] 里对每个 `Location` 调一次）：这就是"逐跳校验"的实现，
/// 也是"白名单不因为跟随重定向而被放宽"的结构性保证。
fn validate_hop(url: &reqwest::Url, policy: &DownloadPolicy<'_>, allowed: &[&str]) -> Result<()> {
    if !policy.scheme_is_acceptable(url.scheme()) {
        return Err(DownloadError::SchemeRejected {
            scheme: url.scheme().to_string(),
        }
        .into());
    }
    let host = url.host_str().ok_or_else(|| {
        RapidOcrError::from(DownloadError::Network {
            detail: format!("`{url}` has no host"),
        })
    })?;
    if !host_is_allowed(host, allowed) {
        return Err(DownloadError::HostRejected {
            host: host.to_string(),
        }
        .into());
    }
    Ok(())
}

/// 发一个 GET（唯一的请求形状：自己的 `User-Agent` + `Referer`，**不带任何凭据**）。
///
/// 重定向的每一个 hop 都经过这里，因此"初始请求"与"跳转请求"的形状不可能漂移；
/// 也正因为形状是这一份，`Authorization`/token 头**在类型上**没有来源
/// （见 `a_redirect_target_receives_no_credential_header`）。
fn send_request(
    client: &Client,
    url: &str,
    req: &DownloadRequest<'_>,
) -> Result<reqwest::blocking::Response> {
    let send_started = Instant::now();
    client
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (compatible; rapid-ocr-rs model downloader)",
        )
        .header(reqwest::header::REFERER, "https://www.modelscope.cn/")
        .send()
        .map_err(|error| {
            classify_send_error(
                error,
                send_started.elapsed(),
                req.connect_timeout,
                req.read_timeout,
            )
        })
}

/// [`send_request`] 的封装：收到 3xx 时**手工**跟随，最多 [`MAX_REDIRECT_HOPS`] 跳。
///
/// # 规则（每一条都是可定位的错误，而不是静默行为）
///
/// 1. **不盲从**：客户端是 `Policy::none()`，因此这里看到的每个 3xx 都是上游的原始应答；
/// 2. **逐跳校验**：`Location` 先按当前 URL 解析（相对引用合法），然后与初始 URL 同样
///    判定 scheme（非 `https` → [`DownloadError::SchemeRejected`]）与 host
///    （越界 → [`DownloadError::HostRejected`]，错误里带**那个** host）；
/// 3. **不把跳转目标的 URL 当成落盘名字**：落盘名字来自**初始 URL** 的末段（在进入这里之前
///    就已经确定并校验过），跳转只改变"从哪里取字节"。这不是"少校验一条"，而是**唯一正确的
///    语义**：真实 CDN 用的是 LFS 对象路径
///    （`https://cdn-lfs-cn-1.modelscope.cn/prod/lfs-objects/f4/2c/0fbd…?filename=PP-OCRv6_det_tiny.onnx`），
///    末段是对象哈希而不是文件名——把"末段必须等于文件名"当规则会让**所有权重**都下载不了
///    （M2b 实测），而它对安全性没有任何贡献：写盘路径与必须匹配的内容都由初始 URL +
///    模型表声明的 SHA-256 决定（§6.4），跳转改变不了其中任何一个；
/// 4. **跳数上界**：已经跟随了 [`MAX_REDIRECT_HOPS`] 跳之后还收到 3xx →
///    [`DownloadError::RedirectRejected`]（`location` 是那个 `Location` 原文）；
/// 5. **3xx 没有 `Location`** → [`DownloadError::RedirectRejected { location: None }]`；
/// 6. **非 3xx 一律不再跟随**：2xx 返回给调用方（长度预检/流式上限/哈希校验照旧），
///    4xx/5xx 也会被返回，由调用方按"非 2xx"报 [`DownloadError::Network`]；
/// 7. **凭据不跨跳**：每个 hop 都用 [`send_request`] 的固定形状（无 `Authorization`、
///    无 cookie、无自定义头）；
/// 8. **超时按跳计**：每跳各自受 `connect_timeout` / `read_timeout` 约束，与
///    "阻塞等待计时而不是整次传输计时"的既有口径一致；一个停在 3xx 上不推进的链，
///    最坏情况是 `MAX_REDIRECT_HOPS` 个连接/读取预算，然后以跳数上界失败。
fn follow_redirect(
    client: &Client,
    mut response: reqwest::blocking::Response,
    initial: &reqwest::Url,
    policy: &DownloadPolicy<'_>,
    req: &DownloadRequest<'_>,
) -> Result<reqwest::blocking::Response> {
    let mut current = initial.clone();
    let mut hops = 0_usize;
    loop {
        if !response.status().is_redirection() {
            return Ok(response);
        }
        let next_location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let Some(next_location) = next_location else {
            return Err(DownloadError::RedirectRejected { location: None }.into());
        };
        if hops >= MAX_REDIRECT_HOPS {
            return Err(DownloadError::RedirectRejected {
                location: Some(next_location),
            }
            .into());
        }
        // 相对 `Location` 按**当前** URL 解析（RFC 9110：Location 可以是相对引用）。
        let next = current.join(&next_location).map_err(|error| {
            RapidOcrError::from(DownloadError::Network {
                detail: format!(
                    "the model host answered with an unusable `Location` value \
                     `{next_location}` (from {current}): {error}"
                ),
            })
        })?;
        validate_hop(&next, policy, req.allowed_hosts)?;
        // 上一跳的响应体在这里被丢弃（`Response` 的 `Drop`），再发下一跳。
        drop(response);
        response = send_request(client, next.as_str(), req)?;
        current = next;
        hops += 1;
    }
}

fn download_verified_with(
    req: &DownloadRequest<'_>,
    policy: &DownloadPolicy<'_>,
    observer: &mut dyn DownloadObserver,
) -> Result<PathBuf> {
    // §6.4：哈希必填。空哈希是调用方的契约错误（类型已经不允许 `None`），
    // 在碰网络或文件系统之前失败。
    let expected = req.expected_sha256.trim();
    if expected.is_empty() {
        return Err(RapidOcrError::Config(
            "download_verified requires a concrete sha256; got an empty string".to_string(),
        ));
    }

    // 1/3：scheme 与 host 的判定在**任何**副作用之前（§6.1 第 1、3 条）。
    // 重定向的**每一跳**都复用同一条判定（[`validate_hop`]），因此这里不是"只查初始 URL"。
    let url = reqwest::Url::parse(req.url).map_err(|error| {
        RapidOcrError::from(DownloadError::Network {
            detail: format!("`{}` is not a valid URL: {error}", req.url),
        })
    })?;
    validate_hop(&url, policy, req.allowed_hosts)?;

    fs::create_dir_all(req.save_dir)?;
    let file_name = extract_file_name(req.url)?;
    // 同一条路径安全的唯一实现：拒绝 `..`、分隔符、盘符与空名。
    crate::model_set::validate_model_file_name(&file_name).map_err(|error| {
        RapidOcrError::ModelResolve(format!(
            "cannot derive a safe file name for `{}`: {error}",
            req.url
        ))
    })?;
    let target = req.save_dir.join(&file_name);

    // §6.1 第 8 条：同目标单飞。第二个调用者在这里等待，然后命中上一步写好的文件。
    let lock = target_lock(&target);
    let _single_flight = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    // §6.3：已存在且哈希正确 → 不下载；已存在但损坏 → 覆盖（下面的原子替换）。
    if target.is_file() {
        let actual = sha256_file(&target)?;
        if actual.eq_ignore_ascii_case(expected) {
            return Ok(target);
        }
    }

    let client = Client::builder()
        .redirect(Policy::none())
        .connect_timeout(req.connect_timeout)
        .timeout(req.read_timeout)
        .build()
        .map_err(|error| {
            RapidOcrError::from(DownloadError::Network {
                detail: format!("cannot build the HTTP client: {error}"),
            })
        })?;

    // §6.1 第 1、3 条 + 本模块文档的"手工逐跳重定向"：初始请求与所有跳转都经过
    // `send_request`（同一个请求形状）与 `validate_hop`（同一条 scheme/host 判定）。
    let response = send_request(&client, req.url, req)?;
    let response = follow_redirect(&client, response, &url, policy, req)?;

    let status = response.status();
    if !status.is_success() {
        return Err(DownloadError::Network {
            detail: format!("the model host answered HTTP {status} for {}", req.url),
        }
        .into());
    }

    // §6.1 第 4 条：长度预检在**创建临时文件之前**。
    let declared_length = response.content_length();
    if let Some(length) = declared_length
        && length > req.max_bytes
    {
        return Err(DownloadError::TooLarge {
            limit_bytes: req.max_bytes,
            observed_bytes: Some(length),
        }
        .into());
    }

    // §6.5：长度未知时按 `max_bytes` 计入需求（宁可提前拒绝，也不写到半个文件）。
    let required = declared_length.unwrap_or(req.max_bytes);
    let available = policy.free_space.available_bytes(req.save_dir)?;
    if available < required {
        return Err(DownloadError::InsufficientSpace {
            required_bytes: required,
            available_bytes: available,
        }
        .into());
    }

    let part_path = unique_part_path(&target);
    let mut part = PartFile::create(&part_path)?;
    let mut hasher = Sha256::new();
    let mut limited = response.take(req.max_bytes.saturating_add(1));
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let read = limited
            .read(&mut buffer)
            .map_err(|error| classify_read_error(error, req.read_timeout))?;
        if read == 0 {
            break;
        }
        written += read as u64;
        // §6.1 第 5 条：`take(max + 1)` 最多多读 1 字节，因此这里一旦超限就立刻停下。
        if written > req.max_bytes {
            return Err(DownloadError::TooLarge {
                limit_bytes: req.max_bytes,
                observed_bytes: Some(written),
            }
            .into());
        }
        hasher.update(&buffer[..read]);
        part.write_all(&buffer[..read])?;
        // 进度：只报告**当前文件**的累计字节（§4.3 的 bytes done/total 由 serve 侧换算）。
        observer.bytes_written(written);
    }

    // §6.1 第 9 条：哈希不符 → 删除临时文件（由 `PartFile` 的 `Drop` 完成）。
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(DownloadError::HashMismatch {
            expected: expected.to_string(),
            actual,
        }
        .into());
    }

    // §6.3/§6.7：校验通过之后才做原子替换；替换失败时原文件不动、临时文件被清掉。
    part.replace(&target)?;
    Ok(target)
}

/// host 是否在可信白名单内。
///
/// 比较是**整串精确**匹配（大小写不敏感），不做后缀/子域放宽：`www.modelscope.cn.evil.com`
/// 与 `evil-www.modelscope.cn` 都不等于 `www.modelscope.cn`。端口不参与比较
/// （白名单的语义是"哪个站点"，而不是"哪个端口"）。
fn host_is_allowed(host: &str, allowed: &[&str]) -> bool {
    allowed
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(host))
}

fn timeout_ms(timeout: Duration) -> u64 {
    u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)
}

/// 发送阶段（连接 + 等待响应头）失败 → 下载错误分类。
///
/// reqwest 0.12 的 blocking 客户端**不区分**"连接阶段超时"与"等待响应超时"：
/// 两者都是 `Kind::Request` + `TimedOut`，而 `is_connect()` 对连接**超时**为 `false`
/// （只对连接**错误**，例如目标拒绝连接，为 `true`）。因此这里用**实测耗时**判定：
/// 连接预算先到期（耗时未达到读取预算）→ [`DownloadError::ConnectTimeout`]；
/// 否则是等待响应的读取超时 → [`DownloadError::ReadTimeout`]。
///
/// 前提是 `connect_timeout < read_timeout`（库内默认 10s / 30s，serve 层同样如此）。
/// 两者相等或反置时该判定退化为"报告读取超时"，不会误报连接超时。
fn classify_send_error(
    error: reqwest::Error,
    elapsed: Duration,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> RapidOcrError {
    if !error.is_timeout() {
        return DownloadError::Network {
            detail: error.to_string(),
        }
        .into();
    }
    classify_send_timeout(error.is_connect(), elapsed, connect_timeout, read_timeout).into()
}

/// [`classify_send_error`] 的判定本身（与 `reqwest::Error` 解耦，因此两个分支都能单测）。
///
/// 规则：`reqwest` 明确报告连接错误（`is_connect`）**或**耗时还没到读取预算
/// → 是连接预算先到期，报 [`DownloadError::ConnectTimeout`] 并给出连接预算；
/// 否则是等待响应的读取超时，报 [`DownloadError::ReadTimeout`] 并给出读取预算。
fn classify_send_timeout(
    is_connect: bool,
    elapsed: Duration,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> DownloadError {
    if is_connect || elapsed < read_timeout {
        DownloadError::ConnectTimeout {
            timeout_ms: timeout_ms(connect_timeout),
        }
    } else {
        DownloadError::ReadTimeout {
            timeout_ms: timeout_ms(read_timeout),
        }
    }
}

/// 读取响应体失败 → 下载错误分类。
///
/// reqwest 的 blocking `Read` 把超时包装成 `io::Error`（内层是 `reqwest::Error`），
/// 因此先尝试取出内层错误再判定超时。
fn classify_read_error(error: std::io::Error, read_timeout: Duration) -> RapidOcrError {
    let timed_out = match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
    {
        Some(inner) => inner.is_timeout(),
        None => error.kind() == std::io::ErrorKind::TimedOut,
    };
    if timed_out {
        DownloadError::ReadTimeout {
            timeout_ms: timeout_ms(read_timeout),
        }
        .into()
    } else {
        DownloadError::Network {
            detail: error.to_string(),
        }
        .into()
    }
}

// ---------------------------------------------------------------------------
// 临时文件与原子替换
// ---------------------------------------------------------------------------

/// 进程内的临时文件序号，保证 `.part-<pid>-<seq>` 唯一（§6.1 第 6 条）。
static PART_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 目标文件对应的唯一临时路径。
///
/// 旧实现在这里用 `target_path.with_extension("part")`，那是一个**固定**名字：
/// 两次并发或一次崩溃残留会让两个下载互相覆盖。现在每次调用都取一个新的序号。
fn unique_part_path(target: &Path) -> PathBuf {
    let sequence = PART_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!("{name}.part-{}-{sequence}", std::process::id()))
}

/// 正在写入的临时文件：**任何**提前返回都会在 `Drop` 里删掉它。
struct PartFile {
    path: PathBuf,
    file: Option<fs::File>,
    keep: bool,
}

impl PartFile {
    fn create(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            file: Some(fs::File::create(path)?),
            keep: false,
        })
    }

    fn write_all(&mut self, buffer: &[u8]) -> Result<()> {
        self.file_mut().write_all(buffer)?;
        Ok(())
    }

    fn file_mut(&mut self) -> &mut fs::File {
        self.file
            .as_mut()
            .expect("the temporary file is open until it is replaced")
    }

    /// 落盘（`sync_all`）后原子替换 `target`；失败时临时文件仍会被 `Drop` 删除。
    fn replace(&mut self, target: &Path) -> Result<()> {
        self.file_mut().flush()?;
        self.file_mut().sync_all()?;
        // 先关掉句柄：Windows 上删除/替换一个仍被本进程打开的文件需要 FILE_SHARE_DELETE，
        // 而 `File::create` 没有请求它。
        drop(self.file.take());
        replace_file(&self.path, target)?;
        self.keep = true;
        Ok(())
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

/// `MoveFileExW` 的 `dwFlags`：目标已存在时覆盖。
const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
/// `MoveFileExW` 的 `dwFlags`：等待写入落到磁盘后再返回。
const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
    fn GetDiskFreeSpaceExW(
        directory_name: *const u16,
        free_bytes_available_to_caller: *mut u64,
        total_number_of_bytes: *mut u64,
        total_number_of_free_bytes: *mut u64,
    ) -> i32;
    fn GetLastError() -> u32;
}

/// UTF-16、以 NUL 结尾的宽字符串（Win32 `*W` API 的入参形式）。
fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    let mut buffer: Vec<u16> = path.as_os_str().encode_wide().collect();
    buffer.push(0);
    buffer
}

/// §6.7 的原子替换：`MoveFileExW(src, dst, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`。
///
/// 失败时返回带 Win32 错误码的可定位错误，并**保证目标文件没有被改动**
/// （`MoveFileExW` 失败时不会部分覆盖目标）；临时文件由调用方的 `PartFile` 删除。
fn replace_file(source: &Path, target: &Path) -> Result<()> {
    let source_wide = wide(source);
    let target_wide = wide(target);
    // SAFETY: both buffers are NUL-terminated UTF-16 owned by this frame; the flags are
    // the documented constants; `MoveFileExW` touches nothing else.
    let ok = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        // SAFETY: `GetLastError` has no preconditions and is read immediately.
        let code = unsafe { GetLastError() };
        return Err(RapidOcrError::Io(std::io::Error::other(format!(
            "MoveFileExW({} -> {}) failed with Win32 error {code}; the existing file was left \
             untouched",
            source.display(),
            target.display()
        ))));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 磁盘可用空间（可注入，便于单测）
// ---------------------------------------------------------------------------

/// 目标目录所在卷的可用字节数（§6.5 的**任务级**空间核算）。
///
/// 生产实现就是 [`DownloadPolicy`] 内部用的同一个 `GetDiskFreeSpaceExW` 探测
/// （[`WindowsFreeSpace`]），因此"下载前的任务级预检"与"每个文件写入前的预检"
/// 是同一个事实，不存在两套口径。调用方（serve 的下载端点）用它把
/// `InsufficientSpace { required_bytes, available_bytes }` 里的两个数值都拿到手。
pub fn available_disk_bytes(directory: impl AsRef<Path>) -> Result<u64> {
    WindowsFreeSpace.available_bytes(directory.as_ref())
}

/// 可用磁盘空间来源。
///
/// 生产实现是 `GetDiskFreeSpaceExW`；单测注入固定值，否则"磁盘不足"这条分支只能靠
/// 真的填满磁盘来验证（§6.1 第 10 条要求这条分支可单测）。
trait FreeSpaceProbe {
    fn available_bytes(&self, directory: &Path) -> Result<u64>;
}

/// 生产实现：`GetDiskFreeSpaceExW(directory, &mut available, null, null)`。
struct WindowsFreeSpace;

impl FreeSpaceProbe for WindowsFreeSpace {
    fn available_bytes(&self, directory: &Path) -> Result<u64> {
        let directory_wide = wide(directory);
        let mut available: u64 = 0;
        // SAFETY: `directory_wide` is a NUL-terminated UTF-16 path; `available` is a live
        // `u64` for the duration of the call; the two remaining out-parameters are optional
        // (`null` is explicitly allowed by the API).
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                directory_wide.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // SAFETY: `GetLastError` has no preconditions and is read immediately.
            let code = unsafe { GetLastError() };
            return Err(RapidOcrError::Io(std::io::Error::other(format!(
                "GetDiskFreeSpaceExW({}) failed with Win32 error {code}",
                directory.display()
            ))));
        }
        Ok(available)
    }
}

/// 从下载 URL 推导落盘文件名（唯一实现）。
///
/// `pub(crate)`：默认模型表也用同一个推导，否则"表里写的文件名"与
/// "下载器实际落盘的文件名"会漂移，逐文件状态校验就会指向不存在的路径。
pub(crate) fn extract_file_name(url: &str) -> Result<String> {
    let trimmed = url.split('?').next().unwrap_or(url);
    let file_name = trimmed
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            RapidOcrError::ModelResolve(format!("cannot extract a file name from url: {url}"))
        })?;
    Ok(file_name.to_string())
}

pub fn sha256_file(path: impl AsRef<Path>) -> Result<String> {
    let mut file = fs::File::open(path.as_ref())?;
    let mut hasher = Sha256::new();
    // 缓冲区放在堆上：Windows 主线程默认只有 1 MiB 栈，1 MiB 的栈数组会在
    // 调用方（CLI/benchmark）直接触发 stack overflow。
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs,
        path::Path,
        sync::{Arc, Barrier, Mutex},
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::{
        model_set::{ModelFileSpec, ModelRole, ModelSet},
        model_source::{MANIFEST_FILE_NAME, ModelRequest, ModelSource},
        test_support::{FixtureResponse, HttpFixture, TempDir},
    };

    /// 测试用的一次性哈希（64 个十六进制字符，形状与真实哈希一致）。
    const UNMATCHED_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    /// 单测的下载白名单：只有本机 fixture 服务器。
    const FIXTURE_HOSTS: [&str; 1] = ["127.0.0.1"];

    /// 独立的 SHA-256（不经过被测的 `sha256_file`），用于构造期望值。
    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    struct FixedFreeSpace(u64);

    impl FreeSpaceProbe for FixedFreeSpace {
        fn available_bytes(&self, _directory: &Path) -> Result<u64> {
            Ok(self.0)
        }
    }

    fn huge_free_space() -> FixedFreeSpace {
        FixedFreeSpace(1 << 40)
    }

    /// 指向本机 fixture 的下载策略：放行明文 `http://`（生产构建里没有这个口子，
    /// 见 [`DownloadPolicy`] 的文档）。**白名单不在这里**：它是
    /// [`DownloadRequest::allowed_hosts`] 的一部分，测试用 [`FIXTURE_HOSTS`]
    /// 作为那个**显式参数**（正好也证明了它是可替换的）。
    fn fixture_policy<'a>(free_space: &'a dyn FreeSpaceProbe) -> DownloadPolicy<'a> {
        DownloadPolicy {
            free_space,
            insecure_http: true,
        }
    }

    fn fixture_request<'a>(
        url: &'a str,
        expected: &'a str,
        dir: &'a Path,
        max_bytes: u64,
    ) -> DownloadRequest<'a> {
        DownloadRequest {
            url,
            expected_sha256: expected,
            save_dir: dir,
            max_bytes,
            connect_timeout: Duration::from_secs(5),
            read_timeout: Duration::from_secs(5),
            allowed_hosts: &FIXTURE_HOSTS,
        }
    }

    /// 目录里的全部条目名（排序），用于断言"没有残留的临时文件"。
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("the download directory must be readable")
            .map(|entry| {
                entry
                    .expect("a readable directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn download_error(error: RapidOcrError) -> DownloadError {
        match error {
            RapidOcrError::Download(error) => error,
            other => panic!("expected a download error, got {other:?}"),
        }
    }

    fn set_file(name: &str, url: &str, sha256: &str, size_bytes: Option<u64>) -> ModelFileSpec {
        ModelFileSpec::new(name, ModelRole::Detector, size_bytes, sha256, url)
            .expect("the test file spec must be valid")
    }

    fn model_set(files: Vec<ModelFileSpec>) -> ModelSet {
        ModelSet {
            id: "test-set".to_string(),
            family: "test".to_string(),
            version: "v1".to_string(),
            files,
        }
    }

    /// 集合下载的测试入口：策略指向本机 fixture（生产入口只认编译期白名单 + 仅 HTTPS）。
    fn download_set(
        set: &ModelSet,
        root: &Path,
        budget: &mut DownloadBudget,
    ) -> Result<Vec<PathBuf>> {
        download_model_set_with(
            set,
            root,
            budget,
            Duration::from_secs(5),
            Duration::from_secs(5),
            &FIXTURE_HOSTS,
            &mut NoObserver,
            &fixture_policy(&huge_free_space()),
        )
    }

    #[test]
    fn sha256_file_hashes_contents_without_loading_api_changes() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rapid-ocr-rs-sha256-{}-{suffix}.tmp",
            std::process::id()
        ));
        fs::write(&path, b"hello").expect("test fixture should be writable");

        let actual = sha256_file(&path).expect("hash should succeed");

        fs::remove_file(&path).expect("test fixture should be removable");
        assert_eq!(
            actual,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    // -----------------------------------------------------------------------
    // 白名单（§6.1 第 3 条）
    // -----------------------------------------------------------------------

    /// 白名单是**编译期**常量：新增一项必须改这个常量，因此必然让这条测试失败，
    /// 从而强制一次显式审查。清单与 CLI 都不能绕过它。
    ///
    /// M2b：第二项（`cdn-lfs-cn-1.modelscope.cn`）是 ModelScope 权重 302 的**目标**，
    /// 它进入白名单是"逐跳校验"能成立的前提——注意它只作为 `Location` 目标出现，
    /// 内容仍然必须匹配默认表声明的 SHA-256。
    #[test]
    fn the_allowed_download_hosts_are_exactly_the_declared_set() {
        assert_eq!(
            ALLOWED_DOWNLOAD_HOSTS,
            ["www.modelscope.cn", "cdn-lfs-cn-1.modelscope.cn"],
            "widening the download host allow-list requires changing the constant and this test"
        );
        assert_eq!(ALLOWED_DOWNLOAD_HOSTS.len(), 2);

        // 每一项都必须是"裸主机名"：带 scheme/端口/路径的写法会让 host 比较永远不成立
        // （那是一个静默失效的白名单）。
        for host in ALLOWED_DOWNLOAD_HOSTS {
            assert!(
                !host.is_empty()
                    && !host.contains(['/', ':', '@', '?', '#'])
                    && !host.starts_with('.')
                    && host.contains('.'),
                "`{host}` must be a bare host name"
            );
        }

        assert!(host_is_allowed(
            "www.modelscope.cn",
            &ALLOWED_DOWNLOAD_HOSTS
        ));
        assert!(host_is_allowed(
            "cdn-lfs-cn-1.modelscope.cn",
            &ALLOWED_DOWNLOAD_HOSTS
        ));
        assert!(
            host_is_allowed("WWW.ModelScope.CN", &ALLOWED_DOWNLOAD_HOSTS),
            "host comparison must be case-insensitive"
        );
        assert!(
            host_is_allowed("CDN-LFS-CN-1.ModelScope.CN", &ALLOWED_DOWNLOAD_HOSTS),
            "host comparison must be case-insensitive"
        );
        for host in [
            "modelscope.cn",
            "www.modelscope.cn.evil.example",
            "evil-www.modelscope.cn",
            "www.modelscope.com",
            "wwwxmodelscope.cn",
            // CDN host 的**兄弟**不能被后缀放宽放进来：白名单是逐串精确比较。
            "cdn-lfs-cn-2.modelscope.cn",
            "cdn-lfs-cn-1.modelscope.cn.evil.example",
            "evil-cdn-lfs-cn-1.modelscope.cn",
            "127.0.0.1",
            "localhost",
            "",
        ] {
            assert!(
                !host_is_allowed(host, &ALLOWED_DOWNLOAD_HOSTS),
                "`{host}` must not pass the download host allow-list"
            );
        }
    }

    /// 本地清单可以提供 URL，但**不能扩大**白名单（OWASP：白名单来自可信配置）。
    #[test]
    fn a_local_manifest_cannot_widen_the_download_host_allow_list() {
        let dir = TempDir::new("download-manifest-widen");
        let manifest = format!(
            r#"{{
              "schema_version": 1,
              "id": "shadow",
              "family": "test",
              "version": "v1",
              "files": [
                {{ "name": "pp_formulanet_plus_m.onnx", "role": "formula_recognizer",
                   "sha256": "{UNMATCHED_HASH}", "size_bytes": 593915961,
                   "source_url": "https://evil.example/pp_formulanet_plus_m.onnx" }}
              ]
            }}"#
        );
        dir.write(MANIFEST_FILE_NAME, manifest.as_bytes());

        let source = ModelSource::select(dir.path()).expect("a well-formed manifest must load");
        let sets = source
            .model_sets(&ModelRequest::formula_only())
            .expect("the manifest declares the required formula role");
        let spec = &sets[0].files[0];
        assert_eq!(
            spec.source_url,
            "https://evil.example/pp_formulanet_plus_m.onnx"
        );

        let request = DownloadRequest {
            url: &spec.source_url,
            expected_sha256: &spec.sha256,
            save_dir: dir.path(),
            max_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
            // 公开入口的默认：只有编译期白名单，清单里的 host 不在里面。
            allowed_hosts: DEFAULT_ALLOWED_HOSTS,
        };
        let error = download_verified(&request)
            .expect_err("a manifest must not widen the compiled allow-list");
        match download_error(error) {
            DownloadError::HostRejected { host } => assert_eq!(host, "evil.example"),
            other => panic!("expected HostRejected, got {other:?}"),
        }
    }

    /// 走**公开**入口（生产策略：编译期白名单 + 仅 HTTPS）的三条前置拒绝，
    /// 全部在任何文件系统副作用之前发生。
    #[test]
    fn the_public_entry_point_rejects_scheme_and_host_before_any_side_effect() {
        let dir = TempDir::new("download-public-rejects");
        let save_dir = dir.path().join("models");

        let cases: [(&str, &str); 3] = [
            ("http://www.modelscope.cn/models/x.onnx", "scheme"),
            ("https://evil.example/models/x.onnx", "host"),
            (
                "https://www.modelscope.cn.evil.example/models/x.onnx",
                "host",
            ),
        ];
        for (url, expected) in cases {
            let request = DownloadRequest {
                url,
                expected_sha256: UNMATCHED_HASH,
                save_dir: &save_dir,
                max_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
                connect_timeout: DEFAULT_CONNECT_TIMEOUT,
                read_timeout: DEFAULT_READ_TIMEOUT,
                // 生产默认：编译期白名单本身（没有任何显式扩展）。
                allowed_hosts: DEFAULT_ALLOWED_HOSTS,
            };
            let error = download_verified(&request).expect_err("the URL must be rejected");
            match (expected, download_error(error)) {
                ("scheme", DownloadError::SchemeRejected { scheme }) => assert_eq!(scheme, "http"),
                ("host", DownloadError::HostRejected { .. }) => {}
                (_, other) => panic!("unexpected error for {url}: {other:?}"),
            }
            assert!(
                !save_dir.exists(),
                "a rejected URL must not create the model directory"
            );
        }
    }

    /// 空哈希是调用方的契约错误：类型已经不允许 `None`，空串同样必须在任何 I/O 之前拒绝。
    #[test]
    fn an_empty_expected_hash_is_refused_before_any_io() {
        let dir = TempDir::new("download-empty-hash");
        let save_dir = dir.path().join("models");
        let request = DownloadRequest {
            url: "https://www.modelscope.cn/models/x.onnx",
            expected_sha256: "   ",
            save_dir: &save_dir,
            max_bytes: DEFAULT_MAX_DOWNLOAD_BYTES,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
            allowed_hosts: DEFAULT_ALLOWED_HOSTS,
        };
        let error = download_verified(&request).expect_err("an empty hash must be refused");
        match error {
            RapidOcrError::Config(message) => assert!(message.contains("sha256"), "{message}"),
            other => panic!("expected a config error, got {other:?}"),
        }
        assert!(!save_dir.exists());
    }

    #[test]
    fn require_model_hash_rejects_a_missing_or_empty_hash() {
        assert_eq!(
            require_model_hash(Some("abc"), "det.onnx").expect("a concrete hash passes"),
            "abc"
        );
        for missing in [None, Some(""), Some("   ")] {
            let error = require_model_hash(missing, "det.onnx")
                .expect_err("a missing hash must not become an unverified download");
            let message = error.to_string();
            assert!(message.contains("det.onnx"), "{message}");
            assert!(message.contains("no SHA-256"), "{message}");
        }
    }

    // -----------------------------------------------------------------------
    // 传输、上限与清理（§6.1 第 1、2、4、5、6、9 条）
    // -----------------------------------------------------------------------

    #[test]
    fn a_verified_download_writes_the_file_and_leaves_no_temp_file() {
        let body = b"rapid-ocr-rs model bytes".to_vec();
        let expected = sha256_hex(&body);
        let server = HttpFixture::start(move |_| FixtureResponse::ok(body.clone()));
        let dir = TempDir::new("download-ok");
        let url = server.url("/model.onnx");

        let path = download_verified_with(
            &fixture_request(&url, &expected, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect("the download must succeed");

        assert_eq!(path, dir.path().join("model.onnx"));
        assert_eq!(
            fs::read(&path).expect("the downloaded file must be readable"),
            b"rapid-ocr-rs model bytes"
        );
        assert_eq!(
            entries(dir.path()),
            vec!["model.onnx".to_string()],
            "no temporary file may survive a successful download"
        );
        assert_eq!(server.request_count(), 1);
    }

    /// 请求形状：GET、带自己的 User-Agent、请求行里就是模型路径。
    #[test]
    fn the_downloader_issues_a_plain_get_with_its_own_user_agent() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let body = b"model".to_vec();
        let expected = sha256_hex(&body);
        let server = HttpFixture::start(move |request| {
            recorder.lock().expect("the recorder lock").push((
                request.index,
                request.method.clone(),
                request.target.clone(),
                request.header("user-agent").map(str::to_string),
            ));
            FixtureResponse::ok(body.clone())
        });
        let dir = TempDir::new("download-request-shape");
        let url = server.url("/model.onnx");

        download_verified_with(
            &fixture_request(&url, &expected, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect("the download must succeed");

        let recorded = seen.lock().expect("the recorder lock");
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, 0, "the fixture numbers connections from 0");
        assert_eq!(recorded[0].1, "GET");
        assert_eq!(recorded[0].2, "/model.onnx");
        assert!(
            recorded[0]
                .3
                .as_deref()
                .is_some_and(|value| value.contains("rapid-ocr-rs")),
            "the downloader must identify itself: {:?}",
            recorded[0].3
        );
    }

    #[test]
    fn a_hash_mismatch_deletes_the_temp_file_and_leaves_no_target() {
        let body = b"actual bytes".to_vec();
        let server = HttpFixture::start(move |_| FixtureResponse::ok(body.clone()));
        let dir = TempDir::new("download-hash");
        let url = server.url("/model.onnx");

        let error = download_verified_with(
            &fixture_request(&url, UNMATCHED_HASH, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("a hash mismatch must fail");

        match download_error(error) {
            DownloadError::HashMismatch { expected, actual } => {
                assert_eq!(expected, UNMATCHED_HASH);
                assert_eq!(actual, sha256_hex(b"actual bytes"));
            }
            other => panic!("expected HashMismatch, got {other:?}"),
        }
        assert!(!dir.path().join("model.onnx").exists());
        assert!(
            entries(dir.path()).is_empty(),
            "the temporary file must be deleted: {:?}",
            entries(dir.path())
        );
    }

    // -----------------------------------------------------------------------
    // 重定向：手工逐跳（§6.1 第 2 条）
    //
    // 这些用例全部在环回地址上：**零公网**。两个 listener 分别扮演"来源主机"与"CDN"，
    // 因此"跳到另一个 host 并成功"与"跳到白名单外的 host 被拒"都是真实的两台服务器。
    // -----------------------------------------------------------------------

    /// 静态断言：跟随上限是一个**小**数（跳数越多越像"盲从"），并且断言 1 跳。
    #[test]
    fn the_redirect_hop_limit_is_small_and_fixed() {
        assert_eq!(
            MAX_REDIRECT_HOPS, 5,
            "changing the hop limit is a security decision; update docs/05 §6.1 item 2 as well"
        );
        const {
            assert!(MAX_REDIRECT_HOPS >= 1);
            assert!(MAX_REDIRECT_HOPS <= 8);
        }
    }

    /// 1) 跳到**允许的** host（另一台 fixture 服务器）→ 成功，落盘文件的哈希匹配。
    ///
    /// 这条用例同时覆盖"相对 `Location` 的解析"：第一跳用绝对 URL 换到第二台服务器，
    /// 第二台再发一个**相对** `Location`（`/mirror/model.onnx`，只有路径），下载器必须把
    /// 它解析成"当前服务器上的那个路径"，而不是当成绝对 URL 或失败。
    #[test]
    fn a_redirect_to_an_allowed_host_is_followed_and_verified() {
        let body = b"rapid-ocr-rs weight bytes behind a 302".to_vec();
        let expected = sha256_hex(&body);
        // 第二台服务器：`/cdn/model.onnx` 回一个**相对**引用（RFC 9110 允许），
        // `/mirror/model.onnx`（或任何别的路径）才给内容。相对引用只能解析成
        // "当前 URL 上的 /mirror/…"，也就是这台服务器自己——而不是来源主机的同名路径。
        let middle_body = body.clone();
        let middle = HttpFixture::start(move |request| match request.target.as_str() {
            "/cdn/model.onnx" => FixtureResponse::redirect("/mirror/model.onnx"),
            _ => FixtureResponse::ok(middle_body.clone()),
        });
        let middle_addr = middle.addr();
        // 一台**从未被访问**的服务器：用来证明"每一跳都真的落在解析出来的 host 上"，
        // 而不是被哪个兜底路径顺手取走。
        let untouched = HttpFixture::start(|_| FixtureResponse::ok(b"never".to_vec()));
        // 第一跳：绝对 URL，指向另一台（仍在白名单内的）服务器。
        let entry = HttpFixture::start(move |_| {
            FixtureResponse::redirect(&format!("http://{middle_addr}/cdn/model.onnx"))
        });
        let dir = TempDir::new("download-redirect-allowed");
        let url = entry.url("/model.onnx");

        let path = download_verified_with(
            &fixture_request(&url, &expected, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect("a redirect inside the allow-list must be followed");

        assert_eq!(path, dir.path().join("model.onnx"));
        assert_eq!(
            sha256_file(&path).expect("hash the landed file"),
            expected,
            "the redirected body must still match the declared SHA-256"
        );
        assert_eq!(
            entries(dir.path()),
            vec!["model.onnx".to_string()],
            "no temporary file may survive a redirected download"
        );
        // 证据强度：每台该被访问的服务器都真的被访问过（入口 1 次 + 中间站 2 次）。
        assert_eq!(entry.request_count(), 1);
        assert_eq!(middle.request_count(), 2);
        assert_eq!(untouched.request_count(), 0);
    }

    /// 2) 跳到**白名单之外**的 host → `HostRejected`（指名那个 host），**一个字节都不写**。
    ///
    /// `localhost:{port}` 是同一台 fixture 服务器的另一个名字：它**可解析**（因此若是
    /// "盲从"就会真的把内容拿回来），唯一的问题是它不在生效白名单里。这正是
    /// `Policy::limited(n)` 的失败模式，因此这条用例锁住"不盲从"。
    #[test]
    fn a_redirect_to_a_host_outside_the_allow_list_is_rejected() {
        let worker = HttpFixture::start(|_| FixtureResponse::ok(b"must never be fetched".to_vec()));
        let port = worker.addr().port();
        let origin = HttpFixture::start(move |_| {
            FixtureResponse::redirect(&format!("http://localhost:{port}/model.onnx"))
        });
        let dir = TempDir::new("download-redirect-off-list");
        let url = origin.url("/model.onnx");

        let error = download_verified_with(
            &fixture_request(&url, UNMATCHED_HASH, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("a redirect outside the allow-list must be rejected");

        match download_error(error) {
            DownloadError::HostRejected { host } => assert_eq!(host, "localhost"),
            other => panic!("expected HostRejected, got {other:?}"),
        }
        assert!(
            entries(dir.path()).is_empty(),
            "a rejected hop must not write anything: {:?}",
            entries(dir.path())
        );
        assert_eq!(
            origin.request_count(),
            1,
            "the origin is asked once; the off-list host never is"
        );
        assert_eq!(
            worker.request_count(),
            0,
            "the downloader must not even connect to the off-list host"
        );
    }

    /// 3) 链长超过 [`MAX_REDIRECT_HOPS`] → `RedirectRejected`（带那个 `Location`），不写文件。
    ///
    /// 服务器把 `/model.onnx` 永远指回它自己：链是无限的，因此"停下来"这件事只有跳数上界
    /// 一个来源。请求数必须是"上限 + 1"（最后一跳只被**判定**，不被请求）。
    #[test]
    fn a_redirect_chain_longer_than_the_hop_limit_is_rejected() {
        let port_holder: Arc<Mutex<Option<u16>>> = Arc::new(Mutex::new(None));
        let recorded = Arc::clone(&port_holder);
        let server = HttpFixture::start(move |_| {
            let port = recorded
                .lock()
                .expect("the port lock")
                .expect("the port is recorded before any request arrives");
            FixtureResponse::redirect(&format!("http://127.0.0.1:{port}/model.onnx"))
        });
        *port_holder.lock().expect("the port lock") = Some(server.addr().port());
        let dir = TempDir::new("download-redirect-loop");
        let url = server.url("/model.onnx");

        let error = download_verified_with(
            &fixture_request(&url, UNMATCHED_HASH, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("an endless redirect chain must hit the hop limit");

        match download_error(error) {
            DownloadError::RedirectRejected { location } => {
                let location = location.expect("the offending Location must be reported");
                assert!(location.ends_with("/model.onnx"), "{location}");
            }
            other => panic!("expected RedirectRejected, got {other:?}"),
        }
        assert!(entries(dir.path()).is_empty());
        assert_eq!(
            server.request_count(),
            MAX_REDIRECT_HOPS + 1,
            "the limit bounds the number of requests, not just the number of jumps"
        );
    }

    /// 一种**不能**被跟随的 3xx：没有 `Location`。它的错误也必须可定位（`location: None`）。
    #[test]
    fn a_redirect_without_a_location_header_is_rejected() {
        let server = HttpFixture::start(|_| FixtureResponse::status(302, "Found", Vec::new()));
        let dir = TempDir::new("download-redirect-no-location");
        let url = server.url("/model.onnx");

        let error = download_verified_with(
            &fixture_request(&url, UNMATCHED_HASH, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("a 3xx without Location cannot be followed");

        assert_eq!(
            download_error(error),
            DownloadError::RedirectRejected { location: None }
        );
        assert!(entries(dir.path()).is_empty());
        assert_eq!(server.request_count(), 1);
    }

    /// 4) 降级到 `http` 的跳转 → `SchemeRejected`，且**不发**那个明文请求。
    ///
    /// 用的端口上确有服务器在监听（因此"拒绝"不是因为连不上）：它是**生产**策略
    /// （只放行 `https`），因为"测试策略放行明文"绝不能顺延到跳转目标上。
    #[test]
    fn a_redirect_that_downgrades_to_http_is_rejected() {
        let plain = HttpFixture::start(|_| FixtureResponse::ok(b"plaintext".to_vec()));
        let port = plain.addr().port();
        let origin = HttpFixture::start(move |_| {
            FixtureResponse::redirect(&format!("http://127.0.0.1:{port}/model.onnx"))
        });
        let dir = TempDir::new("download-redirect-downgrade");
        let url = origin.url("/model.onnx");

        let error = download_verified_with(
            &fixture_request(&url, UNMATCHED_HASH, dir.path(), 1 << 20),
            &DownloadPolicy::production(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("an https -> http downgrade must be rejected");

        match download_error(error) {
            DownloadError::SchemeRejected { scheme } => assert_eq!(scheme, "http"),
            other => panic!("expected SchemeRejected, got {other:?}"),
        }
        assert!(entries(dir.path()).is_empty());
        assert_eq!(
            plain.request_count(),
            0,
            "the plaintext host must not be contacted at all"
        );
    }

    /// 5) 跳转目标**收不到任何凭据头**：`Authorization` / `Proxy-Authorization` / cookie /
    ///    token 一个都不能出现在第二跳上（第一跳也一并断言，证明"本来就没有"）。
    #[test]
    fn a_redirect_target_receives_no_credential_header() {
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let body = b"credential-free body".to_vec();
        let expected = sha256_hex(&body);
        let worker = HttpFixture::start(move |request| {
            recorder.lock().expect("the recorder lock").push(
                request
                    .headers
                    .iter()
                    .map(|(name, _)| name.to_ascii_lowercase())
                    .collect(),
            );
            FixtureResponse::ok(body.clone())
        });
        let worker_addr = worker.addr();
        let origin = HttpFixture::start(move |_| {
            FixtureResponse::redirect(&format!("http://{worker_addr}/cdn/model.onnx"))
        });
        let dir = TempDir::new("download-redirect-credentials");
        let url = origin.url("/model.onnx");

        let path = download_verified_with(
            &fixture_request(&url, &expected, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect("the redirect must be followed");

        let recorded = seen.lock().expect("the recorder lock");
        assert_eq!(recorded.len(), 1, "the target is asked exactly once");
        for header in &recorded[0] {
            for credential in [
                "authorization",
                "proxy-authorization",
                "cookie",
                "x-rapidocr-token",
                "x-auth-token",
            ] {
                assert_ne!(
                    header, credential,
                    "a redirected request must not carry `{header}`; headers: {:?}",
                    recorded[0]
                );
            }
        }
        assert!(
            recorded[0].iter().any(|name| name == "user-agent"),
            "the target must still see the downloader's own User-Agent: {:?}",
            recorded[0]
        );
        assert_eq!(sha256_file(&path).expect("hash"), expected);
    }

    /// 6) **最终**响应体超过上限时，跳转不改变上限：仍然是流式上限（`TooLarge`）+
    ///    临时文件被删除。这条响应刻意是 chunked（没有 `Content-Length`），因此
    ///    `Content-Length` 预检不可能"顺手"挡住它。
    #[test]
    fn a_redirected_body_above_the_cap_is_still_rejected() {
        let worker = HttpFixture::start(|_| FixtureResponse::chunked(vec![6_u8; 64 * 1024]));
        let chunked_addr = worker.addr();
        let origin = HttpFixture::start(move |_| {
            // 末段仍是 `big.onnx`：**改名**在这里不是被检验的东西（有专门的用例）。
            FixtureResponse::redirect(&format!("http://{chunked_addr}/cdn/big.onnx"))
        });
        let dir = TempDir::new("download-redirect-cap");
        let url = origin.url("/big.onnx");
        let expected = sha256_hex(&vec![6_u8; 64 * 1024]);

        let error = download_verified_with(
            &fixture_request(&url, &expected, dir.path(), 4096),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("the streaming cap must bound the redirected body");

        match download_error(error) {
            DownloadError::TooLarge {
                limit_bytes,
                observed_bytes,
            } => {
                assert_eq!(limit_bytes, 4096);
                assert_eq!(
                    observed_bytes,
                    Some(4097),
                    "the redirected body is read at most max_bytes + 1 bytes"
                );
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert!(
            entries(dir.path()).is_empty(),
            "the temporary file must be deleted: {:?}",
            entries(dir.path())
        );
        assert_eq!(origin.request_count(), 1);
        assert_eq!(worker.request_count(), 1);
    }

    /// **真实 CDN 的形状**：跳转目标的末段是 LFS 对象哈希（文件名只在 `?filename=` 里），
    /// 而落盘名字必须仍然是**初始 URL** 的末段。
    ///
    /// 这条用例锁住一个 M2b 实测到的坑：最初把"跳转目标末段必须等于文件名"当成
    /// §6.1 第 2 条的 path 校验，结果真实权重**全部**下载失败
    /// （`https://cdn-lfs-cn-1.modelscope.cn/prod/lfs-objects/…?filename=PP-OCRv6_det_tiny.onnx`）。
    /// 正确的语义是：跳转只决定**从哪里取字节**，写盘路径与内容仍由初始 URL +
    /// 声明的 SHA-256 决定（§6.4）——因此哈希目录段必须被接受，落盘名必须是 `model.onnx`。
    #[test]
    fn a_cdn_style_redirect_with_a_hash_path_still_lands_under_the_original_name() {
        let body = b"lfs object bytes".to_vec();
        let expected = sha256_hex(&body);
        let worker = HttpFixture::start(move |_| FixtureResponse::ok(body.clone()));
        let worker_addr = worker.addr();
        let origin = HttpFixture::start(move |_| {
            FixtureResponse::redirect(&format!(
                "http://{worker_addr}/prod/lfs-objects/f4/2c/0fbd\
                 ?filename=model.onnx&namespace=test&repository=test&tag=model"
            ))
        });
        let dir = TempDir::new("download-redirect-cdn-path");
        let url = origin.url("/model.onnx");

        let path = download_verified_with(
            &fixture_request(&url, &expected, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect("a CDN-style hash path must be followable");

        assert_eq!(
            path,
            dir.path().join("model.onnx"),
            "the local name comes from the initial URL, never from the redirect target"
        );
        assert_eq!(sha256_file(&path).expect("hash"), expected);
        assert_eq!(entries(dir.path()), vec!["model.onnx".to_string()]);
    }

    #[test]
    fn a_non_success_status_is_a_network_error() {
        let server =
            HttpFixture::start(|_| FixtureResponse::status(404, "Not Found", b"nope".to_vec()));
        let dir = TempDir::new("download-404");
        let url = server.url("/model.onnx");

        let error = download_verified_with(
            &fixture_request(&url, UNMATCHED_HASH, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("a 404 must fail");

        match download_error(error) {
            DownloadError::Network { detail } => assert!(detail.contains("404"), "{detail}"),
            other => panic!("expected Network, got {other:?}"),
        }
        assert!(entries(dir.path()).is_empty());
    }

    /// §6.1 第 4 条：`Content-Length` 预检发生在**创建临时文件之前**，因此目录里连
    /// `.part` 都不该出现。
    #[test]
    fn a_declared_length_above_the_cap_is_rejected_before_anything_is_written() {
        let body = vec![7_u8; 4096];
        let expected = sha256_hex(&body);
        let server = HttpFixture::start(move |_| FixtureResponse::ok(body.clone()));
        let dir = TempDir::new("download-length-cap");
        let url = server.url("/big.onnx");

        let error = download_verified_with(
            &fixture_request(&url, &expected, dir.path(), 1024),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("a declared length above the cap must be rejected");

        match download_error(error) {
            DownloadError::TooLarge {
                limit_bytes,
                observed_bytes,
            } => {
                assert_eq!(limit_bytes, 1024);
                assert_eq!(observed_bytes, Some(4096));
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert!(
            entries(dir.path()).is_empty(),
            "nothing may be written, not even a temporary file: {:?}",
            entries(dir.path())
        );
    }

    /// §6.1 第 5 条：没有可用长度时靠 `take(max_bytes + 1)` 兜底。
    #[test]
    fn a_chunked_body_above_the_cap_is_rejected_and_leaves_no_temp_file() {
        let server = HttpFixture::start(|_| FixtureResponse::chunked(vec![3_u8; 8192]));
        let dir = TempDir::new("download-chunked-cap");
        let url = server.url("/big.onnx");

        let error = download_verified_with(
            &fixture_request(&url, UNMATCHED_HASH, dir.path(), 1024),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("a chunked body above the cap must be rejected");

        match download_error(error) {
            DownloadError::TooLarge {
                limit_bytes,
                observed_bytes,
            } => {
                assert_eq!(limit_bytes, 1024);
                assert_eq!(
                    observed_bytes,
                    Some(1025),
                    "the streaming cap reads at most max_bytes + 1 bytes"
                );
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert!(entries(dir.path()).is_empty());
    }

    #[test]
    fn a_close_delimited_body_above_the_cap_is_rejected_and_leaves_no_temp_file() {
        let server = HttpFixture::start(|_| FixtureResponse::close_delimited(vec![4_u8; 8192]));
        let dir = TempDir::new("download-close-cap");
        let url = server.url("/big.onnx");

        let error = download_verified_with(
            &fixture_request(&url, UNMATCHED_HASH, dir.path(), 1024),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("a close-delimited body above the cap must be rejected");

        assert!(matches!(
            download_error(error),
            DownloadError::TooLarge { .. }
        ));
        assert!(entries(dir.path()).is_empty());
    }

    /// 没有长度的响应在**上限之内**必须正常落盘（不是"拒绝一切无长度响应"）。
    #[test]
    fn a_close_delimited_body_within_the_cap_is_accepted() {
        let body = b"no content-length here".to_vec();
        let expected = sha256_hex(&body);
        let server = HttpFixture::start(move |_| FixtureResponse::close_delimited(body.clone()));
        let dir = TempDir::new("download-close-ok");
        let url = server.url("/model.onnx");

        let path = download_verified_with(
            &fixture_request(&url, &expected, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect("a close-delimited body must be readable");

        assert_eq!(
            fs::read(&path).expect("readable"),
            b"no content-length here"
        );
        assert_eq!(entries(dir.path()), vec!["model.onnx".to_string()]);
    }

    /// `extract_file_name` 只取 URL 末段，`..` 这样的末段不能变成落盘路径。
    #[test]
    fn a_url_whose_last_segment_is_not_a_bare_file_name_is_rejected() {
        let server = HttpFixture::start(|_| FixtureResponse::ok(b"never reached".to_vec()));
        let dir = TempDir::new("download-bad-name");
        let url = server.url("/models/..");

        let error = download_verified_with(
            &fixture_request(&url, UNMATCHED_HASH, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("`..` must not become a target path");

        let message = error.to_string();
        assert!(message.contains("safe file name"), "{message}");
        assert!(message.contains("bare relative file name"), "{message}");
        assert!(entries(dir.path()).is_empty());
        assert_eq!(
            server.request_count(),
            0,
            "the URL is rejected before any request"
        );
    }

    // -----------------------------------------------------------------------
    // 缓存命中与原子替换（§6.3/§6.7）
    // -----------------------------------------------------------------------

    #[test]
    fn an_existing_valid_target_is_returned_without_any_request() {
        let body = b"cached model".to_vec();
        let expected = sha256_hex(&body);
        let server = HttpFixture::start(|_| {
            FixtureResponse::status(
                500,
                "Internal Server Error",
                b"must not be reached".to_vec(),
            )
        });
        let dir = TempDir::new("download-cache");
        dir.write("model.onnx", &body);
        let url = server.url("/model.onnx");

        let path = download_verified_with(
            &fixture_request(&url, &expected, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect("a valid cached file must be reused");

        assert_eq!(path, dir.path().join("model.onnx"));
        assert_eq!(
            server.request_count(),
            0,
            "a cache hit must not touch the network"
        );
    }

    /// §6.7 的回归测试：目标已存在且哈希错误时，重新下载后必须被**正确替换**。
    #[test]
    fn an_existing_corrupt_target_is_replaced() {
        let body = b"the real model".to_vec();
        let expected = sha256_hex(&body);
        let server = HttpFixture::start(move |_| FixtureResponse::ok(body.clone()));
        let dir = TempDir::new("download-replace");
        dir.write("model.onnx", b"corrupt bytes");
        let url = server.url("/model.onnx");

        let path = download_verified_with(
            &fixture_request(&url, &expected, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect("a corrupt target must be replaced");

        assert_eq!(fs::read(&path).expect("readable"), b"the real model");
        assert_eq!(
            sha256_file(&path).expect("hash"),
            expected,
            "the replaced file must hash-match"
        );
        assert_eq!(entries(dir.path()), vec!["model.onnx".to_string()]);
        assert_eq!(server.request_count(), 1);
    }

    /// §6.7：替换失败时**原文件必须存活**，且不得留下临时文件。
    ///
    /// 目标以 `FILE_SHARE_READ` 打开（不共享删除），因此 `MoveFileExW` 会以共享冲突失败——
    /// 这是"原子替换失败"的可复现构造，而不是去猜 Win32 的其它失败模式。
    #[test]
    fn a_failed_replace_keeps_the_original_file_and_drops_the_temp_file() {
        use std::os::windows::fs::OpenOptionsExt as _;

        let body = b"the new model".to_vec();
        let expected = sha256_hex(&body);
        let server = HttpFixture::start(move |_| FixtureResponse::ok(body.clone()));
        let dir = TempDir::new("download-replace-fail");
        dir.write("model.onnx", b"the old model");
        let target = dir.path().join("model.onnx");
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0x0000_0001)
            .open(&target)
            .expect("the target must be openable without sharing deletion");
        let url = server.url("/model.onnx");

        let error = download_verified_with(
            &fixture_request(&url, &expected, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("the replace must fail while the target denies deletion");

        match error {
            RapidOcrError::Io(io) => {
                let message = io.to_string();
                assert!(message.contains("MoveFileExW"), "{message}");
                assert!(message.contains("Win32 error"), "{message}");
                assert!(message.contains("left untouched"), "{message}");
            }
            other => panic!("expected an Io error with the Win32 code, got {other:?}"),
        }
        drop(locked);
        assert_eq!(
            fs::read(&target).expect("the original file must still be readable"),
            b"the old model",
            "a failed replace must not damage the original file"
        );
        assert_eq!(
            entries(dir.path()),
            vec!["model.onnx".to_string()],
            "the temporary file must be deleted on a failed replace"
        );
    }

    /// 目标位置是个目录（既不是文件也不是可替换对象）时同样失败且不破坏它。
    #[test]
    fn a_replace_onto_a_directory_fails_and_keeps_the_directory() {
        let body = b"the model".to_vec();
        let expected = sha256_hex(&body);
        let server = HttpFixture::start(move |_| FixtureResponse::ok(body.clone()));
        let dir = TempDir::new("download-replace-dir");
        let target = dir.path().join("model.onnx");
        fs::create_dir(&target).expect("the target directory must be creatable");
        let url = server.url("/model.onnx");

        let error = download_verified_with(
            &fixture_request(&url, &expected, dir.path(), 1 << 20),
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("replacing a directory must fail");

        assert!(matches!(error, RapidOcrError::Io(_)), "{error:?}");
        assert!(target.is_dir(), "the directory must survive");
        assert_eq!(entries(dir.path()), vec!["model.onnx".to_string()]);
    }

    // -----------------------------------------------------------------------
    // 单飞（§6.1 第 8 条）
    // -----------------------------------------------------------------------

    #[test]
    fn two_concurrent_downloads_of_one_target_fetch_exactly_once() {
        let body = vec![9_u8; 64 * 1024];
        let expected = sha256_hex(&body);
        let server = HttpFixture::start(move |_| {
            // 让第一个请求停在服务器上，保证两个线程真的重叠。
            thread::sleep(Duration::from_millis(200));
            FixtureResponse::ok(body.clone())
        });
        let dir = TempDir::new("download-single-flight");
        let url = server.url("/model.onnx");
        let barrier = Arc::new(Barrier::new(2));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let url = url.clone();
            let expected = expected.clone();
            let save_dir = dir.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                download_verified_with(
                    &fixture_request(&url, &expected, &save_dir, 1 << 20),
                    &fixture_policy(&huge_free_space()),
                    &mut NoObserver,
                )
            }));
        }

        for handle in handles {
            let downloaded = handle
                .join()
                .expect("the download thread must not panic")
                .expect("both callers must succeed");
            assert_eq!(downloaded, dir.path().join("model.onnx"));
        }
        assert_eq!(
            server.request_count(),
            1,
            "single flight must issue exactly one network fetch"
        );
        assert_eq!(entries(dir.path()), vec!["model.onnx".to_string()]);
    }

    // -----------------------------------------------------------------------
    // 磁盘空间（§6.1 第 10 条、§6.5）
    // -----------------------------------------------------------------------

    #[test]
    fn insufficient_disk_space_is_reported_before_anything_is_written() {
        let body = vec![1_u8; 4096];
        let expected = sha256_hex(&body);
        let server = HttpFixture::start(move |_| FixtureResponse::ok(body.clone()));
        let dir = TempDir::new("download-space");
        let url = server.url("/model.onnx");

        let error = download_verified_with(
            &fixture_request(&url, &expected, dir.path(), 1 << 20),
            &fixture_policy(&FixedFreeSpace(1000)),
            &mut NoObserver,
        )
        .expect_err("1000 available bytes cannot hold a 4096 byte file");

        match download_error(error) {
            DownloadError::InsufficientSpace {
                required_bytes,
                available_bytes,
            } => {
                assert_eq!(required_bytes, 4096, "a known length is checked exactly");
                assert_eq!(available_bytes, 1000);
            }
            other => panic!("expected InsufficientSpace, got {other:?}"),
        }
        assert!(entries(dir.path()).is_empty());
    }

    /// 长度未知时按 §6.5 用 `max_bytes` 计入需求（保守但有界）。
    #[test]
    fn an_unknown_length_download_is_budgeted_at_the_streaming_cap() {
        let server = HttpFixture::start(|_| FixtureResponse::close_delimited(vec![5_u8; 64]));
        let dir = TempDir::new("download-space-unknown");
        let url = server.url("/model.onnx");

        let error = download_verified_with(
            &fixture_request(&url, UNMATCHED_HASH, dir.path(), 4096),
            &fixture_policy(&FixedFreeSpace(4000)),
            &mut NoObserver,
        )
        .expect_err("an unknown length must be budgeted at the streaming cap");

        match download_error(error) {
            DownloadError::InsufficientSpace {
                required_bytes,
                available_bytes,
            } => {
                assert_eq!(required_bytes, 4096);
                assert_eq!(available_bytes, 4000);
            }
            other => panic!("expected InsufficientSpace, got {other:?}"),
        }
        assert!(entries(dir.path()).is_empty());
    }

    // -----------------------------------------------------------------------
    // 分项超时（§6.1 第 11 条）
    // -----------------------------------------------------------------------

    #[test]
    fn a_stalled_body_read_is_a_read_timeout() {
        let server = HttpFixture::start(|_| {
            FixtureResponse::stall_after_head(1 << 20, Duration::from_secs(2))
        });
        let dir = TempDir::new("download-read-timeout");
        let url = server.url("/model.onnx");
        let mut request = fixture_request(&url, UNMATCHED_HASH, dir.path(), 1 << 20);
        request.read_timeout = Duration::from_millis(200);
        request.connect_timeout = Duration::from_secs(5);

        let error = download_verified_with(
            &request,
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("a stalled body must time out");

        match download_error(error) {
            DownloadError::ReadTimeout { timeout_ms } => assert_eq!(timeout_ms, 200),
            other => panic!("expected ReadTimeout, got {other:?}"),
        }
        assert!(
            entries(dir.path()).is_empty(),
            "a timed-out download must delete its temporary file: {:?}",
            entries(dir.path())
        );
    }

    /// 服务器连响应头都不发：读取预算先到期，必须报 `ReadTimeout` 而不是 `ConnectTimeout`
    /// （连接已经建立成功）。
    #[test]
    fn a_server_that_never_answers_is_a_read_timeout_not_a_connect_timeout() {
        let server = HttpFixture::start(|_| FixtureResponse::silent(Duration::from_secs(2)));
        let dir = TempDir::new("download-header-timeout");
        let url = server.url("/model.onnx");
        let mut request = fixture_request(&url, UNMATCHED_HASH, dir.path(), 1 << 20);
        request.read_timeout = Duration::from_millis(200);
        request.connect_timeout = Duration::from_secs(5);

        let error = download_verified_with(
            &request,
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("a server that never answers must time out");

        match download_error(error) {
            DownloadError::ReadTimeout { timeout_ms } => assert_eq!(timeout_ms, 200),
            other => panic!("expected ReadTimeout, got {other:?}"),
        }
    }

    /// 连接预算与读取预算必须产生**不同**的错误类（§6.1 第 11 条）。
    ///
    /// **环回上无法制造真实的连接阶段超时**：TCP 连接约 0.4 ms 就完成，而 reqwest/tokio
    /// 的定时器粒度是 1 ms 量级——即使把 `connect_timeout` 压到 1 ns，连接也会先完成
    /// （实测：此时失败的是"等待响应"的读取预算，`is_timeout()` 为真但耗时等于读取预算）。
    /// 因此这里拿一个**真实的** reqwest 超时错误（服务器只发响应头后停住），
    /// 再把这个错误交给判定函数，用"连接阶段应有的耗时"验证 `ConnectTimeout` 分支；
    /// 而 `ReadTimeout` 同时有上面的端到端测试。
    #[test]
    fn a_connect_phase_timeout_and_a_read_phase_timeout_are_distinct_classes() {
        let server = HttpFixture::start(|_| FixtureResponse::silent(Duration::from_secs(2)));
        let url = server.url("/model.onnx");
        let connect_timeout = Duration::from_millis(50);
        let read_timeout = Duration::from_millis(200);

        // 真实错误：客户端在读取预算用尽时失败（`is_timeout() == true`）。
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(read_timeout)
            .build()
            .expect("the probe client must build");
        let started = Instant::now();
        let error = client
            .get(&url)
            .send()
            .expect_err("a silent server must not answer");
        let elapsed = started.elapsed();
        assert!(error.is_timeout(), "expected a timeout error: {error}");
        assert!(
            elapsed >= read_timeout,
            "the read budget must have expired: {elapsed:?}"
        );

        // 等待响应：耗时达到读取预算 → ReadTimeout，并报出读取预算。
        assert_eq!(
            classify_send_timeout(error.is_connect(), elapsed, connect_timeout, read_timeout),
            DownloadError::ReadTimeout { timeout_ms: 200 }
        );
        // 连接阶段超时：耗时还没到读取预算 → ConnectTimeout，并报出**连接**预算。
        assert_eq!(
            classify_send_timeout(false, connect_timeout, connect_timeout, read_timeout),
            DownloadError::ConnectTimeout { timeout_ms: 50 }
        );
        // reqwest 明确说是连接错误时，即使耗时超过读取预算也按连接阶段归类。
        assert_eq!(
            classify_send_timeout(true, read_timeout * 4, connect_timeout, read_timeout),
            DownloadError::ConnectTimeout { timeout_ms: 50 }
        );
    }

    /// 真实的连接错误（端口上没有人监听）必须是 `Network`，不能伪装成超时。
    #[test]
    fn a_refused_connection_is_a_network_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener
            .local_addr()
            .expect("a bound listener has an address");
        drop(listener);
        let dir = TempDir::new("download-refused");
        let url = format!("http://{addr}/model.onnx");
        let mut request = fixture_request(&url, UNMATCHED_HASH, dir.path(), 1 << 20);
        // 端口拒绝需要在 SYN 重试之后才会报告，因此两个预算都给足。
        request.connect_timeout = Duration::from_secs(5);
        request.read_timeout = Duration::from_secs(10);

        let error = download_verified_with(
            &request,
            &fixture_policy(&huge_free_space()),
            &mut NoObserver,
        )
        .expect_err("nothing is listening on that port");

        match download_error(error) {
            DownloadError::Network { detail } => assert!(!detail.is_empty()),
            other => panic!("expected Network, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // 预算与集合下载（§6.2）
    // -----------------------------------------------------------------------

    #[test]
    fn the_library_defaults_match_the_documented_download_limits() {
        // 常量断言放进 `const` 块：与默认表/CLI 的默认值同源，改歪了就是编译失败。
        const {
            assert!(DEFAULT_MAX_DOWNLOAD_MB == 1024);
            assert!(DEFAULT_MAX_DOWNLOAD_BYTES == 1024 * 1024 * 1024);
            assert!(
                DEFAULT_MAX_DOWNLOAD_BYTES > 566 * 1_000_000,
                "§6.2: the default cap must fit the 566 MB formula model"
            );
            assert!(DEFAULT_CONNECT_TIMEOUT.as_secs() == 10);
            assert!(DEFAULT_READ_TIMEOUT.as_secs() == 30);
            assert!(
                DEFAULT_CONNECT_TIMEOUT.as_secs() < DEFAULT_READ_TIMEOUT.as_secs(),
                "the connect/read classification relies on the connect budget expiring first"
            );
        }

        let request = DownloadRequest::new(
            "https://www.modelscope.cn/models/x.onnx",
            UNMATCHED_HASH,
            Path::new("."),
        );
        assert_eq!(request.max_bytes, DEFAULT_MAX_DOWNLOAD_BYTES);
        assert_eq!(request.connect_timeout, DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(request.read_timeout, DEFAULT_READ_TIMEOUT);
    }

    #[test]
    fn a_download_budget_never_overspends_and_a_failed_charge_costs_nothing() {
        let mut budget = DownloadBudget::new(1000);
        assert_eq!(budget.remaining_bytes(), 1000);
        assert_eq!(budget.per_file_cap(), 1000);

        budget.charge(400).expect("400 <= 1000");
        assert_eq!(budget.spent_bytes(), 400);
        assert_eq!(budget.remaining_bytes(), 600);
        assert_eq!(budget.per_file_cap(), 600);

        let error = budget.charge(601).expect_err("601 > 600");
        match download_error(error) {
            DownloadError::TooLarge {
                limit_bytes,
                observed_bytes,
            } => {
                assert_eq!(limit_bytes, 600);
                assert_eq!(observed_bytes, Some(601));
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert_eq!(budget.spent_bytes(), 400, "a failed charge must not spend");

        budget.charge(600).expect("the remainder fits exactly");
        assert_eq!(budget.remaining_bytes(), 0);
    }

    #[test]
    fn every_download_error_class_has_a_stable_kind() {
        let cases: [(DownloadError, &str); 10] = [
            (
                DownloadError::SchemeRejected {
                    scheme: "http".to_string(),
                },
                "scheme",
            ),
            (
                DownloadError::RedirectRejected { location: None },
                "redirect",
            ),
            (
                DownloadError::HostRejected {
                    host: "evil.example".to_string(),
                },
                "host",
            ),
            (
                DownloadError::TooLarge {
                    limit_bytes: 1,
                    observed_bytes: Some(2),
                },
                "too_large",
            ),
            (
                DownloadError::Network {
                    detail: "reset".to_string(),
                },
                "network",
            ),
            (
                DownloadError::ConnectTimeout { timeout_ms: 10 },
                "connect_timeout",
            ),
            (
                DownloadError::ReadTimeout { timeout_ms: 20 },
                "read_timeout",
            ),
            (
                DownloadError::InsufficientSpace {
                    required_bytes: 2,
                    available_bytes: 1,
                },
                "insufficient_space",
            ),
            (
                DownloadError::HashMismatch {
                    expected: "aa".to_string(),
                    actual: "bb".to_string(),
                },
                "hash_mismatch",
            ),
            (DownloadError::Cancelled, "cancelled"),
        ];
        for (error, kind) in cases {
            assert_eq!(error.kind(), kind, "error: {error}");
            assert!(!error.to_string().is_empty(), "error: {error}");
            // 库错误是唯一定义处：包进 RapidOcrError 之后分类信息不丢。
            let wrapped = RapidOcrError::from(error.clone());
            assert!(matches!(wrapped, RapidOcrError::Download(_)));
            assert!(
                wrapped.to_string().contains(&error.to_string()),
                "the wrapped error must keep the download message: {wrapped}"
            );
        }
    }

    #[test]
    fn a_set_download_charges_every_file_against_one_running_budget() {
        let bodies: HashMap<&'static str, Vec<u8>> = [
            ("/a.onnx", vec![1_u8; 1000]),
            ("/b.onnx", vec![2_u8; 2000]),
            ("/c.onnx", vec![3_u8; 3000]),
        ]
        .into_iter()
        .collect();
        let server = HttpFixture::start(move |request| {
            FixtureResponse::ok(
                bodies
                    .get(request.target.as_str())
                    .cloned()
                    .unwrap_or_default(),
            )
        });
        let dir = TempDir::new("download-set-budget");

        let files = vec![
            set_file(
                "a.onnx",
                &server.url("/a.onnx"),
                &sha256_hex(&vec![1_u8; 1000]),
                Some(1000),
            ),
            set_file(
                "b.onnx",
                &server.url("/b.onnx"),
                &sha256_hex(&vec![2_u8; 2000]),
                Some(2000),
            ),
            set_file(
                "c.onnx",
                &server.url("/c.onnx"),
                &sha256_hex(&vec![3_u8; 3000]),
                Some(3000),
            ),
        ];
        let mut budget = DownloadBudget::new(100_000);

        let paths = download_set(&model_set(files), dir.path(), &mut budget)
            .expect("all three files fit the budget");

        assert_eq!(paths.len(), 3);
        assert_eq!(server.request_count(), 3);
        assert_eq!(
            budget.spent_bytes(),
            6000,
            "the budget is shared by the whole set"
        );
        assert_eq!(budget.remaining_bytes(), 94_000);
        assert_eq!(
            entries(dir.path()),
            vec![
                "a.onnx".to_string(),
                "b.onnx".to_string(),
                "c.onnx".to_string()
            ]
        );
    }

    #[test]
    fn a_set_download_refuses_a_file_whose_size_exceeds_the_remaining_budget_without_requesting_it()
    {
        let first = vec![1_u8; 40 * 1024];
        let expected_first = sha256_hex(&first);
        let server = HttpFixture::start(move |_| FixtureResponse::ok(first.clone()));
        let dir = TempDir::new("download-set-cap");

        let files = vec![
            set_file(
                "a.onnx",
                &server.url("/a.onnx"),
                &expected_first,
                Some(40 * 1024),
            ),
            // 声明 40 KiB，但 50 KiB 的预算只够第一个文件。
            set_file(
                "b.onnx",
                &server.url("/b.onnx"),
                UNMATCHED_HASH,
                Some(40 * 1024),
            ),
        ];
        let mut budget = DownloadBudget::new(50 * 1024);

        let error = download_set(&model_set(files), dir.path(), &mut budget)
            .expect_err("the second file does not fit the remaining budget");

        match download_error(error) {
            DownloadError::TooLarge {
                limit_bytes,
                observed_bytes,
            } => {
                assert_eq!(limit_bytes, 50 * 1024 - 40 * 1024, "the remaining budget");
                assert_eq!(observed_bytes, Some(40 * 1024), "the declared size");
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert_eq!(
            server.request_count(),
            1,
            "a file that cannot fit must be rejected before any request"
        );
        assert!(dir.path().join("a.onnx").is_file());
        assert!(!dir.path().join("b.onnx").exists());
    }

    /// `size_bytes` 未知 → 没有"提前预检"这一步，但仍然受剩余额度这条**流式**上限保护。
    #[test]
    fn a_set_download_bounds_an_unknown_size_file_by_the_remaining_streaming_cap() {
        let server = HttpFixture::start(|_| FixtureResponse::close_delimited(vec![7_u8; 8192]));
        let dir = TempDir::new("download-set-unknown");
        let files = vec![set_file(
            "a.onnx",
            &server.url("/a.onnx"),
            UNMATCHED_HASH,
            None,
        )];
        let mut budget = DownloadBudget::new(1024);

        let error = download_set(&model_set(files), dir.path(), &mut budget)
            .expect_err("an unknown size is still bounded by the streaming cap");

        match download_error(error) {
            DownloadError::TooLarge {
                limit_bytes,
                observed_bytes,
            } => {
                assert_eq!(limit_bytes, 1024);
                assert_eq!(observed_bytes, Some(1025));
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert!(entries(dir.path()).is_empty());
    }

    #[test]
    fn a_set_download_refuses_unverifiable_unsourced_and_misnamed_files() {
        let dir = TempDir::new("download-set-refusals");
        let url = "https://www.modelscope.cn/models/a.onnx";

        let no_hash = ModelFileSpec {
            name: "a.onnx".to_string(),
            role: ModelRole::Detector,
            size_bytes: Some(10),
            sha256: String::new(),
            source_url: url.to_string(),
        };
        let error = download_model_set(
            &model_set(vec![no_hash]),
            dir.path(),
            &mut DownloadBudget::new(1 << 20),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .expect_err("an unhashed file must not be downloaded");
        assert!(error.to_string().contains("no SHA-256"), "{error}");

        let no_source = set_file("a.onnx", "", UNMATCHED_HASH, Some(10));
        let error = download_model_set(
            &model_set(vec![no_source]),
            dir.path(),
            &mut DownloadBudget::new(1 << 20),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .expect_err("a file without a trusted source must not be downloaded");
        assert!(
            error.to_string().contains("no trusted download source"),
            "{error}"
        );

        let misnamed = set_file("renamed.onnx", url, UNMATCHED_HASH, Some(10));
        let error = download_model_set(
            &model_set(vec![misnamed]),
            dir.path(),
            &mut DownloadBudget::new(1 << 20),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .expect_err("the file name must equal the URL's last segment");
        assert!(error.to_string().contains("must agree"), "{error}");
    }

    #[test]
    fn a_set_download_skips_files_that_are_already_valid() {
        let present = b"already here".to_vec();
        let missing = vec![8_u8; 100];
        let missing_len = missing.len() as u64;
        let expected_present = sha256_hex(&present);
        let expected_missing = sha256_hex(&missing);
        let server = HttpFixture::start(move |_| FixtureResponse::ok(missing.clone()));
        let dir = TempDir::new("download-set-present");
        dir.write("present.onnx", &present);

        let files = vec![
            set_file(
                "present.onnx",
                &server.url("/present.onnx"),
                &expected_present,
                Some(present.len() as u64),
            ),
            set_file(
                "missing.onnx",
                &server.url("/missing.onnx"),
                &expected_missing,
                Some(missing_len),
            ),
        ];
        let mut budget = DownloadBudget::new(1 << 20);

        let paths = download_set(&model_set(files), dir.path(), &mut budget)
            .expect("the set must end up complete");

        assert_eq!(paths.len(), 2);
        assert_eq!(
            server.request_count(),
            1,
            "only the missing file is fetched"
        );
        assert_eq!(budget.spent_bytes(), 100, "a cache hit costs no budget");
    }

    // -----------------------------------------------------------------------
    // 进度、取消与显式白名单（§6.1 第 3 条、§6.6）
    // -----------------------------------------------------------------------

    /// 带观察者、可指定显式白名单的集合下载（测试用 fixture 策略）。
    fn download_set_observed(
        set: &ModelSet,
        root: &Path,
        budget: &mut DownloadBudget,
        allowed_hosts: &[&str],
        observer: &mut dyn DownloadObserver,
    ) -> Result<Vec<PathBuf>> {
        download_model_set_with(
            set,
            root,
            budget,
            Duration::from_secs(5),
            Duration::from_secs(5),
            allowed_hosts,
            observer,
            &fixture_policy(&huge_free_space()),
        )
    }

    /// 记录每一次回调的观察者。`cancel_at` = 在第 N 个文件开始前请求取消（1 基）。
    #[derive(Default)]
    struct RecordingObserver {
        events: Vec<String>,
        cancel_at: Option<usize>,
    }

    impl DownloadObserver for RecordingObserver {
        fn file_started(
            &mut self,
            file: &ModelFileSpec,
            index: usize,
            total: usize,
            declared_bytes: Option<u64>,
        ) -> bool {
            self.events.push(format!(
                "start {} {index}/{total} {declared_bytes:?}",
                file.name
            ));
            self.cancel_at != Some(index)
        }

        fn bytes_written(&mut self, written_bytes: u64) {
            self.events.push(format!("bytes {written_bytes}"));
        }

        fn file_finished(&mut self, file: &ModelFileSpec, index: usize, bytes: u64) {
            self.events
                .push(format!("done {} {index} {bytes}", file.name));
        }
    }

    /// 三个回调的顺序、下标、总数与字节数（§4.3 的进度字段就是这些事实）。
    #[test]
    fn the_observer_sees_every_file_its_index_and_its_bytes_in_order() {
        let first = vec![1_u8; 1000];
        let second = vec![2_u8; 2000];
        let server = HttpFixture::start(move |request| match request.target.as_str() {
            "/a.onnx" => FixtureResponse::ok(first.clone()),
            _ => FixtureResponse::ok(second.clone()),
        });
        let dir = TempDir::new("observer-progress");
        let files = vec![
            set_file(
                "a.onnx",
                &server.url("/a.onnx"),
                &sha256_hex(&vec![1_u8; 1000]),
                Some(1000),
            ),
            set_file(
                "b.onnx",
                &server.url("/b.onnx"),
                &sha256_hex(&vec![2_u8; 2000]),
                Some(2000),
            ),
        ];
        let mut observer = RecordingObserver::default();
        let mut budget = DownloadBudget::new(1 << 20);

        let paths = download_set_observed(
            &model_set(files),
            dir.path(),
            &mut budget,
            &FIXTURE_HOSTS,
            &mut observer,
        )
        .expect("both files must download");

        assert_eq!(paths.len(), 2);
        let starts: Vec<&str> = observer
            .events
            .iter()
            .filter(|event| event.starts_with("start "))
            .map(String::as_str)
            .collect();
        assert_eq!(
            starts,
            vec!["start a.onnx 1/2 Some(1000)", "start b.onnx 2/2 Some(2000)"],
            "{:?}",
            observer.events
        );
        let dones: Vec<&str> = observer
            .events
            .iter()
            .filter(|event| event.starts_with("done "))
            .map(String::as_str)
            .collect();
        assert_eq!(
            dones,
            vec!["done a.onnx 1 1000", "done b.onnx 2 2000"],
            "{:?}",
            observer.events
        );
        // 字节回调报告的是**当前文件**的累计值：每个文件最终都会报出自己的完整大小。
        assert!(observer.events.contains(&"bytes 1000".to_string()));
        assert!(observer.events.contains(&"bytes 2000".to_string()));
        // 在每个文件的区间内，字节数单调不减（分块读取时会有多次回调）。
        let mut current = 0_u64;
        for event in &observer.events {
            if event.starts_with("start ") {
                current = 0;
                continue;
            }
            if let Some(value) = event.strip_prefix("bytes ") {
                let value: u64 = value.parse().expect("a byte count");
                assert!(value >= current, "bytes must be monotonic: {event}");
                current = value;
            }
        }
    }

    /// §6.6：取消在**文件边界**生效——已完成并校验的文件保留，后续文件不再请求，
    /// 临时文件不残留。
    #[test]
    fn cancelling_at_a_file_boundary_keeps_verified_files_and_leaves_no_temp_file() {
        let server = HttpFixture::start(|request| {
            FixtureResponse::ok(vec![
                match request.target.as_str() {
                    "/a.onnx" => 1_u8,
                    "/b.onnx" => 2_u8,
                    _ => 3_u8,
                };
                500
            ])
        });
        let dir = TempDir::new("cancel-boundary");
        let files = ["a.onnx", "b.onnx", "c.onnx"]
            .iter()
            .map(|name| {
                set_file(
                    name,
                    &server.url(&format!("/{name}")),
                    &sha256_hex(&vec![
                        match *name {
                            "a.onnx" => 1_u8,
                            "b.onnx" => 2_u8,
                            _ => 3_u8,
                        };
                        500
                    ]),
                    Some(500),
                )
            })
            .collect::<Vec<_>>();
        let mut observer = RecordingObserver {
            cancel_at: Some(2),
            ..RecordingObserver::default()
        };
        let mut budget = DownloadBudget::new(1 << 20);

        let error = download_set_observed(
            &model_set(files),
            dir.path(),
            &mut budget,
            &FIXTURE_HOSTS,
            &mut observer,
        )
        .expect_err("the cancel request must stop the set download");

        match download_error(error) {
            DownloadError::Cancelled => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
        assert_eq!(
            entries(dir.path()),
            vec!["a.onnx".to_string()],
            "the verified file is kept and no temp file is left behind"
        );
        assert_eq!(
            server.request_count(),
            1,
            "the second file must never be requested"
        );
        assert_eq!(budget.spent_bytes(), 500, "only the kept file is charged");
        assert_eq!(
            sha256_file(dir.path().join("a.onnx")).expect("hash"),
            sha256_hex(&vec![1_u8; 500])
        );
    }

    /// 第一个文件之前就取消：一个字节都不写、一个请求都不发。
    #[test]
    fn a_cancel_requested_before_the_first_file_writes_nothing() {
        let server = HttpFixture::start(|_| FixtureResponse::ok(vec![9_u8; 128]));
        let dir = TempDir::new("cancel-first");
        let files = vec![set_file(
            "a.onnx",
            &server.url("/a.onnx"),
            &sha256_hex(&[9_u8; 128]),
            Some(128),
        )];
        let mut observer = RecordingObserver {
            cancel_at: Some(1),
            ..RecordingObserver::default()
        };

        let error = download_set_observed(
            &model_set(files),
            dir.path(),
            &mut DownloadBudget::new(1 << 20),
            &FIXTURE_HOSTS,
            &mut observer,
        )
        .expect_err("a cancel before the first file must stop immediately");

        assert!(matches!(download_error(error), DownloadError::Cancelled));
        assert!(entries(dir.path()).is_empty());
        assert_eq!(server.request_count(), 0, "no request may be issued");
    }

    /// §6.1 第 3 条：编译期常量**不变**，扩展只能经**显式参数**传入。
    #[test]
    fn an_explicit_host_allow_list_extends_the_compiled_in_one() {
        // 常量本身逐项锁死（与 the_allowed_download_hosts_are_exactly_the_declared_set 同源）。
        assert_eq!(
            ALLOWED_DOWNLOAD_HOSTS,
            ["www.modelscope.cn", "cdn-lfs-cn-1.modelscope.cn"]
        );
        assert_eq!(DEFAULT_ALLOWED_HOSTS, ALLOWED_DOWNLOAD_HOSTS);

        let body = vec![5_u8; 64];
        let expected = sha256_hex(&body);
        let server = HttpFixture::start(move |_| FixtureResponse::ok(body.clone()));
        let dir = TempDir::new("explicit-hosts");
        let files = vec![set_file(
            "a.onnx",
            &server.url("/a.onnx"),
            &expected,
            Some(64),
        )];

        // 用编译期常量：本机 fixture 的 host 不在里面 → 拒绝，且不发请求。
        let error = download_set_observed(
            &model_set(files.clone()),
            dir.path(),
            &mut DownloadBudget::new(1 << 20),
            DEFAULT_ALLOWED_HOSTS,
            &mut NoObserver,
        )
        .expect_err("the compiled allow-list must reject 127.0.0.1");
        match download_error(error) {
            DownloadError::HostRejected { host } => assert_eq!(host, "127.0.0.1"),
            other => panic!("expected HostRejected, got {other:?}"),
        }
        assert_eq!(server.request_count(), 0);
        assert!(entries(dir.path()).is_empty());

        // 同一个请求，只是**显式**传入了另一个受信任列表 → 放行。
        let mut budget = DownloadBudget::new(1 << 20);
        let paths = download_set_observed(
            &model_set(files),
            dir.path(),
            &mut budget,
            &FIXTURE_HOSTS,
            &mut NoObserver,
        )
        .expect("an explicit allow-list entry must be honoured");
        assert_eq!(paths.len(), 1);
        assert_eq!(server.request_count(), 1);
        assert_eq!(
            sha256_file(dir.path().join("a.onnx")).expect("hash"),
            expected
        );
    }

    /// §6.5 的任务级空间探测：与下载器内部用的是**同一个** Win32 实现。
    #[test]
    fn available_disk_bytes_reports_the_free_space_of_this_volume() {
        let dir = TempDir::new("free-space-probe");
        let available = available_disk_bytes(dir.path()).expect("the probe must succeed");
        assert!(
            available > 0,
            "a writable volume has free space: {available}"
        );
        // 不存在的目录同样给出可定位错误，而不是 0（0 会被误读成"磁盘满了"）。
        let missing = dir.path().join("does-not-exist");
        assert!(available_disk_bytes(&missing).is_err());
    }
}
