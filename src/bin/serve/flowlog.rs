//! 流动日志：**每个 HTTP 请求一行** + **每个任务一条生命周期**，默认关闭。
//!
//! # 为什么需要它
//!
//! 报告者的症状是"上传之后左栏一直在转、右栏永远没有结果"，而 curl 驱动同一条
//! 流水线（`POST /api/ocr` → `GET /api/jobs/{id}` → `/result`）完全正常。也就是说
//! 故障发生在**两者之间**：要么请求根本没发出去，要么发出去之后某一步被拒绝。
//! 没有两端可对照的日志时，只能靠猜。本模块提供那次对照：
//!
//! - **请求行**（[`RequestFlow::response`]）：方法、路径、状态码、响应字节数、耗时、请求 id；
//! - **任务行**（[`job_admitted`] / [`job_running`] / [`job_terminal`]）：任务 id、队列、
//!   准入结论与它的原因、排队位置、等待时长、运行时长、终态、结果字节数，
//!   失败时还有状态码 + `code` + `detail`；
//! - 两者共用**同一个请求 id**：一次上传从请求行一路追到任务终态。
//!
//! # 默认关闭，且关闭时不付代价
//!
//! 级别只有两档（[`LogLevel::Off`] 默认 / [`LogLevel::Flow`]），由 `--log-level` 或
//! 环境变量 `RAPID_OCR_SERVE_LOG` 给出（CLI 优先，见 [`LogLevel::resolve`]）。
//! 关闭时出口是 [`FlowSink::Off`]：所有记录函数在第一行就返回，不做任何格式化。
//!
//! # 为什么有一个 `Capture` 出口
//!
//! 日志必须能被**断言**，否则"加了日志"这件事本身无法验证。测试用
//! [`FlowSink::Capture`] 把同样的行收进内存，而不是让测试去重写一套格式化逻辑——
//! 生产路径与测试路径经过的是同一个函数（docs/06 的流程轮记录逐行断言这些行）。

use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 流动日志级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LogLevel {
    /// 默认：请求行与任务行都不打印。
    #[default]
    Off,
    /// `flow`：每个 HTTP 请求一行，每个任务的生命周期若干行。
    Flow,
}

impl LogLevel {
    /// 文档/启动日志里显示的名字（与 `--log-level` 的取值逐字相同）。
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Flow => "flow",
        }
    }

    /// 解析一个级别取值。
    pub(super) fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "quiet" => Ok(Self::Off),
            "flow" | "info" | "verbose" => Ok(Self::Flow),
            other => Err(format!(
                "unknown log level `{other}`; expected `off` or `flow`"
            )),
        }
    }

    /// 生效级别：**`--log-level` > `RAPID_OCR_SERVE_LOG` > 默认 `off`**。
    ///
    /// 与 `--provider` / `--max-side` 无关：那两条的优先级是 CLI > YAML > 默认，而日志
    /// 级别没有 YAML 面（它不是引擎配置），因此只有两层。空字符串按"未给出"处理，
    /// 这样 `RAPID_OCR_SERVE_LOG=` 不会变成一条难以定位的启动期错误。
    pub(super) fn resolve(cli: Option<&str>, env: Option<&str>) -> Result<Self, String> {
        if let Some(raw) = cli.filter(|raw| !raw.trim().is_empty()) {
            return Self::parse(raw);
        }
        if let Some(raw) = env.filter(|raw| !raw.trim().is_empty()) {
            return Self::parse(raw);
        }
        Ok(Self::default())
    }
}

/// 流动日志的出口。
#[derive(Clone, Default)]
pub(super) enum FlowSink {
    /// 关闭：所有记录函数立即返回。
    #[default]
    Off,
    /// 生产路径：逐行写到 stderr（与 `serve:` 前缀的既有诊断行同一路，不引入新依赖）。
    Stderr,
    /// 测试路径：逐行留在内存里，供测试**逐行断言**。
    ///
    /// 只在测试构建里存在——发布的二进制里没有这条分支，因此"默认关闭时不付代价"
    /// 不只是"第一行就返回"，而是这段代码根本不存在。
    #[cfg(test)]
    Capture(Arc<Mutex<Vec<String>>>),
}

impl FlowSink {
    /// 由级别得到出口（生产路径）。
    pub(super) fn for_level(level: LogLevel) -> Self {
        match level {
            LogLevel::Off => Self::Off,
            LogLevel::Flow => Self::Stderr,
        }
    }

    /// 测试用：把行收进内存。
    #[cfg(test)]
    pub(super) fn capture() -> Self {
        Self::Capture(Arc::new(Mutex::new(Vec::new())))
    }

    /// 已经记录的每一行（`Capture` 之外返回空）。
    #[cfg(test)]
    pub(super) fn lines(&self) -> Vec<String> {
        match self {
            Self::Capture(lines) => lines
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
            _ => Vec::new(),
        }
    }

    /// 是否真的会写出东西（记录路径用它做零代价短路）。
    pub(super) fn enabled(&self) -> bool {
        !matches!(self, Self::Off)
    }

    /// 写一行。**唯一的**输出点：前缀、换行与目的地都只在这里决定。
    fn emit(&self, line: String) {
        match self {
            Self::Off => {}
            Self::Stderr => eprintln!("{line}"),
            #[cfg(test)]
            Self::Capture(lines) => lines
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(line),
        }
    }
}

impl std::fmt::Debug for FlowSink {
    /// 出口只以"开/关"出现在调试输出里：`Capture` 的内容不属于状态描述。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Off => "FlowSink::Off",
            Self::Stderr => "FlowSink::Stderr",
            #[cfg(test)]
            Self::Capture(_) => "FlowSink::Capture",
        })
    }
}

/// 请求 id 的分配器。进程内单调递增，从 1 开始（0 留给"未知/非请求"）。
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// 一次请求的流动日志：请求 id + 起点 + 方法与路径；响应写出时落一行。
///
/// 它被移交给**真正写响应的那个线程**（`serve-ocr` 之外的引擎工作线程、评估线程），
/// 因此耗时覆盖的是"从收到请求到写出响应"的整段时间，而不是 accept 线程那一段。
///
/// `Clone` 只用于"把同一份记录交给另一个线程"：克隆体与原件共享**同一个请求 id**，
/// 因此无论响应由哪个线程写出，这次请求的 id 都只有一个。
#[derive(Clone)]
pub(super) struct RequestFlow {
    id: u64,
    method: &'static str,
    path: String,
    started: Instant,
    sink: FlowSink,
}

impl RequestFlow {
    /// 开始记录一次请求。`sink` 关闭时不分配 id，也不记时间。
    pub(super) fn begin(sink: &FlowSink, method: &'static str, path: &str) -> Self {
        Self {
            id: NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
            method,
            path: path.to_string(),
            started: Instant::now(),
            sink: sink.clone(),
        }
    }

    /// 请求 id：任务生命周期行用它把一条上传串起来。
    pub(super) fn id(&self) -> u64 {
        self.id
    }

    /// 响应写出后落一行（状态码 + 响应字节数 + 耗时）。
    pub(super) fn response(&self, status: u16, bytes: usize) {
        if !self.sink.enabled() {
            return;
        }
        self.sink.emit(format!(
            "serve-flow: req={} {} {} -> {} bytes={} in {}ms",
            self.id,
            self.method,
            self.path,
            status,
            bytes,
            self.started.elapsed().as_millis()
        ));
    }

    /// 一次**任务准入**被拒绝（`/api/ocr` 在读 body 之前或之后被挡下）。
    ///
    /// 这条行没有任务 id：被拒绝的上传**没有**变成任务，这正是要如实写下来的事实。
    pub(super) fn job_rejected(&self, queue: &str, status: u16, code: &str, detail: &str) {
        if !self.sink.enabled() {
            return;
        }
        self.sink.emit(format!(
            "serve-flow: req={} queue={} admission=rejected status={} code={} detail={}",
            self.id,
            queue,
            status,
            code,
            one_line(detail)
        ));
    }
}

/// 一个任务在流动日志里的身份：**建任务时确定，之后不再变**。
///
/// 把这三样捆在一起而不是逐个当参数传，有两个实际好处：调用点不可能把某个任务的队列配到
/// 另一个任务的 id 上；行前缀也只有一处（[`JobIdentity::prefix`]），三条任务行因此不会各自
/// 拼各自的字段顺序。
#[derive(Debug, Clone, Copy)]
pub(super) struct JobIdentity<'a> {
    /// 创建这个任务的 HTTP 请求的流动日志 id。
    pub request: u64,
    pub job: &'a str,
    pub queue: &'a str,
}

impl JobIdentity<'_> {
    /// 三条任务行共用的前缀（`req=` / `job=` / `queue=` 的顺序只在这一处）。
    fn prefix(&self) -> String {
        format!(
            "serve-flow: req={} job={} queue={}",
            self.request, self.job, self.queue
        )
    }
}

/// 准入通过：任务已入队（`position` 是入队那一刻的队列位置）。
pub(super) fn job_admitted(sink: &FlowSink, id: JobIdentity<'_>, position: usize, decision: &str) {
    if !sink.enabled() {
        return;
    }
    sink.emit(format!(
        "{} admission=accepted decision={decision} queued position={position}",
        id.prefix()
    ));
}

/// 任务开始执行：`wait_ms` 是从入队到真正开跑的时间（排队代价）。
pub(super) fn job_running(sink: &FlowSink, id: JobIdentity<'_>, wait_ms: u64) {
    if !sink.enabled() {
        return;
    }
    sink.emit(format!("{} running wait_ms={wait_ms}", id.prefix()));
}

/// 任务终态。失败时带上状态码 + `code` + `detail`——"为什么失败"必须能直接从这一行读出来。
pub(super) fn job_terminal(
    sink: &FlowSink,
    id: JobIdentity<'_>,
    state: &str,
    result_bytes: u64,
    backend_ms: u64,
    failure: Option<(u16, &str, String)>,
) {
    if !sink.enabled() {
        return;
    }
    let outcome = match failure {
        Some((status, code, detail)) => {
            format!("status={status} code={code} detail={}", one_line(&detail))
        }
        None => format!("result_bytes={result_bytes}"),
    };
    sink.emit(format!(
        "{} terminal state={state} {outcome} backend_ms={backend_ms}",
        id.prefix()
    ));
}

/// 日志行必须**一行一条**：把 `detail` 里的换行与连续空白压平。
///
/// `detail` 来自 `serde_json::Value`（可能内嵌多行的错误文本），而一条流动日志被拆成
/// 多行会让"按行 grep 请求 id"这件事失效。压平是这里唯一允许的改写，内容不删减。
fn one_line(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|character| match character {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect();
    let mut out = String::with_capacity(flat.len());
    let mut last_space = false;
    for character in flat.chars() {
        let is_space = character == ' ';
        if is_space && last_space {
            continue;
        }
        last_space = is_space;
        out.push(character);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        FlowSink, JobIdentity, LogLevel, RequestFlow, job_admitted, job_running, job_terminal,
        one_line,
    };

    #[test]
    fn the_level_switch_is_off_unless_asked_for_explicitly() {
        assert_eq!(
            LogLevel::resolve(None, None).expect("default"),
            LogLevel::Off
        );
        assert_eq!(LogLevel::default(), LogLevel::Off);
        assert_eq!(LogLevel::parse("off").expect("off"), LogLevel::Off);
        assert_eq!(LogLevel::parse("FLOW").expect("flow"), LogLevel::Flow);
        // CLI 优先于环境变量（与 `--provider` 的"CLI > 其它"同一取向）。
        assert_eq!(
            LogLevel::resolve(Some("flow"), Some("off")).expect("cli wins"),
            LogLevel::Flow
        );
        assert_eq!(
            LogLevel::resolve(None, Some("flow")).expect("env when no cli"),
            LogLevel::Flow
        );
        // 空字符串 = 未给出，而不是一条启动期错误。
        assert_eq!(
            LogLevel::resolve(Some("  "), Some("")).expect("blank means unset"),
            LogLevel::Off
        );
        let error = LogLevel::resolve(Some("chatty"), None).expect_err("unknown level");
        assert!(error.contains("chatty"), "{error}");
        assert!(error.contains("off") && error.contains("flow"), "{error}");
    }

    /// 测试用的任务身份（`request` 通常来自 [`RequestFlow::id`]）。
    fn identity<'a>(request: u64, job: &'a str, queue: &'a str) -> JobIdentity<'a> {
        JobIdentity {
            request,
            job,
            queue,
        }
    }

    /// 关闭时**一行都不写**：`Off` 出口不接受任何记录。
    #[test]
    fn the_off_sink_records_nothing_at_all() {
        let sink = FlowSink::for_level(LogLevel::Off);
        assert!(!sink.enabled());
        let flow = RequestFlow::begin(&sink, "POST", "/api/ocr");
        flow.response(202, 334);
        flow.job_rejected("text", 409, "models_missing", "rec.onnx");
        job_admitted(&sink, identity(flow.id(), "job-1", "text"), 0, "run");
        job_running(&sink, identity(flow.id(), "job-1", "text"), 3);
        job_terminal(
            &sink,
            identity(flow.id(), "job-1", "text"),
            "succeeded",
            42,
            7,
            None,
        );
        assert!(sink.lines().is_empty());
    }

    /// 一次上传的完整轨迹：请求行 + 任务行，**同一个请求 id**。
    #[test]
    fn one_upload_is_followable_end_to_end_by_its_request_id() {
        let sink = FlowSink::capture();
        let flow = RequestFlow::begin(&sink, "POST", "/api/ocr");
        let request = flow.id();
        assert!(request > 0, "request ids start at 1");

        job_admitted(&sink, identity(request, "job-7", "text"), 0, "run");
        flow.response(202, 334);
        job_running(&sink, identity(request, "job-7", "text"), 4);
        job_terminal(
            &sink,
            identity(request, "job-7", "text"),
            "succeeded",
            35201,
            885,
            None,
        );

        let lines = sink.lines();
        assert_eq!(lines.len(), 4, "{lines:#?}");
        for line in &lines {
            assert!(
                line.contains(&format!("req={request}")),
                "every line of one upload must carry its request id: {line}"
            );
        }
        assert!(lines[0].contains("admission=accepted"), "{}", lines[0]);
        assert!(lines[0].contains("decision=run"), "{}", lines[0]);
        assert!(lines[0].contains("position=0"), "{}", lines[0]);
        assert_eq!(
            lines[1],
            format!("serve-flow: req={request} POST /api/ocr -> 202 bytes=334 in 0ms"),
            "the request line always ends with the measured duration"
        );
        assert!(lines[2].contains("running wait_ms=4"), "{}", lines[2]);
        assert!(
            lines[3].contains("terminal state=succeeded"),
            "{}",
            lines[3]
        );
        assert!(lines[3].contains("result_bytes=35201"), "{}", lines[3]);
        assert!(lines[3].contains("backend_ms=885"), "{}", lines[3]);
    }

    /// 失败任务：状态码、`code`、`detail` 都在同一行里（否则"为什么失败"要另找地方）。
    #[test]
    fn a_failed_job_carries_its_status_code_and_detail_on_one_line() {
        let sink = FlowSink::capture();
        let flow = RequestFlow::begin(&sink, "POST", "/api/ocr");
        flow.job_rejected("formula", 409, "models_corrupt", "pix2text-mfd-1.5.onnx");
        job_terminal(
            &sink,
            identity(flow.id(), "job-9", "formula"),
            "failed",
            12,
            30,
            Some((
                422,
                "unsupported_input",
                "decode failed\nsecond line".into(),
            )),
        );
        let lines = sink.lines();
        assert!(
            lines[0].contains("admission=rejected")
                && lines[0].contains("status=409")
                && lines[0].contains("code=models_corrupt")
                && lines[0].contains("detail=pix2text-mfd-1.5.onnx"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("state=failed")
                && lines[1].contains("status=422")
                && lines[1].contains("code=unsupported_input")
                && lines[1].contains("detail=decode failed second line"),
            "{}",
            lines[1]
        );
        // 一条日志行绝不允许多行：换行/制表符被压平，但内容不删减。
        assert!(!lines[1].contains('\n'), "{:?}", lines[1]);
        assert!(
            lines[1].contains("second line"),
            "content must survive: {}",
            lines[1]
        );
    }

    /// 请求 id 逐个递增：并发请求不会得到同一个 id（关联性靠它成立）。
    #[test]
    fn request_ids_are_unique_and_increasing() {
        let sink = FlowSink::for_level(LogLevel::Off);
        let first = RequestFlow::begin(&sink, "GET", "/api/status").id();
        let second = RequestFlow::begin(&sink, "GET", "/api/status").id();
        assert!(second > first, "{second} must be greater than {first}");
    }

    #[test]
    fn a_detail_is_flattened_onto_exactly_one_line() {
        assert_eq!(one_line("a\nb\tc"), "a b c");
        assert_eq!(one_line("  a   b  "), "a b");
        assert_eq!(one_line(""), "");
    }
}
