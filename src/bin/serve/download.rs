//! 下载 worker：独立的有界 channel + 独立线程（§8.1 的最后一行）。
//!
//! # M1 的范围（**如实说明**）
//!
//! 库侧的加固下载器已经在 M0b 落地（`model_store::download_verified`：仅 HTTPS、拒绝
//! 重定向、host 白名单、体积上限、唯一临时名、`MoveFileExW` 原子替换、单飞、空间预检）。
//! **本里程碑不做的是"把它接到 HTTP 上的下载任务"**——那是 `docs/05` §11 的 M2：
//! 下载任务进度、按集合的逐文件下载、文件边界的取消、以及下载完成后的惰性建引擎。
//!
//! 因此 M1 的这段接缝是：**启动一个真实的独立 worker 与有界 channel**，收到命令后
//! 如实把任务判为失败并给出可定位原因，**不发出任何网络请求、不写任何文件**。
//! 这样做的目的是：
//!
//! 1. 页面（§9）里的下载按钮**可见地降级**：`POST /api/models/download` 要么
//!    `403 downloads_disabled`（未开 `--allow-download`），要么创建一个真实的
//!    `kind=model_download` 任务并在下一次轮询时显示失败原因与"未实现"的事实，
//!    而不是静默成功或让页面崩掉；
//! 2. 线程模型（§8.1）与任务协议（§4.2 的 `GET /api/jobs/{id}`、
//!    `POST /api/jobs/{id}/cancel`）在 M1 就是**真的**：M2 只需要替换
//!    [`DownloadCommand`] 的处理体，不需要改动 worker/队列/任务的生命周期。
//!
//! 有界容量用编译期常量而不是新的 CLI 选项：§3 的选项清单是冻结的，M2 若要暴露
//! `--max-queue-download` 再按同样的校验方式加。

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use super::server::ServeShared;

/// 下载 channel 的有界容量（满 → 503 `busy`，与 OCR 队列同语义）。
pub(super) const DOWNLOAD_QUEUE_CAPACITY: usize = 4;

/// 下载 worker 的等待上限：只用于周期性检查关闭标志。
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 一条下载命令（M1 只有 M2 需要的两个字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DownloadCommand {
    pub job_id: String,
    pub set_id: String,
}

/// M1 的失败原因（写进 `JobView.error`，页面直接显示）。
pub(super) const NOT_IMPLEMENTED_REASON: &str = "model downloads are not wired up in this \
     milestone (docs/05 §11 M2); no network request was made and no file was written";

/// 下载 worker 主循环：M2 把 `NOT_IMPLEMENTED_REASON` 那一行换成真实的逐文件下载。
pub(super) fn worker(runtime: Arc<ServeShared>, inbox: Receiver<DownloadCommand>) {
    loop {
        match inbox.recv_timeout(POLL_INTERVAL) {
            Ok(DownloadCommand { job_id, .. }) => {
                if runtime.is_shutting_down() {
                    runtime.fail_download(&job_id, "serve is shutting down");
                    continue;
                }
                // 任务可能已经被取消：`begin_download` 会拒绝非 `Queued` 的任务，
                // 这里必须**如实忽略**而不是 panic。
                if runtime.begin_download(&job_id).is_err() {
                    continue;
                }
                runtime.fail_download(&job_id, NOT_IMPLEMENTED_REASON);
            }
            Err(RecvTimeoutError::Timeout) => {
                if runtime.is_shutting_down() {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
