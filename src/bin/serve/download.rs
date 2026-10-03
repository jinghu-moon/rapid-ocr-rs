//! 下载 worker：独立的有界 channel + 独立线程（§8.1 的最后一行）。
//!
//! # 这一层做什么
//!
//! 库侧的加固下载器在 M0b 落地（仅 HTTPS、拒绝重定向、host 白名单、体积上限、唯一临时名、
//! `MoveFileExW` 原子替换、单飞、空间预检），M2 把它接到 HTTP 上的下载任务：
//!
//! 1. [`worker`] 从有界 channel 收到 [`DownloadCommand`]（只有 `job_id` 与 `set_id`，
//!    §7.2：请求体**不得**携带 URL），把集合从 [`super::model_plan::ModelPlan`] 解析出来，
//!    再用 [`ModelDownloader`] 执行；
//! 2. 进度（逐文件 done/total、字节 done/total、当前文件名）经 [`DownloadSink`] 写进
//!    任务存储，因此 `GET /api/jobs/{id}` 是进度与状态的**唯一**事实来源；
//! 3. 取消（§6.6）在**文件边界**生效：[`DownloadSink::file_started`] 返回 `false` 时不开始
//!    下一个文件，库侧返回 [`DownloadError::Cancelled`]；已经校验通过的文件保留。
//!    **做不到的事**：正在进行的那个文件不会被中断（阻塞式 HTTP 读取没有安全的中断语义），
//!    因此取消的延迟上界是"当前文件的剩余下载时间"。
//!
//! # 为什么执行体是可注入的
//!
//! 与 `engine.rs` 的 [`super::engine::OcrBackend`] 同一个理由：需要在**不依赖公网**的前提下
//! 驱动成功、哈希失败、预算拒绝、逐文件失败、文件边界取消与进度记账。生产路径永远是
//! [`real_downloader_factory`]（真库下载器）；脚本化只出现在测试模块里。
//! host 允许列表是**显式参数**（[`DownloadJob::allowed_hosts`]），库常量不被修改（§6.1 第 3 条）。
//!
//! 有界容量用编译期常量而不是新的 CLI 选项：§3 的选项清单是冻结的，将来若要暴露
//! `--max-queue-download` 再按同样的校验方式加。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use rapid_ocr_rs::{
    DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT, DownloadBudget, DownloadError, DownloadObserver,
    ModelFileSpec, ModelSet, RapidOcrError, download_model_set_observed,
};

use super::error::ServeError;
use super::jobs::DownloadProgress;
use super::limits::ServeConfigError;
use super::model_plan::PendingDownload;
use super::server::ServeShared;

/// 下载 channel 的有界容量（满 → 503 `busy`，与 OCR 队列同语义）。
pub(super) const DOWNLOAD_QUEUE_CAPACITY: usize = 4;

/// 下载 worker 的等待上限：只用于周期性检查关闭标志。
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 一条下载命令：只有任务 id 与被点击的集合 id（§4.2 + §7.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DownloadCommand {
    pub job_id: String,
    pub set_id: String,
}

/// 一次下载任务的全部输入（HTTP 层已经从模型计划里解析好的东西）。
pub(super) struct DownloadJob<'a> {
    /// 集合本身（由 `set_id` 严格解析，**没有** `sets[0]` 回落）。
    pub set: &'a ModelSet,
    /// 落盘目录（模型目录；`/api/models` 报告状态用的是同一个目录）。
    pub root: &'a Path,
    /// `--max-download-mb` 换算出的**整批**额度（§6.2）。
    pub budget_bytes: u64,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    /// **显式**的可信 host 列表 = 编译期白名单 ∪ `--allow-download-host`（§6.1 第 3 条）。
    pub allowed_hosts: Vec<String>,
}

impl DownloadJob<'_> {
    /// 任务的进度起点（审计口径：只统计需要下载的文件）。
    pub fn initial_progress(&self, pending: PendingDownload) -> DownloadProgress {
        DownloadProgress::planned(pending.files, pending.bytes)
    }
}

/// 进度与取消的接收端（serve 侧）。
///
/// 与库的 [`DownloadObserver`] 一一对应，但语义按 HTTP 任务的需要命名：库不知道"任务"，
/// serve 不知道"读取块"。生产实现是 [`JobSink`]。
pub(super) trait DownloadSink {
    /// 开始一个文件之前询问；返回 `false` = 在文件边界取消。
    fn file_started(
        &mut self,
        file: &ModelFileSpec,
        index: usize,
        total: usize,
        declared_bytes: Option<u64>,
    ) -> bool;
    /// 当前文件已写入的累计字节。
    fn bytes_written(&mut self, written_bytes: u64);
    /// 一个文件已通过校验并落盘。
    fn file_finished(&mut self, file: &ModelFileSpec, index: usize, bytes: u64);
}

/// 集合下载的执行体（可注入，见模块文档）。
pub(super) trait ModelDownloader: Send {
    /// 下载整个集合；返回每个文件在磁盘上的路径。
    ///
    /// 错误类型与库一致（[`RapidOcrError`]）：它已经把 `DownloadError` 的十二类包在里面，
    /// serve 侧只需要一次 `ServeError::from`，不另造第二套表示。
    fn download(
        &mut self,
        job: &DownloadJob<'_>,
        sink: &mut dyn DownloadSink,
    ) -> Result<Vec<PathBuf>, RapidOcrError>;
}

/// 执行体工厂（每个 worker 一个实例；测试注入脚本化实现）。
pub(super) type DownloaderFactory = Arc<dyn Fn() -> Box<dyn ModelDownloader> + Send + Sync>;

/// 生产路径的工厂：真库下载器。
pub(super) fn real_downloader_factory() -> DownloaderFactory {
    Arc::new(|| Box::new(RealDownloader))
}

/// [`rapid_ocr_rs::download_model_set_observed`] 的薄包装。
struct RealDownloader;

impl ModelDownloader for RealDownloader {
    fn download(
        &mut self,
        job: &DownloadJob<'_>,
        sink: &mut dyn DownloadSink,
    ) -> Result<Vec<PathBuf>, RapidOcrError> {
        let hosts: Vec<&str> = job.allowed_hosts.iter().map(String::as_str).collect();
        let mut budget = DownloadBudget::new(job.budget_bytes);
        let mut observer = SinkObserver { sink };
        download_model_set_observed(
            job.set,
            job.root,
            &mut budget,
            job.connect_timeout,
            job.read_timeout,
            &hosts,
            &mut observer,
        )
    }
}

/// 库的观察者 → serve 的 [`DownloadSink`]。
struct SinkObserver<'a> {
    sink: &'a mut dyn DownloadSink,
}

impl DownloadObserver for SinkObserver<'_> {
    fn file_started(
        &mut self,
        file: &ModelFileSpec,
        index: usize,
        total: usize,
        declared_bytes: Option<u64>,
    ) -> bool {
        self.sink.file_started(file, index, total, declared_bytes)
    }

    fn bytes_written(&mut self, written_bytes: u64) {
        self.sink.bytes_written(written_bytes);
    }

    fn file_finished(&mut self, file: &ModelFileSpec, index: usize, bytes: u64) {
        self.sink.file_finished(file, index, bytes);
    }
}

/// `--allow-download-host` 的**唯一**校验实现（§6.1 第 3 条）。
///
/// 只接受裸主机名：带 scheme、端口、路径、userinfo、通配符或空白的取值都拒绝。
/// 失败带开关名与取值（可定位），并且**不会**被静默忽略——"以为加了白名单其实没生效"
/// 比启动失败危险得多。重复项按大小写不敏感去重（库的比较也是大小写不敏感的整串比较）。
pub(super) fn validate_extra_hosts(hosts: &[String]) -> Result<Vec<String>, ServeConfigError> {
    let mut out: Vec<String> = Vec::with_capacity(hosts.len());
    for host in hosts {
        let trimmed = host.trim();
        let reject = |reason: &str| ServeConfigError::new("--allow-download-host", host, reason);
        if trimmed.is_empty() {
            return Err(reject("the host must not be empty"));
        }
        if trimmed.len() != host.len() {
            return Err(reject("the host must not contain surrounding whitespace"));
        }
        if trimmed.contains(|c: char| {
            c.is_whitespace() || matches!(c, '/' | '\\' | ':' | '@' | '?' | '#' | '*' | ',')
        }) {
            return Err(reject(
                "the host must be a bare host name (no scheme, port, path, userinfo or wildcard)",
            ));
        }
        if !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        {
            return Err(reject("the host must be an ASCII host name"));
        }
        if trimmed.starts_with('.') || trimmed.ends_with('.') || trimmed.starts_with('-') {
            return Err(reject(
                "the host must not start or end with a dot or a dash",
            ));
        }
        if !out
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(trimmed))
        {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

/// 下载 worker 主循环。
pub(super) fn worker(
    runtime: Arc<ServeShared>,
    inbox: Receiver<DownloadCommand>,
    factory: DownloaderFactory,
) {
    let mut downloader = factory();
    loop {
        match inbox.recv_timeout(POLL_INTERVAL) {
            Ok(command) => run_one(&runtime, &command, downloader.as_mut()),
            Err(RecvTimeoutError::Timeout) => {
                if runtime.is_shutting_down() {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// 执行一条命令（任务状态的合法性由 `begin_download` 守卫）。
fn run_one(
    runtime: &Arc<ServeShared>,
    command: &DownloadCommand,
    downloader: &mut dyn ModelDownloader,
) {
    if runtime.is_shutting_down() {
        // 任务此刻还是 `Queued`：`Running → Failed` 会被存储拒绝，因此按 §4.3 的
        // "排队中取消"处理，而不是留下一个永远停在 `queued` 的任务。
        runtime.abandon_download(&command.job_id);
        return;
    }
    // 任务可能已经被取消（排队中的取消是可靠且立即的）：`begin_download` 会拒绝
    // 非 `Queued` 的任务，这里**如实忽略**而不是 panic，也不是把它改回排队。
    if runtime.begin_download(&command.job_id).is_err() {
        return;
    }
    let Some(job) = runtime.download_job(&command.set_id) else {
        // `POST /api/models/download` 已经按 id 解析过一次；执行期消失（清单被替换）时
        // 给可定位错误，**不**回落到别的集合。
        runtime.finish_download_failed(
            &command.job_id,
            ServeError::ModelSetNotFound {
                set_id: command.set_id.clone(),
                known: runtime.known_set_ids(),
            },
        );
        return;
    };
    let pending = runtime
        .pending_download(&command.set_id)
        .unwrap_or(PendingDownload {
            files: job.set.files.len(),
            bytes: None,
        });
    let mut sink = JobSink::new(Arc::clone(runtime), command.job_id.clone(), &job, pending);

    match downloader.download(&job, &mut sink) {
        Ok(_paths) => {
            // 取消在最后一个文件之后到达同样是取消请求：不能让它在无声中消失。
            if runtime.is_cancel_requested(&command.job_id) {
                runtime.finish_download_cancelled(&command.job_id);
            } else {
                runtime.finish_download_ok(&command.job_id);
            }
        }
        Err(RapidOcrError::Download(DownloadError::Cancelled)) => {
            runtime.finish_download_cancelled(&command.job_id);
        }
        Err(error) => runtime.finish_download_failed(&command.job_id, ServeError::from(error)),
    }
}

/// 把库的进度回调写进任务存储的接收端。
struct JobSink {
    runtime: Arc<ServeShared>,
    job_id: String,
    progress: DownloadProgress,
    /// 此前**已完成**文件的校验后字节之和（`bytes_done` = 它 + 当前文件的写入字节）。
    completed_bytes: u64,
    current_bytes: u64,
}

impl JobSink {
    fn new(
        runtime: Arc<ServeShared>,
        job_id: String,
        job: &DownloadJob<'_>,
        pending: PendingDownload,
    ) -> Self {
        Self {
            runtime,
            job_id,
            progress: job.initial_progress(pending),
            completed_bytes: 0,
            current_bytes: 0,
        }
    }

    /// 把当前进度写进任务存储（每次回调一次；锁的持有时间是 μs 级）。
    fn push(&self) {
        let mut progress = self.progress.clone();
        progress.bytes_done = self.completed_bytes.saturating_add(self.current_bytes);
        self.runtime.set_download_progress(&self.job_id, progress);
    }
}

impl DownloadSink for JobSink {
    fn file_started(
        &mut self,
        file: &ModelFileSpec,
        _index: usize,
        total: usize,
        _declared_bytes: Option<u64>,
    ) -> bool {
        // §6.6 的**唯一**取消检查点。
        if self.runtime.is_cancel_requested(&self.job_id) {
            return false;
        }
        self.progress.files_total = total;
        self.progress.current_file = Some(file.name.clone());
        self.current_bytes = 0;
        self.push();
        true
    }

    fn bytes_written(&mut self, written_bytes: u64) {
        self.current_bytes = written_bytes;
        self.push();
    }

    fn file_finished(&mut self, _file: &ModelFileSpec, index: usize, bytes: u64) {
        // 库的 `index` 是 1 基下标，因此它就是"已完成文件数"。
        self.progress.files_done = index;
        self.progress.current_file = None;
        self.completed_bytes = self.completed_bytes.saturating_add(bytes);
        self.current_bytes = 0;
        self.push();
    }
}

/// 下载超时（§6.1 第 11 条的分项超时没有 CLI 开关，用库的默认值；与库内调用方一致）。
pub(super) fn default_timeouts() -> (Duration, Duration) {
    (DEFAULT_CONNECT_TIMEOUT, DEFAULT_READ_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::validate_extra_hosts;

    #[test]
    fn only_bare_hosts_may_extend_the_allow_list() {
        let ok = validate_extra_hosts(&[
            "example.com".to_string(),
            "mirror.example.com".to_string(),
            " example.org ".to_string(),
        ]);
        // 前后空白是**配置错误**，不是需要猜测的输入。
        assert!(ok.is_err(), "{ok:?}");

        let good = validate_extra_hosts(&[
            "example.com".to_string(),
            "mirror.example.com".to_string(),
            "EXAMPLE.com".to_string(),
        ])
        .expect("bare hosts are valid");
        assert_eq!(
            good,
            vec!["example.com".to_string(), "mirror.example.com".to_string()],
            "case-insensitive duplicates are folded"
        );

        for bad in [
            "",
            "   ",
            "https://example.com",
            "example.com:443",
            "user@example.com",
            "example.com/models",
            "*.example.com",
            "exa mple.com",
            ".example.com",
            "-example.com",
            "例子.中国",
        ] {
            let error = validate_extra_hosts(&[bad.to_string()])
                .expect_err(&format!("`{bad}` must be rejected"));
            assert_eq!(error.field(), "--allow-download-host");
            assert!(
                error.to_string().contains("--allow-download-host"),
                "the error must locate the flag: {error}"
            );
        }
    }
}
