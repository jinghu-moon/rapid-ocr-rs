//! 端到端测试：**真实绑定的端口** + 原始 TCP 客户端（§12 的"协议/安全/队列"三行）。
//!
//! # 为什么用原始 TCP 而不是 HTTP 客户端
//!
//! 要验证的东西里有相当一部分是"客户端**故意**不规矩"时服务端的行为：错误的 `Host`
//! （防 DNS rebinding）、缺失/`null`/不匹配的 `Origin`、声明了但永不发送的
//! `Content-Length`（413 必须**在读 body 之前**给出）、错误的 `Content-Type`。
//! 这些用常规客户端库很难精确构造，而直接写字节既精确又不新增依赖。
//!
//! # 引擎为什么可以被脚本化
//!
//! 队列、准入、任务生命周期这些行为与"真实模型识别得对不对"无关，但真实引擎每张图要
//! 几百毫秒到数秒、还要 30 MB 模型在场。因此 [`ScriptedBackend`] 通过
//! `ServeContext::engine_factory`（见 `engine.rs` 的模块文档）替换引擎：
//! 生产路径永远是 `real_engine_factory`，脚本化只出现在本测试模块里。
//!
//! 模型目录由**本地 manifest** 构造（三个几十字节的假文件 + 真实 SHA-256），
//! 因此"模型齐备 → 引擎预加载 Ready"这条路径在测试里也走的是**真实**的
//! `ModelSource` / `ModelSet` / 单一来源规则，而不是被 stub 掉。
//!
//! # 公平性怎么驱动（§12 的双向要求）
//!
//! 引擎只有一个 worker，因此"另一个队列被灌满时本队列多久被服务"必须靠**慢速后端**观察：
//! `delay=25ms` 的脚本化后端 + `limit=0` 的滑动窗口。两个方向都跑：
//! 公式洪水下的普通任务、普通洪水下的公式任务，上界取调度器的
//! `wait_bound`（`capacity × 对方配额`）再乘上单任务耗时并留足余量。
//! 公式队列在 M1 生产路径上不可达（§10.8），因此这里显式把 `OcrRouting { formula: true }`
//! 打开，并用 `?queue=formula` 选择队列——这就是"测试专用慢速路径"的全部内容。

use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::download::{DownloadJob, DownloadSink, DownloaderFactory, ModelDownloader};
use super::engine::{BackendProvider, EngineFactory, OcrBackend};
use super::evaluate::EvalRoot;
use super::http::{BoundServer, ServeHandle};
use super::limits::RawServeLimits;
use super::model_plan::{FormulaDetectorSpec, ModelPlan};
use super::queue::QueueClass;
use super::run::render_page;
use super::security::{LocalOrigin, ServeToken, generate_nonce};
use super::server::{FreeSpaceFactory, OcrRouting, ServeContext, ServeShared};
use super::state::ServeStartup;
use rapid_ocr_rs::{
    CoordinateSpace, DetectionOutcome, DownloadError, EngineConfig, EngineInfo, FileIdentity,
    FormulaPolicy, GenericProviderPreference, ImageInfo, ImageSize, LangDet, LangRec,
    ModelFileSpec, ModelType, OcrOutput, OcrRegion, OcrRequest, OcrTimings, OcrVersion, Polygon,
    ProviderInfo, ProviderResolutionInfo, RapidOcrError, RecognitionOutcome, RegionSource,
    ResolvedProvider, StageReports, sha256_file,
};
use serde_json::{Value, json};

// ---------------------------------------------------------------- 测试基础设施

/// 脚本化后端的参数。
#[derive(Clone)]
struct Scripted {
    delay: Duration,
    regions: usize,
    text_bytes: usize,
    fail: bool,
    calls: Arc<AtomicUsize>,
    running: Arc<AtomicBool>,
    /// 本后端的 `selected_ep` 标签（M3 的 provider 切换靠它证明"换了哪个会话"）。
    ep: String,
    /// 每次识别是谁服务的（按顺序），用来断言"队列里的任务用了新引擎"。
    served: Arc<Mutex<Vec<String>>>,
    /// 每次识别收到的 `FormulaPolicy`（M4：公式队列真的把公式管线接上了）。
    formulas: Arc<Mutex<Vec<FormulaPolicy>>>,
    /// 可选的闸门：识别在返回**之前**阻塞，直到测试放行（M4 的单飞测试用它把
    /// "评估正在跑"变成确定性事实，而不是靠 sleep 猜时序）。
    gate: Option<Arc<Gate>>,
}

impl Scripted {
    fn fast() -> Self {
        Self {
            delay: Duration::from_millis(0),
            regions: 3,
            text_bytes: 0,
            fail: false,
            calls: Arc::new(AtomicUsize::new(0)),
            running: Arc::new(AtomicBool::new(false)),
            ep: "cpu".to_string(),
            served: Arc::new(Mutex::new(Vec::new())),
            formulas: Arc::new(Mutex::new(Vec::new())),
            gate: None,
        }
    }

    fn slow(delay: Duration) -> Self {
        Self {
            delay,
            ..Self::fast()
        }
    }

    /// 识别进入闸门（到达即通知测试，然后阻塞到放行）。
    fn with_gate(mut self, gate: Arc<Gate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// 最近的 `n` 次识别收到的公式策略（时间顺序）。
    fn formulas(&self) -> Vec<FormulaPolicy> {
        self.formulas
            .lock()
            .expect("the scripted recorder is not poisoned")
            .clone()
    }

    fn factory(self) -> EngineFactory {
        Arc::new(
            move |_config: &EngineConfig| -> Result<Box<dyn OcrBackend>, RapidOcrError> {
                Ok(Box::new(ScriptedBackend {
                    state: self.clone(),
                }))
            },
        )
    }
}

struct ScriptedBackend {
    state: Scripted,
}

impl OcrBackend for ScriptedBackend {
    fn recognize(&mut self, request: OcrRequest) -> Result<OcrOutput, RapidOcrError> {
        self.state.calls.fetch_add(1, Ordering::SeqCst);
        self.state
            .formulas
            .lock()
            .expect("the scripted recorder is not poisoned")
            .push(request.formula.clone());
        self.state
            .served
            .lock()
            .expect("the scripted recorder is not poisoned")
            .push(self.state.ep.clone());
        if let Some(gate) = &self.state.gate {
            gate.wait();
        }
        self.state.running.store(true, Ordering::SeqCst);
        if !self.state.delay.is_zero() {
            std::thread::sleep(self.state.delay);
        }
        self.state.running.store(false, Ordering::SeqCst);
        if self.state.fail {
            return Err(RapidOcrError::InvalidImage(
                "scripted failure: the image cannot be decoded".to_string(),
            ));
        }
        Ok(scripted_output(self.state.regions, self.state.text_bytes))
    }

    fn provider(&self) -> BackendProvider {
        BackendProvider {
            selected_ep: self.state.ep.clone(),
            fallback_to_cpu: false,
        }
    }
}

/// 一份结构完整、可被 `to_output_json` 序列化的输出。
///
/// `pub(crate)`：`results.rs` 的单元测试也用它构造成功载荷（同一份样例，避免两处各写一套）。
pub(crate) fn scripted_output(regions: usize, text_bytes: usize) -> OcrOutput {
    let provider = ProviderResolutionInfo {
        requested: GenericProviderPreference::Cpu,
        selected_ep: ResolvedProvider::Cpu,
        fallback_to_cpu: false,
    };
    let built = (0..regions)
        .map(|index| {
            let text = if text_bytes == 0 {
                format!("region-{index}")
            } else {
                "x".repeat(text_bytes)
            };
            OcrRegion::text(
                RegionSource::Detected {
                    detector_index: index,
                },
                Some(Polygon {
                    points: [[1.0, 1.0], [10.0, 1.0], [10.0, 5.0], [1.0, 5.0]],
                }),
                Some(DetectionOutcome { score: 0.9 }),
                None,
                Some(RecognitionOutcome {
                    text,
                    score: 0.9,
                    words: None,
                }),
            )
        })
        .collect();
    OcrOutput {
        schema_version: 1,
        image: ImageInfo {
            original_size: ImageSize {
                width: 100,
                height: 50,
            },
            processed_size: ImageSize {
                width: 100,
                height: 50,
            },
            coordinate_space: CoordinateSpace::Image,
        },
        stages: StageReports::default(),
        regions: built,
        timings: OcrTimings {
            total_ms: 1.0,
            ..OcrTimings::default()
        },
        engine: EngineInfo {
            model_id: "scripted".to_string(),
            provider: ProviderInfo {
                detector: provider,
                classifier: None,
                recognizer: provider,
            },
        },
    }
}

/// 测试运行时的组装参数。
struct TestOptions {
    limits: RawServeLimits,
    allow_download: bool,
    allow_download_hosts: Vec<String>,
    /// `--allow-provider-fallback`（§7.5）：运行期切换 provider 会用同一个值。
    allow_provider_fallback: bool,
    routing: OcrRouting,
    /// `--formula-detector`（M4）；路由打开时它必须有值，否则 `FormulaPolicy` 会是
    /// "只处理显式区域"的退化形状。它是**路径 + 集合声明的哈希**（评审 P1-1）。
    formula_detector: Option<FormulaDetectorSpec>,
    /// `--eval-root` 沙箱（评审 P2-3）：默认指向测试模型目录（评估夹具就写在那里）。
    eval_root: Option<EvalRoot>,
    engine_factory: EngineFactory,
    downloader: DownloaderFactory,
    free_space: FreeSpaceFactory,
    model_dir: PathBuf,
    engine_config: EngineConfig,
    /// A2 的确定性钩子：**模型冷验证返回之后、响应写出之前**调用一次（原始 HTTP 层用）。
    ///
    /// 单飞（"第二个并发调用得到 503 `busy`"）必须被确定性地观察到，而不是靠 sleep 猜时序：
    /// 有了它，测试可以在"第一个调用已经写完校验结论、但还没有回响应"的**精确窗口**里
    /// 发第二个请求。
    post_verify: Option<Box<dyn Fn() + Send + Sync>>,
}

impl TestOptions {
    fn new(model_dir: PathBuf, scripted: Scripted) -> Self {
        let eval_root = EvalRoot::new(&model_dir).ok();
        Self {
            limits: RawServeLimits::default(),
            allow_download: false,
            allow_download_hosts: Vec::new(),
            allow_provider_fallback: false,
            routing: OcrRouting::text_only(
                "test server: no --formula-detector, so the formula queue is refused (§10.8)",
            ),
            formula_detector: None,
            eval_root,
            engine_factory: scripted.factory(),
            downloader: ScriptedDownload::default().factory(),
            free_space: Arc::new(|_dir: &Path| Ok(1 << 40)),
            model_dir,
            engine_config: test_engine_config(),
            post_verify: None,
        }
    }

    /// 打开公式路由（M4）：检测模型路径在场，公式队列因此可达。
    ///
    /// `detector` 只需是一个路径：脚本化后端不加载它（真实启动期的存在性校验在 `run.rs`），
    /// 但 `FormulaPolicy.detector_path` 必须真的有值——否则接上的就是"只处理显式区域"的
    /// 退化形状，公式任务会静默地一个区域都产不出来。
    ///
    /// **文件必须真的在磁盘上**：评审 P1-2 之后，公式队列的准入按检测模型的哈希状态判定，
    /// "配置了检测模型但文件不在"同样是 409。`run.rs` 在启动期就拒绝这种配置，因此测试
    /// 夹具也复现这个不变量（缺文件时写一份占位内容）。
    fn with_formula_routing(mut self, detector: &Path) -> Self {
        if !detector.is_file() {
            let name = detector
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "detector.onnx".to_string());
            std::fs::write(detector, model_file_bytes(&name)).expect("write the detector fixture");
        }
        self.routing = OcrRouting::formula_enabled();
        self.formula_detector = Some(FormulaDetectorSpec {
            path: detector.to_path_buf(),
            expected_sha256: None,
        });
        self
    }

    /// 与 [`Self::with_formula_routing`] 相同，但检测模型带**集合声明的** SHA-256
    /// （评审 P1-1：损坏的检测模型必须在准入/加载时被抓住）。
    fn with_verified_formula_detector(mut self, detector: &Path, sha256: &str) -> Self {
        self.routing = OcrRouting::formula_enabled();
        self.formula_detector = Some(FormulaDetectorSpec {
            path: detector.to_path_buf(),
            expected_sha256: Some(sha256.to_string()),
        });
        self
    }

    /// 关闭评估沙箱（评审 P2-3：不给 `--eval-root` 时端点必须拒绝）。
    fn without_eval_root(mut self) -> Self {
        self.eval_root = None;
        self
    }

    /// 覆盖队列容量/配额（§8.3 的公平性测试用）。
    fn with_limits(mut self, limits: RawServeLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// 与 `OCR-Model/test-config-small.yaml` 一致的文本管线选择（PP-OCRv6 small / multi+ch）。
///
/// 用它而不是 `EngineConfig::default()`：`/api/models` 的集合由这个选择决定，而
/// 默认表的 v4 条目没有 `size_bytes`（M0a 记录），会让"每个文件都报告体积"的断言
/// 变成在测另一件事。
fn test_engine_config() -> EngineConfig {
    let mut config = EngineConfig::default();
    config.det.ocr_version = OcrVersion::PPocrV6;
    config.det.model_type = ModelType::Small;
    config.det.lang = LangDet::Multi;
    config.rec.model.ocr_version = OcrVersion::PPocrV6;
    config.rec.model.model_type = ModelType::Small;
    config.rec.model.lang = LangRec::Ch;
    config
}

/// 一个真实运行的服务实例（Drop 时关闭）。
struct TestServer {
    handle: Option<ServeHandle>,
    addr: SocketAddr,
    token: String,
    host: String,
    origin: String,
}

impl TestServer {
    fn start(options: TestOptions) -> Self {
        let bound = BoundServer::bind(0).expect("an ephemeral port must bind");
        let addr = bound.local_addr();
        let local = LocalOrigin::from_bound(addr).expect("loopback");
        let token = ServeToken::generate().expect("the OS CSPRNG must be available");
        let nonce = generate_nonce().expect("the OS CSPRNG must be available");
        let page = render_page(&token, &nonce).expect("the page must inject cleanly");
        let startup = ServeStartup::validate(
            options.limits,
            options.engine_config,
            None,
            None,
            options.allow_provider_fallback,
        )
        .expect("the test limits must be valid");
        let model_plan =
            ModelPlan::resolve(&options.model_dir, &startup.plan.engine).expect("model plan");
        let snapshot = model_plan.snapshot();
        let post_verify = options.post_verify;
        let context = ServeContext {
            limits: startup.limits,
            plan: startup.plan,
            model_plan,
            snapshot,
            token,
            local,
            page,
            nonce,
            allow_download: options.allow_download,
            allow_download_hosts: options.allow_download_hosts,
            // §7.5：`ServeStartup::validate` 用的是同一个开关，运行期切换 provider 也用它。
            allow_provider_fallback: options.allow_provider_fallback,
            routing: options.routing,
            formula_detector: options.formula_detector,
            eval_root: options.eval_root,
            engine_factory: options.engine_factory,
            downloader: options.downloader,
            free_space: options.free_space,
            post_verify,
        };
        let handle = ServeHandle::start(bound, context).expect("the runtime must start");
        let token = handle.shared().token().as_str().to_string();
        Self {
            handle: Some(handle),
            addr,
            token,
            host: format!("127.0.0.1:{}", addr.port()),
            origin: format!("http://127.0.0.1:{}", addr.port()),
        }
    }

    fn shared(&self) -> Arc<ServeShared> {
        Arc::clone(self.handle.as_ref().expect("running").shared())
    }

    /// 一次 GET（默认带 token 与 Host）。
    fn get(&self, path: &str) -> RawResponse {
        self.request(
            "GET",
            path,
            &[
                ("Host", self.host.as_str()),
                ("X-RapidOCR-Token", self.token.as_str()),
            ],
            None,
        )
    }

    /// 一次 POST，带 token + Host + Origin（除非显式覆盖）。
    fn post(&self, path: &str, headers: &[(&str, &str)], body: &[u8]) -> RawResponse {
        let mut all = vec![
            ("Host", self.host.as_str()),
            ("X-RapidOCR-Token", self.token.as_str()),
            ("Origin", self.origin.as_str()),
        ];
        all.extend_from_slice(headers);
        self.request("POST", path, &all, Some(body))
    }

    /// 一次原始请求（`headers` 里可以重复/覆盖默认头；`body` 为 `None` 时不发送 body，
    /// 但 `Content-Length` 由调用方在 headers 里显式给出）。
    ///
    /// 每个请求都带 `Connection: close`：响应因此一定以关闭连接结束，测试客户端不必
    /// 依赖 `Content-Length`（`tiny_http` 对 >32 KiB 的已知长度响应用 chunked，
    /// 内联页面正是这种）。
    fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> RawResponse {
        raw_request(self.addr, method, path, headers, body)
    }

    fn submit_ocr(&self, body: &[u8]) -> RawResponse {
        self.post(
            "/api/ocr",
            &[("Content-Type", "application/octet-stream")],
            body,
        )
    }

    /// `POST /api/models/download`：请求体**只有** `set_id`（§7.2 禁止 URL）。
    fn download(&self, set_id: &str) -> RawResponse {
        self.post(
            "/api/models/download",
            &[("Content-Type", "application/json")],
            format!("{{\"set_id\":\"{set_id}\"}}").as_bytes(),
        )
    }

    /// 轮询任务直到终态（或超时）。
    fn wait_terminal(&self, id: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let response = self.get(&format!("/api/jobs/{id}"));
            assert_eq!(response.status, 200, "job view: {}", response.text());
            let value = response.json();
            let state = value["state"].as_str().unwrap_or_default().to_string();
            if matches!(state.as_str(), "succeeded" | "failed" | "cancelled") {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "job {id} did not finish within {timeout:?}: {value}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // `ServeHandle` 的 `Drop` 负责 unblock + join；这里只是让它确定地发生。
        self.handle.take();
    }
}

/// 极简 HTTP 响应。
struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|error| panic!("body is not JSON ({error}): {}", self.text()))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn code(&self) -> String {
        self.json()["code"].as_str().unwrap_or_default().to_string()
    }
}

/// 一次原始请求的自由函数形式（**并发**测试用：另一个线程只需要有 `addr` 就能发请求，
/// 而不必借用 `TestServer`。M3 的 provider 切换请求会一直阻塞到切换序列结束，
/// 因此它必须在后台线程里发出）。
///
/// 每个请求都带 `Connection: close`：响应因此一定以关闭连接结束，测试客户端不必
/// 依赖 `Content-Length`（`tiny_http` 对 >32 KiB 的已知长度响应用 chunked，
/// 内联页面正是这种）。
fn raw_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> RawResponse {
    let mut raw = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    for (name, value) in headers {
        raw.push_str(&format!("{name}: {value}\r\n"));
    }
    match body {
        Some(body) => {
            raw.push_str(&format!("Content-Length: {}\r\n", body.len()));
            raw.push_str("\r\n");
            let mut bytes = raw.into_bytes();
            bytes.extend_from_slice(body);
            send(addr, &bytes)
        }
        None => {
            raw.push_str("\r\n");
            send(addr, raw.as_bytes())
        }
    }
}

/// 直接把请求字节写进 socket，读到连接关闭，再按响应的定界方式取出 body。
fn send(addr: SocketAddr, raw: &[u8]) -> RawResponse {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    stream.write_all(raw).expect("write request");
    stream.flush().expect("flush");

    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) => panic!("reading the response failed: {error}"),
        }
    }
    parse_response(&buffer)
}

/// 把一份完整的原始响应字节解析成 [`RawResponse`]。
///
/// 抽成自由函数的理由：A2 的单飞用例必须在**同一个连接**上"先写头、再写体"，
/// 因此它自己读字节，然后复用这里的定界逻辑（不能有两套解析）。
fn parse_response(buffer: &[u8]) -> RawResponse {
    let head_end = find(buffer, b"\r\n\r\n").expect("the response must contain a header block") + 4;
    let head = String::from_utf8_lossy(&buffer[..head_end - 4]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("cannot parse the status line: {status_line:?}"));
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();
    let raw_body = buffer.get(head_end..).unwrap_or_default();

    let header = |name: &str| -> Option<String> {
        headers
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    let body = match header("Transfer-Encoding") {
        Some(value) if value.to_ascii_lowercase().contains("chunked") => decode_chunked(raw_body),
        _ => match header("Content-Length").and_then(|value| value.parse::<usize>().ok()) {
            Some(length) => raw_body.get(..length).unwrap_or(raw_body).to_vec(),
            None => raw_body.to_vec(),
        },
    };
    RawResponse {
        status,
        headers,
        body,
    }
}

/// 解 chunked 响应体（`tiny_http` 对 >32 KiB 的已知长度响应会用 chunked）。
fn decode_chunked(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(position) = find(rest, b"\r\n") {
        let size_line = String::from_utf8_lossy(&rest[..position]).into_owned();
        let size_text = size_line.split(';').next().unwrap_or_default().trim();
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            break;
        };
        rest = &rest[position + 2..];
        if size == 0 {
            break;
        }
        let Some(chunk) = rest.get(..size) else {
            out.extend_from_slice(rest);
            break;
        };
        out.extend_from_slice(chunk);
        rest = rest.get(size + 2..).unwrap_or_default();
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ------------------------------------------------ M2 的下载接缝（无网络）

/// 测试模型文件的内容：**由文件名唯一决定**。
///
/// 模型目录的 manifest 用它的真实 SHA-256 声明哈希，脚本化下载器写出同样的字节，
/// 因此"下载完成后 `/api/models` 真的变成 `complete`"走的是库的真实哈希校验，
/// 而不是把状态标记成"完成"。
fn model_file_bytes(name: &str) -> Vec<u8> {
    format!("M2 scripted model file {name}").into_bytes()
}

/// 一个用本地 manifest 描述的模型目录：
/// `det.onnx` / `rec.onnx` / `dict.txt`（文本管线）+ `fx.onnx`（`formula_recognizer`）。
///
/// M4 起**每个**清单来源都必须声明公式 role：`serve` 的模型清单请求
/// `ModelRequest::text_and_formula`（页面要能报告公式集合的体积并让用户按集合下载），
/// 因此缺 role 是一个可定位的启动期错误（§5.3），不是静默降级。
///
/// `present` 里的文件真的写到磁盘上（内容 = [`model_file_bytes`]），其余缺失；
/// `declared_size` 为 `Some` 时每个文件的 `size_bytes` 都写成它（用于构造
/// "声明体积超过预算/磁盘空间"的场景，而文件本身仍然很小）。
fn manifest_model_dir(name: &str, present: &[&str], declared_size: Option<u64>) -> PathBuf {
    let dir = m2_root().join(format!("manifest-{name}-{}", unique()));
    std::fs::create_dir_all(&dir).expect("create the model dir");
    let files = model_fixture_files();
    let mut manifest = String::from(
        "{\"schema_version\":1,\"id\":\"test-set\",\"family\":\"PP-OCR\",\"version\":\"v-test\",\
         \"languages\":[\"en\"],\"files\":[",
    );
    for (index, (file, role)) in files.iter().enumerate() {
        let bytes = model_file_bytes(file);
        let path = dir.join(file);
        std::fs::write(&path, &bytes).expect("write the model file");
        let sha = sha256_file(&path).expect("hash the model file");
        if index > 0 {
            manifest.push(',');
        }
        let size = declared_size.unwrap_or(bytes.len() as u64);
        manifest.push_str(&format!(
            "{{\"name\":\"{file}\",\"role\":\"{role}\",\"sha256\":\"{sha}\",\"size_bytes\":{size},\
             \"source_url\":\"https://www.modelscope.cn/models/{file}\"}}"
        ));
    }
    manifest.push_str("]}");
    std::fs::write(dir.join("manifest.json"), manifest).expect("write the manifest");
    for (file, _) in files {
        if !present.contains(&file) {
            std::fs::remove_file(dir.join(file)).expect("remove the absent file");
        }
    }
    dir
}

/// 测试清单描述的四个文件（M4 起包含公式识别模型）。
///
/// 文本三个 + 公式一个：这样**同一个集合**里就有两条管线的 role，
/// "按 role 分组判定就绪"这条规则在本地清单来源下也被真实地走到了。
fn model_fixture_files() -> [(&'static str, &'static str); 4] {
    [
        ("det.onnx", "detector"),
        ("rec.onnx", "recognizer"),
        ("dict.txt", "dictionary"),
        ("fx.onnx", "formula_recognizer"),
    ]
}

/// 测试清单里的文本管线文件（`present` 参数里最常用的那一组）。
fn text_fixture_files() -> [&'static str; 3] {
    ["det.onnx", "rec.onnx", "dict.txt"]
}

/// 与 [`manifest_model_dir`] 相同，但公式识别模型**已经在场**。
///
/// M2 的下载用例考察的是**文本集合**的进度、预算、磁盘核算与取消；公式文件（566 MB，
/// 在真实场景里是一个独立的下载动作）不该参与这些数字。M4 起每个清单都必须声明
/// `formula_recognizer` role（否则启动期就报缺 role），因此这里把它放成 `present`：
/// 它不出现在"待下载"集合里，M2 的数字仍然逐字成立。
fn manifest_model_dir_text_only(
    name: &str,
    present: &[&str],
    declared_size: Option<u64>,
) -> PathBuf {
    let mut files: Vec<&str> = present.to_vec();
    files.push("fx.onnx");
    manifest_model_dir(name, &files, declared_size)
}

/// M2 fixture 的根目录（与 M1 的 `target/m1-serve-tests` 分开，避免互相干扰）。
fn m2_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/m2-serve-tests")
}

/// 一个"到点即停"的闸门：脚本化下载器在某个文件开始后阻塞，直到测试放行。
///
/// 有了它，"运行中的进度"与"运行中取消"就是**确定性**的断言，而不是靠 sleep 猜时序。
/// 两条 channel 的容量都是 0：`send` 会一直阻塞到对端 `recv`。
struct Gate {
    reached: std::sync::mpsc::SyncSender<()>,
    reached_rx: Mutex<std::sync::mpsc::Receiver<()>>,
    release: std::sync::mpsc::SyncSender<()>,
    release_rx: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl Gate {
    fn new() -> Arc<Self> {
        let (reached, reached_rx) = std::sync::mpsc::sync_channel(0);
        let (release, release_rx) = std::sync::mpsc::sync_channel(0);
        Arc::new(Self {
            reached,
            reached_rx: Mutex::new(reached_rx),
            release,
            release_rx: Mutex::new(release_rx),
        })
    }

    /// 下载器侧：通知测试"到达"并阻塞到放行。
    fn wait(&self) {
        let _ = self.reached.send(());
        let release = self.release_rx.lock().expect("the gate is not poisoned");
        let _ = release.recv();
    }

    /// 测试侧：等到下载器到达闸门。
    fn arrive(&self) {
        let reached = self.reached_rx.lock().expect("the gate is not poisoned");
        let _ = reached.recv();
    }

    /// 测试侧：放行一次。
    fn release(&self) {
        let _ = self.release.send(());
    }
}

/// 脚本化下载器里"一个待下载文件"的动作（不足时复用最后一个；空计划 = 全部写入）。
#[derive(Debug, Clone)]
enum Step {
    /// 把 [`model_file_bytes`] 写到模型目录（因此哈希校验真的会通过）。
    Write,
    /// 报告哈希不匹配（文件**不**落盘，模拟库侧删除临时文件后的结论）。
    HashMismatch,
    /// 报告传输失败。
    Network,
    /// 报告在文件边界取消。
    Cancelled,
}

/// 脚本化下载器的共享状态（工厂每次调用都克隆它，因此多次任务共享记录）。
#[derive(Clone)]
struct ScriptedDownload {
    plan: Vec<Step>,
    /// 每次任务收到的**显式** host 允许列表（证明它是参数而不是被改掉的常量）。
    seen_hosts: Arc<Mutex<Vec<Vec<String>>>>,
    /// 真正写完的文件（按顺序），用于断言"取消后停在哪"。
    written: Arc<Mutex<Vec<String>>>,
    /// 每个文件开始后阻塞一次（可选）。
    gate: Option<Arc<Gate>>,
}

impl Default for ScriptedDownload {
    fn default() -> Self {
        Self {
            plan: Vec::new(),
            seen_hosts: Arc::new(Mutex::new(Vec::new())),
            written: Arc::new(Mutex::new(Vec::new())),
            gate: None,
        }
    }
}

impl ScriptedDownload {
    fn with_plan(plan: Vec<Step>) -> Self {
        Self {
            plan,
            ..Self::default()
        }
    }

    fn with_gate(gate: Arc<Gate>) -> Self {
        Self {
            gate: Some(gate),
            ..Self::default()
        }
    }

    fn step(&self, index: usize) -> Step {
        if self.plan.is_empty() {
            return Step::Write;
        }
        self.plan
            .get(index)
            .cloned()
            .unwrap_or_else(|| self.plan.last().cloned().expect("a non-empty plan"))
    }

    fn factory(self) -> DownloaderFactory {
        Arc::new(move || {
            Box::new(ScriptedDownloader {
                state: self.clone(),
            })
        })
    }

    fn hosts_per_call(&self) -> Vec<Vec<String>> {
        self.seen_hosts.lock().expect("not poisoned").clone()
    }

    fn written(&self) -> Vec<String> {
        self.written.lock().expect("not poisoned").clone()
    }
}

/// 把 [`ScriptedDownload`] 的计划演成一个 [`ModelDownloader`]。
///
/// 它**只**替代"网络 + 落盘"这一段：待下载文件如何选出（`state_in` 的真实实现）、
/// 进度回调、取消检查点、任务状态机、`/api/models` 的哈希校验全部是生产代码。
struct ScriptedDownloader {
    state: ScriptedDownload,
}

impl ModelDownloader for ScriptedDownloader {
    fn download(
        &mut self,
        job: &DownloadJob<'_>,
        sink: &mut dyn DownloadSink,
    ) -> Result<Vec<PathBuf>, RapidOcrError> {
        self.state
            .seen_hosts
            .lock()
            .expect("not poisoned")
            .push(job.allowed_hosts.clone());
        let pending: Vec<ModelFileSpec> = job
            .set
            .files
            .iter()
            .filter(|file| !file.state_in(job.root).is_present())
            .cloned()
            .collect();
        let total = pending.len();
        let mut paths = Vec::with_capacity(total);
        for (offset, file) in pending.iter().enumerate() {
            let index = offset + 1;
            // 生产代码里的**唯一**取消检查点（由 `JobSink` 查询任务存储）。
            if !sink.file_started(file, index, total, file.size_bytes) {
                return Err(DownloadError::Cancelled.into());
            }
            if let Some(gate) = &self.state.gate {
                gate.wait();
            }
            match self.state.step(offset) {
                Step::Write => {
                    let bytes = model_file_bytes(&file.name);
                    std::fs::write(job.root.join(&file.name), &bytes)
                        .expect("the scripted download must be able to write the model file");
                    // 分两块上报：进度是"当前文件的累计值"（与库的分块读取一致）。
                    sink.bytes_written(bytes.len() as u64 / 2);
                    sink.bytes_written(bytes.len() as u64);
                    sink.file_finished(file, index, bytes.len() as u64);
                    self.state
                        .written
                        .lock()
                        .expect("not poisoned")
                        .push(file.name.clone());
                    paths.push(job.root.join(&file.name));
                }
                Step::HashMismatch => {
                    sink.bytes_written(1);
                    return Err(DownloadError::HashMismatch {
                        expected: file.sha256.clone(),
                        actual: "00".repeat(32),
                    }
                    .into());
                }
                Step::Network => {
                    return Err(DownloadError::Network {
                        detail: "scripted transport failure".to_string(),
                    }
                    .into());
                }
                Step::Cancelled => return Err(DownloadError::Cancelled.into()),
            }
        }
        Ok(paths)
    }
}

/// 空的模型目录（默认表来源，全部文件缺失）。
fn empty_model_dir(name: &str) -> PathBuf {
    let dir = test_root().join(format!("empty-{name}-{}", unique()));
    std::fs::create_dir_all(&dir).expect("create the empty model dir");
    dir
}

/// 齐备的模型目录：本地 `manifest.json` + 四个内容为 `name` 的文件（SHA-256 真实计算）。
///
/// 四个 = 文本三个 + `fx.onnx`（`formula_recognizer`）：M4 起"齐备"是**两条管线**都齐备，
/// 否则 `/api/models` 的 `formula.complete` 会是假、页面上的公式开关会被正确地禁用。
fn complete_model_dir(name: &str) -> PathBuf {
    let dir = test_root().join(format!("complete-{name}-{}", unique()));
    std::fs::create_dir_all(&dir).expect("create the model dir");
    let files = model_fixture_files();
    let mut manifest = String::from(
        "{\"schema_version\":1,\"id\":\"test-set\",\"family\":\"PP-OCR\",\"version\":\"v-test\",\
         \"files\":[",
    );
    for (index, (file, role)) in files.iter().enumerate() {
        let path = dir.join(file);
        std::fs::write(&path, format!("test model file {file}")).expect("write the model file");
        let sha = sha256_file(&path).expect("hash the model file");
        if index > 0 {
            manifest.push(',');
        }
        manifest.push_str(&format!(
            "{{\"name\":\"{file}\",\"role\":\"{role}\",\"sha256\":\"{sha}\",\"size_bytes\":18}}"
        ));
    }
    manifest.push_str("]}");
    std::fs::write(dir.join("manifest.json"), manifest).expect("write the manifest");
    dir
}

fn test_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/m1-serve-tests")
}

fn unique() -> u64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

fn limits(text: usize, formula: usize) -> RawServeLimits {
    RawServeLimits {
        max_queue_text: text,
        max_queue_formula: formula,
        ..RawServeLimits::default()
    }
}

/// 等待某个条件（用于把测试的时序断言写成"有上界"而不是"恰好"）。
fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

// ---------------------------------------------------------------- 页面与安全

#[test]
fn the_page_carries_the_nonce_csp_and_the_frozen_security_headers() {
    let server = TestServer::start(TestOptions::new(empty_model_dir("page"), Scripted::fast()));
    // 浏览器请求 `GET /` 时**不会**带令牌（§7.2：令牌只对 /api/* 必需）——
    // 这正是页面把令牌注入 `window.__RROCR__` 的原因。
    let response = server.request("GET", "/", &[("Host", server.host.as_str())], None);

    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(
        response.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(response.header("X-Content-Type-Options"), Some("nosniff"));
    assert_eq!(response.header("Referrer-Policy"), Some("no-referrer"));
    assert_eq!(response.header("Cache-Control"), Some("no-store"));
    assert!(
        response.header("Access-Control-Allow-Origin").is_none(),
        "CORS headers must never be sent"
    );

    let csp = response
        .header("Content-Security-Policy")
        .expect("CSP header");
    assert!(!csp.contains("unsafe-inline"), "{csp}");
    assert!(csp.contains("script-src 'nonce-"), "{csp}");
    assert!(csp.contains("style-src 'nonce-"), "{csp}");
    // nonce 与页面里的每一个 nonce 属性逐字节相同。
    let nonce = csp
        .split("script-src 'nonce-")
        .nth(1)
        .and_then(|rest| rest.split('\'').next())
        .expect("a nonce in the CSP");
    assert_eq!(nonce, server.shared().nonce());
    let page = response.text();
    assert!(
        !page.contains("__CSP_NONCE__"),
        "no placeholder may survive"
    );
    assert!(!page.contains("__SRV_TOKEN__"));
    assert_eq!(page.matches(&format!("nonce=\"{nonce}\"")).count(), 3);
    assert_eq!(page.matches(&server.token).count(), 3);
}

#[test]
fn the_token_is_required_on_every_api_route() {
    let server = TestServer::start(TestOptions::new(empty_model_dir("token"), Scripted::fast()));
    // 没有令牌时 `GET /` 仍然可用（它是令牌的下发者），而每个 /api/* 都是 401。
    assert_eq!(
        server
            .request("GET", "/", &[("Host", server.host.as_str())], None)
            .status,
        200
    );
    for path in ["/api/status", "/api/models", "/api/jobs/job-0"] {
        let response = server.request("GET", path, &[("Host", server.host.as_str())], None);
        assert_eq!(response.status, 401, "{path}: {}", response.text());
        assert_eq!(response.code(), "unauthorized", "{path}");
        // 错误体形状固定为三键。
        let body = response.json();
        assert!(body.get("code").is_some() && body.get("message").is_some());
        assert!(body.get("detail").is_some(), "detail must exist: {body}");
    }
    // 错误的 token 同样是 401（不是 403 / 404）。
    let response = server.request(
        "GET",
        "/api/status",
        &[
            ("Host", server.host.as_str()),
            ("X-RapidOCR-Token", "not-the-token"),
        ],
        None,
    );
    assert_eq!(response.status, 401);
}

#[test]
fn the_host_header_is_validated_for_dns_rebinding() {
    let server = TestServer::start(TestOptions::new(empty_model_dir("host"), Scripted::fast()));
    let token = server.token.clone();
    for host in [
        "evil.example:8760",
        "127.0.0.1:1",
        "[::1]:8760",
        "localhost",
        "127.0.0.10:1",
    ] {
        let response = server.request(
            "GET",
            "/api/status",
            &[("Host", host), ("X-RapidOCR-Token", token.as_str())],
            None,
        );
        assert_eq!(response.status, 421, "Host: {host} → {}", response.text());
        assert_eq!(response.code(), "bad_host");
    }
    // 没有 Host 头也是 421。
    let response = server.request(
        "GET",
        "/api/status",
        &[("X-RapidOCR-Token", token.as_str())],
        None,
    );
    assert_eq!(response.status, 421);
}

#[test]
fn state_changing_requests_require_a_matching_origin() {
    let server = TestServer::start(TestOptions::new(
        empty_model_dir("origin"),
        Scripted::fast(),
    ));
    let token = server.token.clone();
    let host = server.host.clone();
    let body = b"not-an-image".as_slice();

    // 缺失 Origin。
    let response = server.request(
        "POST",
        "/api/ocr",
        &[
            ("Host", host.as_str()),
            ("X-RapidOCR-Token", token.as_str()),
            ("Content-Type", "application/octet-stream"),
        ],
        Some(body),
    );
    assert_eq!(response.status, 403, "{}", response.text());
    assert_eq!(response.code(), "bad_origin");

    // `Origin: null`（沙箱 iframe / file:// 会这么发）。
    let response = server.request(
        "POST",
        "/api/ocr",
        &[
            ("Host", host.as_str()),
            ("X-RapidOCR-Token", token.as_str()),
            ("Origin", "null"),
            ("Content-Type", "application/octet-stream"),
        ],
        Some(body),
    );
    assert_eq!(response.status, 403);

    // 另一个 origin。
    let response = server.request(
        "POST",
        "/api/ocr",
        &[
            ("Host", host.as_str()),
            ("X-RapidOCR-Token", token.as_str()),
            ("Origin", "http://127.0.0.1:1"),
            ("Content-Type", "application/octet-stream"),
        ],
        Some(body),
    );
    assert_eq!(response.status, 403);

    // 正确 origin 但队列/模型状态另有结论：403 不再是它的失败原因。
    let response = server.request(
        "POST",
        "/api/ocr",
        &[
            ("Host", host.as_str()),
            ("X-RapidOCR-Token", token.as_str()),
            ("Origin", "http://127.0.0.1:1/"),
            ("Content-Type", "application/octet-stream"),
        ],
        Some(body),
    );
    assert_eq!(response.status, 403, "a trailing slash is still a mismatch");
}

#[test]
fn read_requests_do_not_require_an_origin() {
    let server = TestServer::start(TestOptions::new(
        empty_model_dir("get-origin"),
        Scripted::fast(),
    ));
    let response = server.get("/api/status");
    assert_eq!(response.status, 200, "{}", response.text());
}

// ---------------------------------------------------------------- 准入顺序

#[test]
fn an_over_long_content_length_is_rejected_before_the_body_is_read() {
    let mut raw = limits(4, 2);
    raw.max_body_mb = 1;
    let server = TestServer::start(TestOptions {
        limits: raw,
        ..TestOptions::new(empty_model_dir("413"), Scripted::fast())
    });

    // 声明 4 MiB、**一个字节都不发**：服务端必须直接回 413。这正是"拒绝发生在读取
    // 请求体之前"的可观测证据（否则这个请求会一直等到读取超时）。
    let response = server.request(
        "POST",
        "/api/ocr",
        &[
            ("Host", server.host.as_str()),
            ("X-RapidOCR-Token", server.token.as_str()),
            ("Origin", server.origin.as_str()),
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", "4194304"),
        ],
        None,
    );
    assert_eq!(response.status, 413, "{}", response.text());
    assert_eq!(response.code(), "payload_too_large");
}

#[test]
fn unknown_paths_are_404_and_wrong_methods_are_405() {
    let server = TestServer::start(TestOptions::new(empty_model_dir("route"), Scripted::fast()));

    // 不存在的东西仍然是 404（`not_found` 只用于"没有这条路由"）。
    for path in [
        "/favicon.ico",
        "/api",
        "/api/jobs/job-0/annotated",
        "/api/nope",
    ] {
        let response = server.get(path);
        assert_eq!(response.status, 404, "{path}: {}", response.text());
        assert_eq!(response.code(), "not_found", "{path}");
    }

    // M3 的两个端点现在是**真实路由**：任务不存在 → 404 `job_not_found`（不是路由没匹配）。
    for path in [
        "/api/jobs/job-0/annotated.png",
        "/api/jobs/job-0/export?format=json",
    ] {
        let response = server.get(path);
        assert_eq!(response.status, 404, "{path}: {}", response.text());
        assert_eq!(response.code(), "job_not_found", "{path}");
    }

    let response = server.get("/api/ocr");
    assert_eq!(response.status, 405, "{}", response.text());
    assert_eq!(response.code(), "method_not_allowed");
    assert_eq!(response.header("Allow"), Some("POST"));

    // M2 新增的 `POST /api/engine/reload` 是真实路由：GET 它必须是 405 而不是 404。
    let response = server.get("/api/engine/reload");
    assert_eq!(response.status, 405, "{}", response.text());
    assert_eq!(response.code(), "method_not_allowed");
    assert_eq!(response.header("Allow"), Some("POST"));

    // M3 的两个端点只接受 GET。
    for path in ["/api/jobs/job-0/annotated.png", "/api/jobs/job-0/export"] {
        let response = server.post(path, &[], b"");
        assert_eq!(response.status, 405, "{path}: {}", response.text());
        assert_eq!(response.header("Allow"), Some("GET"), "{path}");
    }
}

#[test]
fn the_ocr_media_type_is_validated() {
    let server = TestServer::start(TestOptions::new(empty_model_dir("ctype"), Scripted::fast()));
    let response = server.post(
        "/api/ocr",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        b"x=1",
    );
    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(response.code(), "bad_request");
}

#[test]
fn the_max_side_query_parameter_is_validated() {
    let server = TestServer::start(TestOptions::new(
        empty_model_dir("maxside"),
        Scripted::fast(),
    ));
    for bad in ["0", "29", "abc", "99999999999999999999"] {
        let response = server.post(
            &format!("/api/ocr?max_side={bad}"),
            &[("Content-Type", "application/octet-stream")],
            b"image",
        );
        assert_eq!(response.status, 400, "max_side={bad}: {}", response.text());
    }
}

// ---------------------------------------------------------------- 协议闭环

#[test]
fn the_job_goes_from_202_to_succeeded_and_its_result_round_trips() {
    let server = TestServer::start(TestOptions::new(
        complete_model_dir("flow"),
        Scripted::fast(),
    ));
    // 模型齐备 → 引擎预加载 Ready（走的是真实的 ModelSource/ModelSet 路径）。
    let status = server.get("/api/status").json();
    assert_eq!(status["state"], "ready");
    assert_eq!(status["engine"]["state"], "ready");
    assert_eq!(status["engine"]["fallback_to_cpu"], false);
    assert_eq!(status["provider"]["requested"], "cpu");
    assert_eq!(status["provider"]["selected_ep"], "cpu");
    assert_eq!(status["provider"]["fallback_to_cpu"], false);

    let response = server.submit_ocr(b"pretend png bytes");
    assert_eq!(response.status, 202, "{}", response.text());
    let accepted = response.json();
    assert_eq!(accepted["state"], "queued");
    assert_eq!(accepted["queue"], "text");
    assert_eq!(accepted["kind"], "ocr");
    assert!(accepted["job_id"].as_str().is_some());
    assert!(accepted["position"].as_u64().is_some());

    let id = accepted["job_id"].as_str().expect("job id").to_string();
    let view = server.wait_terminal(&id, Duration::from_secs(20));
    assert_eq!(view["state"], "succeeded", "{view}");

    let result = server.get(&format!("/api/jobs/{id}/result"));
    assert_eq!(result.status, 200, "{}", result.text());
    assert_eq!(
        result.header("Content-Type"),
        Some("application/json; charset=utf-8")
    );
    let value = result.json();
    assert_eq!(value["regions"].as_array().map(Vec::len), Some(3));
    assert_eq!(value["regions"][0]["kind"], "text");
    assert_eq!(value["regions"][0]["recognition"]["text"], "region-0");
    assert!(value["regions"][0]["polygon"]["points"].is_array());
    assert!(
        value["text"]
            .as_str()
            .is_some_and(|text| text.contains("region-1")),
        "plain text must be present: {value}"
    );
    // 冻结页面的"复制全文"读 `plain_text`（docs/05 §9.2）；它与库的 `text` 同值。
    assert_eq!(value["plain_text"], value["text"]);
}

#[test]
fn a_failed_job_replays_its_original_status_and_code_on_result() {
    let server = TestServer::start(TestOptions::new(
        complete_model_dir("fail"),
        Scripted {
            fail: true,
            ..Scripted::fast()
        },
    ));
    let accepted = server.submit_ocr(b"broken").json();
    let id = accepted["job_id"].as_str().expect("job id").to_string();
    let view = server.wait_terminal(&id, Duration::from_secs(20));
    assert_eq!(view["state"], "failed");
    assert!(
        view["error"]
            .as_str()
            .is_some_and(|error| error.contains("cannot be decoded")),
        "{view}"
    );

    // `/result` 重放真实原因（422 unsupported_input），而不是把它压成 job_not_finished。
    let result = server.get(&format!("/api/jobs/{id}/result"));
    assert_eq!(result.status, 422, "{}", result.text());
    assert_eq!(result.code(), "unsupported_input");
}

#[test]
fn a_result_over_the_limit_is_a_413_result_too_large() {
    let mut raw = limits(4, 2);
    raw.max_result_mb = 1;
    let server = TestServer::start(TestOptions {
        limits: raw,
        ..TestOptions::new(
            complete_model_dir("toobig"),
            Scripted {
                regions: 1,
                text_bytes: 2 * 1024 * 1024,
                ..Scripted::fast()
            },
        )
    });
    let accepted = server.submit_ocr(b"huge result").json();
    let id = accepted["job_id"].as_str().expect("job id").to_string();
    let view = server.wait_terminal(&id, Duration::from_secs(30));
    assert_eq!(view["state"], "failed", "{view}");

    let result = server.get(&format!("/api/jobs/{id}/result"));
    assert_eq!(result.status, 413, "{}", result.text());
    assert_eq!(result.code(), "result_too_large");
}

#[test]
fn a_result_under_the_limit_is_served_intact() {
    let mut raw = limits(4, 2);
    raw.max_result_mb = 1;
    let server = TestServer::start(TestOptions {
        limits: raw,
        ..TestOptions::new(
            complete_model_dir("under"),
            Scripted {
                regions: 1,
                text_bytes: 64 * 1024,
                ..Scripted::fast()
            },
        )
    });
    let accepted = server.submit_ocr(b"small result").json();
    let id = accepted["job_id"].as_str().expect("job id").to_string();
    assert_eq!(
        server.wait_terminal(&id, Duration::from_secs(20))["state"],
        "succeeded"
    );
    let result = server.get(&format!("/api/jobs/{id}/result"));
    assert_eq!(result.status, 200);
    assert_eq!(
        result.json()["regions"][0]["recognition"]["text"],
        "x".repeat(64 * 1024)
    );
}

#[test]
fn cancelling_a_queued_job_succeeds_and_a_running_job_is_409() {
    let server = TestServer::start(TestOptions {
        limits: limits(2, 1),
        ..TestOptions::new(
            complete_model_dir("cancel"),
            Scripted::slow(Duration::from_millis(400)),
        )
    });

    // 第一个任务进入 Running。
    let first = server.submit_ocr(b"a").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    assert!(
        wait_until(Duration::from_secs(10), || {
            server.get(&format!("/api/jobs/{first}")).json()["state"] == "running"
        }),
        "the first job must reach running"
    );

    // 排队中的第二个任务可以被可靠取消（§4.3）。
    let second = server.submit_ocr(b"b").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    let cancelled = server.post(&format!("/api/jobs/{second}/cancel"), &[], b"");
    assert_eq!(cancelled.status, 200, "{}", cancelled.text());
    assert_eq!(cancelled.json()["state"], "cancelled");

    // 运行中的任务不可取消：409 not_cancellable，且状态**不变**。
    let running = server.post(&format!("/api/jobs/{first}/cancel"), &[], b"");
    assert_eq!(running.status, 409, "{}", running.text());
    assert_eq!(running.code(), "not_cancellable");
    assert_eq!(
        server.get(&format!("/api/jobs/{first}")).json()["state"],
        "running",
        "a refused cancel must not change the state"
    );

    // 终态再取消也是 409（不假装成功）。
    server.wait_terminal(&first, Duration::from_secs(20));
    let again = server.post(&format!("/api/jobs/{first}/cancel"), &[], b"");
    assert_eq!(again.status, 409);
}

#[test]
fn a_full_queue_is_503_without_reading_the_body() {
    let server = TestServer::start(TestOptions {
        limits: limits(1, 1),
        ..TestOptions::new(
            complete_model_dir("busy"),
            Scripted::slow(Duration::from_millis(400)),
        )
    });

    let first = server.submit_ocr(b"a").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    assert!(wait_until(Duration::from_secs(10), || {
        server.get(&format!("/api/jobs/{first}")).json()["state"] == "running"
    }));
    // 队列容量 1 → 第二个任务占满队列。
    let second = server.submit_ocr(b"b");
    assert_eq!(second.status, 202, "{}", second.text());
    // 第三个任务立刻 503（不等、不读 body）。
    let third = server.submit_ocr(b"c");
    assert_eq!(third.status, 503, "{}", third.text());
    assert_eq!(third.code(), "busy");

    server.wait_terminal(&first, Duration::from_secs(20));
}

/// 评审 P2-1：队列在容量上限时，**并发**到达的一批请求必须全部 503，
/// 且**没有一个**读入了请求体。
///
/// 证据与 413 的用例同一手法：声明 1 MiB 却一个字节都不发。任何一个请求只要越过了
/// 第 4 步，就会阻塞在读取 body 上（直到 30 s 的读取超时），而这些请求必须是**立刻**
/// 503——因此"全部 503"本身就证明它们都没有读 body。
#[test]
fn a_full_queue_rejects_every_concurrent_request_without_reading_a_body() {
    let server = TestServer::start(TestOptions {
        limits: limits(1, 1),
        ..TestOptions::new(
            complete_model_dir("reserve-full"),
            Scripted::slow(Duration::from_millis(600)),
        )
    });

    // 一个长任务占住 worker，队列里再放一个 → 队列满（容量 1）。
    let running = server.submit_ocr(b"a").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    assert!(wait_until(Duration::from_secs(10), || {
        server.get(&format!("/api/jobs/{running}")).json()["state"] == "running"
    }));
    let queued = server.submit_ocr(b"b");
    assert_eq!(queued.status, 202, "{}", queued.text());

    let addr = server.addr;
    let host = server.host.clone();
    let origin = server.origin.clone();
    let token = server.token.clone();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let mut threads = Vec::new();
    for _ in 0..8 {
        let (host, origin, token) = (host.clone(), origin.clone(), token.clone());
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            raw_request(
                addr,
                "POST",
                "/api/ocr",
                &[
                    ("Host", host.as_str()),
                    ("X-RapidOCR-Token", token.as_str()),
                    ("Origin", origin.as_str()),
                    ("Content-Type", "application/octet-stream"),
                    ("Content-Length", "1048576"),
                ],
                None,
            )
        }));
    }
    let mut refused = 0;
    for thread in threads {
        let response = thread.join().expect("the request thread must finish");
        assert_eq!(response.status, 503, "{}", response.text());
        assert_eq!(response.code(), "busy");
        refused += 1;
    }
    assert_eq!(refused, 8);

    // 被占住的那些请求**都没有**消耗容量：任务照常跑完。
    let view = server.wait_terminal(&running, Duration::from_secs(20));
    assert_eq!(view["state"], "succeeded", "{view}");
}

/// 评审 P2-1 的核心：**预留是原子的，而且不会泄漏容量**。
///
/// 为什么这条断言在 `reserve_queue_slot` 这一层而不是 HTTP 层：`accept_loop` 今天在
/// **单线程**里处理请求（准入 → 读 body → 入队都在 `handle` 的一次调用内），因此
/// HTTP 上那三个步骤本来就不可能交错。把"判定 + 占位"合成同一个临界区之后，
/// 这条保证不再依赖那个线程模型的事实——它由数据结构本身成立。
/// 并发因此在这里被真正地跑出来（8 个线程抢同一个容量 1 的队列）。
#[test]
fn the_queue_reservation_is_atomic_and_never_leaks_capacity() {
    let server = TestServer::start(TestOptions {
        limits: limits(1, 1),
        ..TestOptions::new(complete_model_dir("reserve-atomic"), Scripted::fast())
    });
    let shared = server.shared();

    let barrier = Arc::new(std::sync::Barrier::new(8));
    let mut threads = Vec::new();
    for _ in 0..8 {
        let shared = Arc::clone(&shared);
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            let reservation = shared.reserve_queue_slot(QueueClass::Text);
            let held = reservation.is_some();
            // 抢到的那个凭据被**故意保留**一会儿（不提交、也不提前释放），
            // 因此容量 1 的队列最多只能有一个赢家。
            std::thread::sleep(Duration::from_millis(50));
            drop(reservation);
            held
        }));
    }
    let mut winners = 0;
    for thread in threads {
        if thread.join().expect("the contender must finish") {
            winners += 1;
        }
    }
    assert_eq!(winners, 1, "exactly one contender may hold the only slot");

    // 全部释放之后，容量必须**完好无损**：下一个请求又能拿到那个槽位（没有泄漏，
    // 也没有反向的"多释放"把容量变大——后者用两次连续预留来证明）。
    let first = shared
        .reserve_queue_slot(QueueClass::Text)
        .expect("the released slot must be available again");
    assert!(
        shared.reserve_queue_slot(QueueClass::Text).is_none(),
        "capacity 1 must admit exactly one reservation"
    );
    drop(first);
    shared
        .reserve_queue_slot(QueueClass::Text)
        .expect("dropping a reservation must return the slot");
}

#[test]
fn evicted_jobs_are_410_and_after_the_ttl_they_become_404() {
    let mut raw = limits(2, 1);
    raw.max_retained = 1;
    raw.job_ttl_secs = 1;
    let server = TestServer::start(TestOptions {
        limits: raw,
        ..TestOptions::new(complete_model_dir("evict"), Scripted::fast())
    });

    let first = server.submit_ocr(b"a").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    server.wait_terminal(&first, Duration::from_secs(20));
    let second = server.submit_ocr(b"b").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    server.wait_terminal(&second, Duration::from_secs(20));

    // 保留上限 1 → 第一个任务被淘汰，但进了 tombstone → 410（不是 404）。
    let evicted = server.get(&format!("/api/jobs/{first}"));
    assert_eq!(evicted.status, 410, "{}", evicted.text());
    assert_eq!(evicted.code(), "job_evicted");
    assert_eq!(server.get(&format!("/api/jobs/{first}/result")).status, 410);

    // tombstone 自己也有 TTL：过期后同一个 id 变成 404（两条语义必须可区分）。
    assert!(
        wait_until(Duration::from_secs(15), || {
            server.get(&format!("/api/jobs/{first}")).status == 404
        }),
        "the tombstone must expire back to 404"
    );
    assert_eq!(server.get("/api/jobs/never-existed").status, 404);
}

// ---------------------------------------------------------------- 状态与模型

#[test]
fn status_reports_the_frozen_three_provider_fields_and_redacts_paths() {
    let dir = complete_model_dir("status");
    let server = TestServer::start(TestOptions::new(dir.clone(), Scripted::fast()));
    let response = server.get("/api/status");
    assert_eq!(response.status, 200);
    let value = response.json();

    for key in ["requested", "selected_ep", "fallback_to_cpu"] {
        assert!(
            value["provider"].get(key).is_some(),
            "provider.{key} must always exist: {value}"
        );
    }
    assert_eq!(value["model_dir"], "<redacted>");
    assert_eq!(value["source"], "local_manifest");
    assert_eq!(value["downloads_allowed"], false);
    assert!(value["ort"]["version"].is_string() || value["ort"]["version"].is_null());
    assert!(value["queues"]["text"]["capacity"].as_u64().unwrap_or(0) > 0);
    assert!(value["queues"]["formula"]["capacity"].as_u64().unwrap_or(0) > 0);
    assert!(value["limits"]["max_body_bytes"].as_u64().unwrap_or(0) > 0);
    assert!(
        value["memory"]["peak_working_set_bytes"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
    assert!(value["retention"]["job_ttl_ms"].as_u64().unwrap_or(0) > 0);

    // 本机绝对路径绝不出现在任何状态响应里（§7.4/§10.9）。
    let text = response.text();
    assert!(!text.contains(&dir.display().to_string()), "{text}");
    assert!(!text.contains("\\\\?\\"), "{text}");
    assert!(
        !server
            .get("/api/models")
            .text()
            .contains(&dir.display().to_string())
    );
}

#[test]
fn models_reports_every_file_state_and_matches_the_ocr_409() {
    // 空目录：默认表来源，文本三个文件全缺 + 公式模型缺失（M4）。
    let server = TestServer::start(TestOptions::new(
        empty_model_dir("models"),
        Scripted::fast(),
    ));
    let models = server.get("/api/models");
    assert_eq!(models.status, 200);
    let value = models.json();
    assert_eq!(value["source"], "default_table");
    assert_eq!(value["downloads_allowed"], false);
    let sets = value["sets"].as_array().expect("sets");
    assert_eq!(sets.len(), 2, "text set + formula set (M4): {value}");
    let files = sets[0]["files"].as_array().expect("files");
    assert_eq!(files.len(), 3);
    for file in files {
        assert_eq!(file["state"], "missing");
        assert!(file["name"].as_str().is_some());
        assert!(file["role"].as_str().is_some());
        assert!(file["size_bytes"].as_u64().is_some());
        assert!(file["sha256"].as_str().is_some());
        assert!(file["source_url"].as_str().is_some());
    }
    // 公式集合：**一个**文件、有体积与哈希（页面必须能在下载前显示 566 MB）。
    let formula_set = &sets[1];
    assert_eq!(formula_set["id"], "PP-FormulaNet_plus-M");
    let formula_files = formula_set["files"].as_array().expect("formula files");
    assert_eq!(formula_files.len(), 1);
    assert_eq!(formula_files[0]["role"], "formula_recognizer");
    assert_eq!(formula_files[0]["name"], "pp_formulanet_plus_m.onnx");
    assert_eq!(formula_files[0]["state"], "missing");
    assert_eq!(formula_files[0]["size_bytes"], 593_915_961u64);
    assert_eq!(
        formula_files[0]["sha256"],
        "71b6d389cf7b857e45252a4b98cfced1a3ffca7bf24d9497d02d052a41d9493b"
    );

    // 顶层四个字段是**文本管线**作用域（引擎要加载的那些文件）；
    // 公式管线在 `formula` 块里单独报告（M4 的根因修复：公式缺失不得让普通 OCR 变 409）。
    let missing: Vec<&str> = value["missing"]
        .as_array()
        .expect("missing")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(missing.len(), 3);
    assert_eq!(value["complete"], false);
    assert_eq!(value["blocked"], value["missing"]);
    assert_eq!(
        value["formula"]["missing"],
        serde_json::json!(["pp_formulanet_plus_m.onnx"])
    );
    assert_eq!(value["formula"]["complete"], false);
    assert_eq!(
        value["formula"]["required_roles"],
        serde_json::json!(["formula_recognizer"])
    );
    // 这个测试实例没有配置检测模型：路由关闭，且理由是可读文字（不是只有禁用态）。
    assert_eq!(value["formula"]["routing"], false);
    assert!(
        value["formula"]["disabled_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("--formula-detector")),
        "{value}"
    );

    // 引擎在空模型目录下是 BlockedModelsMissing，而**服务**是 Ready（§7.6）。
    let status = server.get("/api/status").json();
    assert_eq!(status["state"], "ready");
    assert_eq!(status["engine"]["state"], "blocked_models_missing");
    // 引擎的缺失清单同样只含文本文件（公式模型缺失不是"引擎起不来"）。
    let engine_missing = status["engine"]["missing"]
        .as_array()
        .expect("engine missing");
    assert_eq!(engine_missing.len(), 3, "{status}");
    assert_eq!(status["formula"]["routing"], false);
    assert!(status["limits"]["max_eval_cases"].as_u64().unwrap_or(0) > 0);

    // OCR 409：字段与 /api/models **逐字节一致**（文本作用域）。
    let rejected = server.submit_ocr(b"image bytes");
    assert_eq!(rejected.status, 409, "{}", rejected.text());
    assert_eq!(rejected.code(), "models_missing");
    let detail = &rejected.json()["detail"];
    assert_eq!(detail["missing"], value["missing"], "{detail}");
    assert_eq!(detail["corrupt"], value["corrupt"]);
    assert_eq!(detail["source"], value["source"]);
    assert_eq!(detail["model_dir"], value["model_dir"]);

    // 公式队列：路由未启用 → 400（不静默按文本处理，M1 的语义在 M4 不变）。
    let formula = server.post(
        "/api/ocr?queue=formula",
        &[("Content-Type", "application/octet-stream")],
        b"image bytes",
    );
    assert_eq!(formula.status, 400, "{}", formula.text());
    assert_eq!(formula.code(), "bad_request");
}

#[test]
fn an_engine_that_fails_to_load_is_failed_with_a_reason_and_503() {
    let dir = complete_model_dir("enginefail");
    let server = TestServer::start(TestOptions {
        engine_factory: Arc::new(|_config: &EngineConfig| {
            Err(RapidOcrError::UnsupportedProvider(
                "scripted: the requested provider is unavailable in this build".to_string(),
            ))
        }),
        ..TestOptions::new(dir, Scripted::fast())
    });

    let status = server.get("/api/status").json();
    assert_eq!(
        status["state"], "ready",
        "the service is ready even if the engine is not"
    );
    assert_eq!(status["engine"]["state"], "failed");
    assert!(
        status["engine"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("provider is unavailable")),
        "{status}"
    );
    // 引擎不可用 → 503 engine_unavailable 且带 reason。
    let rejected = server.submit_ocr(b"image");
    assert_eq!(rejected.status, 503, "{}", rejected.text());
    assert_eq!(rejected.code(), "engine_unavailable");
    assert!(rejected.json()["detail"]["reason"].is_string());
}

// ---------------------------------------------------------------- 下载接缝（M2）

/// `POST /api/models/download` 的三种"必须拒绝"与"已经齐备时如实成功"。
///
/// **M2 的行为变化**（这里是有意的期望变更，不是弱化）：
/// - M1 的 worker 会把任务判为失败并写明"未实现、无网络 I/O"；M2 替换了处理体，
///   因此同一个请求现在要么真的下载，要么（集合已经齐备时）**成功且 0 个文件要下**；
/// - 未知 `set_id` 从 M1 的 400 `bad_request` 变成 **404 `model_set_not_found`**：
///   §4.2 的请求体只有 `set_id`，"未知集合"必须有可定位的答复（带上请求的 id 与已知集合），
///   而不是与"请求体畸形"共用 400。
#[test]
fn the_download_endpoint_refuses_disabled_unknown_and_url_bearing_requests() {
    let dir = complete_model_dir("download");
    let disabled = TestServer::start(TestOptions::new(dir.clone(), Scripted::fast()));
    let response = disabled.post(
        "/api/models/download",
        &[("Content-Type", "application/json")],
        br#"{"set_id":"test-set"}"#,
    );
    assert_eq!(response.status, 403, "{}", response.text());
    assert_eq!(response.code(), "downloads_disabled");

    // 打开 --allow-download：这是一个**真实**任务。集合已经齐备 → 0 个文件要下 → 成功。
    let enabled = TestServer::start(TestOptions {
        allow_download: true,
        ..TestOptions::new(dir, Scripted::fast())
    });
    let response = enabled.post(
        "/api/models/download",
        &[("Content-Type", "application/json")],
        br#"{"set_id":"test-set"}"#,
    );
    assert_eq!(response.status, 202, "{}", response.text());
    let accepted = response.json();
    assert_eq!(accepted["kind"], "model_download");
    assert_eq!(accepted["queue"], "download");
    assert_eq!(accepted["download"]["files_total"], 0);
    let id = accepted["job_id"].as_str().expect("job id").to_string();
    let view = enabled.wait_terminal(&id, Duration::from_secs(20));
    assert_eq!(view["state"], "succeeded", "{view}");
    assert_eq!(view["kind"], "model_download");
    assert_eq!(view["failure"], serde_json::Value::Null);

    // 未知 set_id 与带 URL 的请求体都必须被拒绝，而不是被忽略。
    let unknown = enabled.post(
        "/api/models/download",
        &[("Content-Type", "application/json")],
        br#"{"set_id":"nope"}"#,
    );
    assert_eq!(unknown.status, 404, "{}", unknown.text());
    assert_eq!(unknown.code(), "model_set_not_found");
    let with_url = enabled.post(
        "/api/models/download",
        &[("Content-Type", "application/json")],
        br#"{"set_id":"test-set","url":"https://evil.example/x.onnx"}"#,
    );
    assert_eq!(with_url.status, 400, "{}", with_url.text());
}

// ---------------------------------------------------------------- 双向公平性

/// 公式洪水下普通任务不被饿死（§8.3 方向一）。
#[test]
fn a_formula_flood_does_not_starve_text_jobs() {
    let delay = Duration::from_millis(25);
    let dir = complete_model_dir("fair-formula");
    let scripted = Scripted::slow(delay);
    let server = TestServer::start(
        TestOptions::new(dir.clone(), scripted.clone())
            .with_formula_routing(&dir.join("pix2text-mfd-1.5.onnx"))
            .with_limits(limits(4, 2)),
    );
    let shared = server.shared();
    // 上界来自 `/api/status` 的公开字段（调度参数的可观测口径），而不是内部访问器。
    let status = server.get("/api/status").json();
    let bound = status["queues"]["text"]["wait_bound"]
        .as_u64()
        .expect("wait_bound") as usize;
    assert!(bound > 0, "{status}");
    assert_eq!(shared.service_state(), super::state::ServiceState::Ready);

    let starved = run_flood(&server, "formula", QueueClass::Text, bound, delay);
    assert!(
        starved.is_none(),
        "the text job was starved under a formula flood: {starved:?}"
    );
}

/// 普通洪水下公式任务不被饿死（§8.3 方向二）。
#[test]
fn a_text_flood_does_not_starve_formula_jobs() {
    let delay = Duration::from_millis(25);
    let dir = complete_model_dir("fair-text");
    let scripted = Scripted::slow(delay);
    let server = TestServer::start(
        TestOptions::new(dir.clone(), scripted.clone())
            .with_formula_routing(&dir.join("pix2text-mfd-1.5.onnx"))
            .with_limits(limits(4, 2)),
    );
    let shared = server.shared();
    assert_eq!(shared.engine_state_name(), "ready");
    let status = server.get("/api/status").json();
    let bound = status["queues"]["formula"]["wait_bound"]
        .as_u64()
        .expect("wait_bound") as usize;
    assert!(bound > 0, "{status}");

    let starved = run_flood(&server, "text", QueueClass::Formula, bound, delay);
    assert!(
        starved.is_none(),
        "the formula job was starved under a text flood: {starved:?}"
    );
}

/// 让 `flood` 队列保持饱和，同时提交一个 `victim` 队列的任务并计时。
///
/// 返回 `Some(elapsed_ms)` 表示超过上界（被饿死），`None` 表示在界内完成。
fn run_flood(
    server: &TestServer,
    flood: &str,
    victim: QueueClass,
    bound_jobs: usize,
    delay: Duration,
) -> Option<u128> {
    let flood = flood.to_string();
    let url = format!("/api/ocr?queue={flood}");
    let stop = Arc::new(AtomicBool::new(false));
    let flooded = Arc::new(AtomicUsize::new(0));

    // 先让 worker 忙起来（否则第一次提交会立刻被服务，测不出排队时长）。
    let warmup = server.submit_ocr(b"warmup");
    assert!(warmup.status == 202, "{}", warmup.text());
    let warmup_id = warmup.json()["job_id"].as_str().expect("id").to_string();
    server.wait_terminal(&warmup_id, Duration::from_secs(20));

    let flooder = {
        let stop = Arc::clone(&stop);
        let flooded = Arc::clone(&flooded);
        let addr = server.addr;
        let host = server.host.clone();
        let origin = server.origin.clone();
        let token = server.token.clone();
        let url = url.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let raw = format!(
                    "POST {url} HTTP/1.1\r\nConnection: close\r\nHost: {host}\r\nOrigin: {origin}\r\n\
                     X-RapidOCR-Token: {token}\r\nContent-Type: application/octet-stream\r\n\
                     Content-Length: 5\r\n\r\nimage"
                );
                let response = send(addr, raw.as_bytes());
                if response.status == 202 {
                    flooded.fetch_add(1, Ordering::SeqCst);
                } else {
                    // 队列满（503）：让出一点时间，避免把 accept 线程打满。
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        })
    };

    // 等洪水真的把**目标队列灌满**（`used == capacity`）：只有这样，"受害者仍在界内被服务"
    // 才能证明双向配额在生效，而不是因为另一队列恰好是空的。
    let saturated = wait_until(Duration::from_secs(10), || {
        let status = server.get("/api/status").json();
        let queues = &status["queues"][flood.as_str()];
        let used = queues["used"].as_u64().unwrap_or(0);
        let capacity = queues["capacity"].as_u64().unwrap_or(0);
        used >= capacity
    });
    assert!(
        saturated,
        "the flood never saturated the {flood} queue (accepted {} job(s))",
        flooded.load(Ordering::SeqCst)
    );

    let victim_path = match victim {
        QueueClass::Text => "/api/ocr?queue=text",
        QueueClass::Formula => "/api/ocr?queue=formula",
    };
    let started = Instant::now();
    let mut accepted = None;
    for _ in 0..200 {
        let response = server.post(
            victim_path,
            &[("Content-Type", "application/octet-stream")],
            b"victim",
        );
        if response.status == 202 {
            accepted = Some(response.json());
            break;
        }
        assert_eq!(response.status, 503, "{}", response.text());
        std::thread::sleep(Duration::from_millis(2));
    }
    let accepted = accepted.expect("the victim job must eventually be accepted");
    let id = accepted["job_id"].as_str().expect("id").to_string();
    let view = server.wait_terminal(&id, Duration::from_secs(30));
    let elapsed = started.elapsed();
    // 计时窗口里洪水一直在被服务（否则"没被饿死"就没有意义）。
    let flooded_during_wait = flooded.load(Ordering::SeqCst);
    stop.store(true, Ordering::SeqCst);
    let _ = flooder.join();

    assert_eq!(view["state"], "succeeded", "{view}");
    assert!(
        flooded_during_wait > 0,
        "the {flood} queue was never served while the victim waited"
    );
    // 可证明上界：`capacity × 对方配额` 个"对方"任务 + 1 个正在运行的 + 余量。
    let budget = delay * u32::try_from(bound_jobs + 3).expect("small bound");
    if elapsed > budget + Duration::from_millis(750) {
        return Some(elapsed.as_millis());
    }
    None
}

// ---------------------------------------------------------------- 路由守卫

#[test]
fn the_formula_queue_is_refused_when_formula_routing_is_off() {
    let server = TestServer::start(TestOptions::new(
        complete_model_dir("noformula"),
        Scripted::fast(),
    ));
    let response = server.post(
        "/api/ocr?queue=formula",
        &[("Content-Type", "application/octet-stream")],
        b"image",
    );
    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(response.code(), "bad_request");
    // 普通任务照常可提交（公式关闭不影响文本）。
    assert_eq!(server.submit_ocr(b"image").status, 202);

    // M4：路由关闭的**理由**必须能通过 `/api/models`（页面据此禁用开关）与 `/api/status`
    // 读到，而不是只表现为一个禁用控件（§9.4：不只靠颜色）。
    let models = server.get("/api/models").json();
    assert_eq!(models["formula"]["routing"], false);
    let reason = models["formula"]["disabled_reason"]
        .as_str()
        .expect("a textual reason");
    assert!(reason.contains("--formula-detector"), "{reason}");
    assert_eq!(models["formula"]["detector"]["configured"], false);
    assert_eq!(models["formula"]["detector"]["file"], Value::Null);
    let status = server.get("/api/status").json();
    assert_eq!(status["formula"]["routing"], false);
    assert_eq!(
        status["formula"]["disabled_reason"],
        models["formula"]["disabled_reason"]
    );
}

// ---------------------------------------------------------------- M4：公式队列

/// 公式队列是**真实**的第二条管线：`queue=formula` 进公式队列，并在 worker 里拿到
/// 由模型集解析出的 `FormulaPolicy`（识别模型 + 集合声明的 SHA-256 + 检测模型）。
///
/// 脚本化后端不加载模型，但它在 `OcrRequest.formula` 上看到的正是生产路径组装的那一份，
/// 因此"路由真的接上了公式管线"不是靠猜测：三个字段逐个断言。
#[test]
fn the_formula_queue_runs_the_formula_pipeline_from_the_model_set() {
    let dir = complete_model_dir("m4-formula");
    let detector = dir.join("pix2text-mfd-1.5.onnx");
    let scripted = Scripted::fast();
    let server = TestServer::start(
        TestOptions::new(dir.clone(), scripted.clone()).with_formula_routing(&detector),
    );

    // 模型集报告公式集合齐备 + 路由可用（页面据此允许勾选）。
    let models = server.get("/api/models").json();
    assert_eq!(models["formula"]["complete"], true, "{models}");
    assert_eq!(models["formula"]["routing"], true);
    assert_eq!(models["formula"]["disabled_reason"], Value::Null);
    assert_eq!(models["formula"]["detector"]["configured"], true);
    assert_eq!(
        models["formula"]["detector"]["file"], "pix2text-mfd-1.5.onnx",
        "only the file name is exposed (§7.4)"
    );
    assert!(
        !models.to_string().contains(&dir.display().to_string()),
        "no absolute path may leak: {models}"
    );

    let text = server.submit_ocr(b"text image");
    assert_eq!(text.status, 202, "{}", text.text());
    assert_eq!(text.json()["queue"], "text");
    let formula = server.post(
        "/api/ocr?queue=formula",
        &[("Content-Type", "application/octet-stream")],
        b"formula image",
    );
    assert_eq!(formula.status, 202, "{}", formula.text());
    let accepted = formula.json();
    assert_eq!(accepted["queue"], "formula", "{accepted}");
    assert_eq!(accepted["kind"], "ocr");

    let text_id = text.json()["job_id"].as_str().expect("id").to_string();
    let formula_id = accepted["job_id"].as_str().expect("id").to_string();
    let text_view = server.wait_terminal(&text_id, Duration::from_secs(20));
    let formula_view = server.wait_terminal(&formula_id, Duration::from_secs(20));
    assert_eq!(text_view["state"], "succeeded", "{text_view}");
    assert_eq!(formula_view["state"], "succeeded", "{formula_view}");
    assert_eq!(formula_view["queue"], "formula");

    let policies = scripted.formulas();
    assert_eq!(policies.len(), 2, "one request per job");
    // 顺序：文本任务先入队（两个队列都非空时调度器先取文本）。
    assert!(
        !policies[0].enabled,
        "the text queue must not enable the formula pipeline: {:?}",
        policies[0]
    );
    assert_eq!(policies[0].model_path, None);
    let formula = &policies[1];
    assert!(formula.enabled, "the formula queue must enable it");
    assert_eq!(
        formula.model_path.as_deref(),
        Some(dir.join("fx.onnx").as_path())
    );
    assert_eq!(
        formula.detector_path.as_deref(),
        Some(detector.as_path()),
        "the detector comes from the startup-resolved path"
    );
    // 识别模型的 SHA-256 来自**模型集**（权威判定在下一次真实加载时由库执行）。
    let declared = models["sets"][0]["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|file| file["role"] == "formula_recognizer")
        .and_then(|file| file["sha256"].as_str())
        .expect("the formula model's declared hash");
    assert_eq!(formula.expected_model_sha256.as_deref(), Some(declared));
    // §10.8：识别模型是被**显式**下载的集合成员，公式区域仍按请求开关逐次选择。
    assert!(
        !formula.include_token_ids,
        "token ids stay out of the default"
    );
}

/// 公式集合不齐备：`queue=formula` 是 **409**（可定位、指向要下载的文件），
/// 而普通 OCR 完全不受影响（M4 的核心不变量，走真实 HTTP）。
#[test]
fn an_incomplete_formula_set_is_409_while_text_ocr_keeps_working() {
    let dir = manifest_model_dir("m4-formula-incomplete", &text_fixture_files(), None);
    let detector = dir.join("pix2text-mfd-1.5.onnx");
    let server = TestServer::start(
        TestOptions::new(dir.clone(), Scripted::fast()).with_formula_routing(&detector),
    );

    // `/api/models`：文本齐备（顶层 complete=true），公式不齐备（formula.complete=false）。
    let models = server.get("/api/models").json();
    assert_eq!(models["complete"], true, "{models}");
    assert_eq!(models["missing"], serde_json::json!([]));
    assert_eq!(models["formula"]["complete"], false);
    assert_eq!(models["formula"]["missing"], serde_json::json!(["fx.onnx"]));
    assert_eq!(models["formula"]["routing"], true);

    // 公式请求在**读 body 之前**就被拒绝（§4.4 第 4 步之后），且 detail 是公式作用域。
    let formula = server.post(
        "/api/ocr?queue=formula",
        &[("Content-Type", "application/octet-stream")],
        b"formula image",
    );
    assert_eq!(formula.status, 409, "{}", formula.text());
    assert_eq!(formula.code(), "models_missing");
    let detail = &formula.json()["detail"];
    assert_eq!(detail["scope"], "formula");
    assert_eq!(detail["missing"], serde_json::json!(["fx.onnx"]));
    // 评审 P1-2：请求路径**不再**有"只看存在性"的第二套清单——判定与 `/api/models`
    // 同源（哈希状态）。检测模型在场且没有声明哈希时它如实报告 `present`/`sha256: null`。
    assert_eq!(detail["detector"]["state"], "present");
    assert_eq!(detail["detector"]["sha256"], Value::Null);
    assert!(
        detail.get("missing_on_disk").is_none(),
        "the existence-only field is gone: {detail}"
    );

    // 普通 OCR 照常可用（脚本化后端），并且它拿到的策略**没有**启用公式。
    let text = server.submit_ocr(b"text image");
    assert_eq!(text.status, 202, "{}", text.text());
    let id = text.json()["job_id"].as_str().expect("id").to_string();
    let view = server.wait_terminal(&id, Duration::from_secs(20));
    assert_eq!(view["state"], "succeeded", "{view}");
    assert_eq!(view["queue"], "text");
}

/// 公式角色在**运行期**被拿掉（文件删除）时：`/api/models` 与 409 都如实反映，
/// 而不是缓存启动期快照（与 M2"下载完成后立刻变 present"同一条规则）。
#[test]
fn removing_the_formula_model_turns_the_route_into_a_409() {
    let dir = complete_model_dir("m4-formula-removed");
    let detector = dir.join("pix2text-mfd-1.5.onnx");
    let server = TestServer::start(
        TestOptions::new(dir.clone(), Scripted::fast()).with_formula_routing(&detector),
    );
    assert_eq!(
        server.get("/api/models").json()["formula"]["complete"],
        true
    );
    std::fs::remove_file(dir.join("fx.onnx")).expect("remove the formula model");

    let models = server.get("/api/models").json();
    assert_eq!(models["formula"]["complete"], false);
    assert_eq!(models["formula"]["missing"], serde_json::json!(["fx.onnx"]));
    // 文本集合仍然齐备：顶层字段不受公式影响。
    assert_eq!(models["complete"], true, "{models}");
    let formula = server.post(
        "/api/ocr?queue=formula",
        &[("Content-Type", "application/octet-stream")],
        b"formula image",
    );
    assert_eq!(formula.status, 409, "{}", formula.text());
    assert_eq!(formula.json()["detail"]["scope"], "formula");
    assert_eq!(server.submit_ocr(b"text").status, 202);
}

// ------------------------------------------------- M1 评审：检测模型完整性 / 校验缓存

/// 一个带 `formula_detector` role 的本地清单目录（评审 P1-1 的夹具）。
///
/// 五个文件：文本三个 + 公式识别 + 公式检测。检测模型的**声明哈希**就是写盘时那一份
/// 内容的哈希，因此测试可以随后把它改写来构造"存在但内容不对"。
struct DetectorFixture {
    dir: PathBuf,
    detector: PathBuf,
    detector_sha: String,
}

fn detector_model_dir(name: &str) -> DetectorFixture {
    let dir = m2_root().join(format!("detector-{name}-{}", unique()));
    std::fs::create_dir_all(&dir).expect("create the model dir");
    let files = [
        ("det.onnx", "detector"),
        ("rec.onnx", "recognizer"),
        ("dict.txt", "dictionary"),
        ("fx.onnx", "formula_recognizer"),
        ("mfd.onnx", "formula_detector"),
    ];
    let mut manifest = String::from(
        "{\"schema_version\":1,\"id\":\"test-set\",\"family\":\"PP-OCR\",\"version\":\"v-test\",\
         \"languages\":[\"en\"],\"files\":[",
    );
    let mut detector_sha = String::new();
    for (index, (file, role)) in files.iter().enumerate() {
        let bytes = model_file_bytes(file);
        let path = dir.join(file);
        std::fs::write(&path, &bytes).expect("write the model file");
        let sha = sha256_file(&path).expect("hash the model file");
        if *file == "mfd.onnx" {
            detector_sha = sha.clone();
        }
        if index > 0 {
            manifest.push(',');
        }
        manifest.push_str(&format!(
            "{{\"name\":\"{file}\",\"role\":\"{role}\",\"sha256\":\"{sha}\",\"size_bytes\":{},\
             \"source_url\":\"https://www.modelscope.cn/models/{file}\"}}",
            bytes.len()
        ));
    }
    manifest.push_str("]}");
    std::fs::write(dir.join("manifest.json"), manifest).expect("write the manifest");
    DetectorFixture {
        detector: dir.join("mfd.onnx"),
        detector_sha,
        dir,
    }
}

/// 评审 P1-1：损坏的公式**检测**模型必须被抓住——`/api/models` 如实报 `corrupt`，
/// 并且 `queue=formula` 在**读 body 之前**就是 409 `models_corrupt`。
///
/// 旧实现的缺口（本用例的根因）：检测模型的路径被单独传递、集合声明的 SHA-256 被丢掉，
/// 而请求路径只做存在性预检——于是"文件在、内容错"能一路走到 worker 里才失败。
#[test]
fn a_corrupt_formula_detector_is_reported_and_refused_before_the_body() {
    let fixture = detector_model_dir("corrupt-detector");
    std::fs::write(&fixture.detector, b"corrupted detector bytes").expect("corrupt the detector");
    let server = TestServer::start(
        TestOptions::new(fixture.dir.clone(), Scripted::fast())
            .with_verified_formula_detector(&fixture.detector, &fixture.detector_sha),
    );

    // `/api/models`：集合里的检测模型是损坏的，且**只给文件名**（§7.4 脱敏）。
    let models = server.get("/api/models").json();
    assert_eq!(models["formula"]["complete"], false, "{models}");
    assert_eq!(
        models["formula"]["corrupt"],
        json!(["mfd.onnx"]),
        "{models}"
    );
    assert_eq!(models["formula"]["detector"]["configured"], true);
    assert_eq!(models["formula"]["detector"]["file"], "mfd.onnx");
    assert_eq!(
        models["formula"]["detector"]["state"], "corrupt",
        "{models}"
    );
    assert_eq!(
        models["formula"]["detector"]["sha256"], fixture.detector_sha,
        "the declared hash must be reported (not dropped)"
    );
    assert!(
        !models
            .to_string()
            .contains(&fixture.dir.display().to_string()),
        "no absolute path may leak: {models}"
    );

    // 请求体**没有被读**：声明 1 MiB 却一个字节都不发。若服务端在这里读 body，
    // 这个请求会一直等到读取超时（测试因此会超时失败），所以 409 本身就是证据。
    let refused = server.request(
        "POST",
        "/api/ocr?queue=formula",
        &[
            ("Host", server.host.as_str()),
            ("X-RapidOCR-Token", server.token.as_str()),
            ("Origin", server.origin.as_str()),
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", "1048576"),
        ],
        None,
    );
    assert_eq!(refused.status, 409, "{}", refused.text());
    assert_eq!(refused.code(), "models_corrupt");
    let detail = &refused.json()["detail"];
    assert_eq!(detail["scope"], "formula");
    assert_eq!(detail["corrupt"], json!(["mfd.onnx"]));
    assert_eq!(detail["detector"]["state"], "corrupt");

    // 普通 OCR 完全不受影响（那是公式队列的缺口，不是引擎的）。
    let text = server.submit_ocr(b"text image");
    assert_eq!(text.status, 202, "{}", text.text());
}

/// 评审 P1-1：检测模型来自 `--formula-detector`（**不在任何集合里**）时同样要被校验，
/// 而且它是唯一阻塞项时 `detail.corrupt` 仍要点名它——否则 `code=models_corrupt` 会
/// 列不出任何一个文件名。
#[test]
fn a_cli_formula_detector_is_verified_by_the_same_rule() {
    let dir = manifest_model_dir_text_only("cli-detector", &text_fixture_files(), None);
    let detector = dir.join("pix2text-mfd-1.5.onnx");
    std::fs::write(&detector, b"a detector the manifest never declared").expect("write detector");
    let declared = sha256_file(&detector).expect("hash the detector");
    let server = TestServer::start(
        TestOptions::new(dir.clone(), Scripted::fast())
            .with_verified_formula_detector(&detector, &declared),
    );

    // 内容正确 → 公式集合齐备、检测模型 `present`、公式队列照常 202。
    let models = server.get("/api/models").json();
    assert_eq!(models["formula"]["complete"], true, "{models}");
    assert_eq!(models["formula"]["detector"]["state"], "present");
    assert_eq!(models["formula"]["detector"]["sha256"], declared);
    let accepted = server.post(
        "/api/ocr?queue=formula",
        &[("Content-Type", "application/octet-stream")],
        b"formula image",
    );
    assert_eq!(accepted.status, 202, "{}", accepted.text());

    // 同一个路径、内容被替换 → 身份变化 → 必须重新校验并拒绝（不能继续用旧结论）。
    std::fs::write(&detector, b"corrupted after the first verification").expect("corrupt");
    let models = server.get("/api/models").json();
    assert_eq!(
        models["formula"]["detector"]["state"], "corrupt",
        "{models}"
    );
    let refused = server.request(
        "POST",
        "/api/ocr?queue=formula",
        &[
            ("Host", server.host.as_str()),
            ("X-RapidOCR-Token", server.token.as_str()),
            ("Origin", server.origin.as_str()),
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", "1048576"),
        ],
        None,
    );
    assert_eq!(refused.status, 409, "{}", refused.text());
    assert_eq!(refused.code(), "models_corrupt");
    assert_eq!(
        refused.json()["detail"]["corrupt"],
        json!(["pix2text-mfd-1.5.onnx"]),
        "the detector is the only blocker and must be named"
    );
}

/// 评审 P1-2 + 性能一节：`/api/models` 复用**身份键控**的校验结论，而不是每次重新哈希。
///
/// `cold_this_call` 是**单次报告**里真的重算了摘要的文件数：启动快照已经验证过一次，
/// 因此稳态下必须是 0；一个文件被替换（体积/mtime 变化）时，只有它必须重新验证。
#[test]
fn models_reuses_the_verified_digest_until_the_file_identity_changes() {
    let fixture = detector_model_dir("verify-cache");
    let server = TestServer::start(TestOptions::new(fixture.dir.clone(), Scripted::fast()));

    let first = server.get("/api/models").json();
    assert_eq!(
        first["verification"]["cold_this_call"], 0,
        "the startup snapshot already verified every declared file: {first}"
    );
    assert_eq!(
        first["verification"]["identity"],
        "path + size + mtime + SHA-256 of the first and last 64 KiB",
        "the identity must name all four components (B: the windows narrow the blind spot)"
    );
    assert_eq!(
        first["verification"]["partial_window_bytes"], 65536,
        "{first}"
    );
    assert!(
        first["verification"]["partial_reads"].as_u64().unwrap_or(0) >= 5,
        "every check reads the 128 KiB window: {first}"
    );
    assert!(
        first["verification"]["residual_blind_spot"]
            .as_str()
            .unwrap_or_default()
            .contains("not a security boundary"),
        "the honest statement must say what the heuristic is not: {first}"
    );
    assert!(first["verification"]["last_cold_ms"].is_number(), "{first}");
    assert!(
        first["verification"]["cold_verifications"]
            .as_u64()
            .unwrap_or(0)
            >= 5,
        "the five declared files were verified at least once: {first}"
    );
    let hits = first["verification"]["cache_hits"].as_u64().unwrap_or(0);

    let second = server.get("/api/models").json();
    assert_eq!(
        second["verification"]["cold_this_call"], 0,
        "a cache hit must not re-hash: {second}"
    );
    assert!(
        second["verification"]["cache_hits"].as_u64().unwrap_or(0) >= hits + 5,
        "five files were answered from the cache: {second}"
    );

    // 替换公式识别模型：身份变化 → **只有它**重新验证，并且结论变为 corrupt。
    std::fs::write(fixture.dir.join("fx.onnx"), b"replaced formula model").expect("replace");
    let third = server.get("/api/models").json();
    assert_eq!(
        third["verification"]["cold_this_call"], 1,
        "only the replaced file may be re-hashed: {third}"
    );
    assert_eq!(third["formula"]["complete"], false, "{third}");
    assert_eq!(third["formula"]["corrupt"], json!(["fx.onnx"]));
}

// ------------------------------------------- A1：`--reverify-models`（启动期冷验证，fail-fast）

/// A2 的页面侧：`重新校验` 按钮必须真的在页面里，并按页面既有的写法接线。
///
/// 页面是**冻结契约**的一部分（§9：三个 `nonce=` 属性逐字节相同、占位符与令牌计数固定），
/// 因此这里同时断言"新增按钮没有破坏那三条不变量"：
///
/// - 没有内联 `style=` 或 `onclick=`（CSP 是 nonce，内联处理器会被拦掉；
///   页面既有的做法是 `addEventListener` + CSS 类）；
/// - 按钮与结论区都在，且用 `role="status"`（`aria-live`）通报结果；
/// - 离线预览（`DEMO`）分支不发请求，如实解释"没有服务端"。
#[test]
fn the_page_carries_the_reverify_button_without_inline_handlers_or_styles() {
    let server = TestServer::start(TestOptions::new(
        empty_model_dir("page-reverify"),
        Scripted::fast(),
    ));
    let page = server.get("/").text();

    assert!(
        page.contains("id=\"verifyBtn\"") && page.contains("重新校验"),
        "the page must carry the reverify button"
    );
    assert!(
        page.contains("/api/models/reverify"),
        "the button must go through the documented endpoint"
    );
    assert!(
        page.contains("$('verifyBtn').addEventListener('click', reverifyModels)"),
        "the handler must be attached with addEventListener (no inline onclick under the nonce CSP)"
    );
    assert!(
        page.contains("id=\"verifyOut\"") && page.contains("role=\"status\""),
        "the outcome must be announced through a live region"
    );
    assert!(
        !page.contains("onclick=") && !page.contains(" style=\""),
        "no inline handlers or inline styles (docs/05 §9.5, WCAG: the page's own CSP forbids them)"
    );
    // 页面里那个唯一的空请求体入口（有 body 就是 400）。
    assert!(
        page.contains("xhrSend('POST', '/api/models/reverify', '')"),
        "the request body must be empty, exactly as the endpoint requires"
    );
    // 冻结的占位符计数（§9：nonce 属性 ×3、占位符出现 ×4、令牌 ×3）。
    assert_eq!(
        page.matches("__CSP_NONCE__").count(),
        0,
        "placeholders are injected"
    );
    assert_eq!(
        page.matches("__SRV_TOKEN__").count(),
        0,
        "placeholders are injected"
    );
}

// A1 的门禁本身在 `serve::run::tests`（三个用例：健康 / 损坏 / 缺失），
// **进程边界**那一条在 `tests/serve_startup.rs`。
//
// 为什么不在这个文件里跑子进程：本模块的测试二进制在 `target/<profile>/deps/` 下，
// cargo 在那里不保证注入 `CARGO_BIN_EXE_rapidocr`，靠"猜上一级目录"可能拿到
// `cargo build` 留下的、**没有** `serve` feature 的旧可执行文件——那会让断言对象
// 与本次构建无关。集成测试里有 cargo 的保证。

// ------------------------------- A2：`POST /api/models/reverify`（清缓存 + 冷验证 + 重建引擎）

/// B × A2 的端到端证据：**同体积 + 同 mtime** 的内容替换在真实服务路径上被抓住。
///
/// 旧的 `(path, size, mtime)` 身份对这次替换会命中缓存、继续报 `present`；现在
/// `/api/models` 会把它报成 `corrupt`，并且在 `verification` 的成本账里留下两条痕迹：
/// `partial_mismatches` 增长、本次 `cold_this_call` 至少 1（**只有它**被重新哈希）。
#[test]
fn models_detects_a_same_size_same_mtime_swap_through_the_content_windows() {
    let dir = complete_model_dir("verify-window-swap");
    let path = dir.join("rec.onnx");
    let server = TestServer::start(TestOptions::new(dir, Scripted::fast()));

    let first = server.get("/api/models").json();
    assert_eq!(first["complete"], true, "{first}");
    let mismatches_before = first["verification"]["partial_mismatches"]
        .as_u64()
        .expect("partial_mismatches");

    // 同体积 + 把 mtime 写回原值：stat 身份三项全部不变。
    let identity_before = FileIdentity::of(&path).expect("identity before the swap");
    let before = std::fs::metadata(&path)
        .expect("metadata")
        .modified()
        .expect("mtime");
    let original = std::fs::read(&path).expect("read the healthy model");
    std::fs::write(&path, corrupt_bytes(original.len())).expect("swap the content");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open to restore the timestamp");
    file.set_modified(before).expect("write the old mtime back");
    drop(file);
    let identity_after = FileIdentity::of(&path).expect("identity after the swap");
    assert_eq!(
        identity_after.size(),
        identity_before.size(),
        "the size must be unchanged"
    );
    assert_eq!(
        identity_after.modified(),
        identity_before.modified(),
        "the mtime must be written back"
    );
    assert_ne!(
        identity_after.partial(),
        identity_before.partial(),
        "only the first/last 64 KiB windows can distinguish this swap"
    );

    let second = server.get("/api/models").json();
    assert!(
        second["verification"]["cold_this_call"]
            .as_u64()
            .expect("cold_this_call")
            >= 1,
        "the swap must force a re-hash (a stale hit would make this 0 and miss it): {second}"
    );
    assert!(
        second["verification"]["partial_mismatches"]
            .as_u64()
            .expect("partial_mismatches")
            > mismatches_before,
        "the reason must be visible in the cost ledger: {second}"
    );
    assert_eq!(second["complete"], false, "{second}");
    assert_eq!(second["corrupt"], json!(["rec.onnx"]), "{second}");

    // 端点也报同一个结论（同一条哈希证据），并且 reason 说的是"内容变了"。
    let response = server.post("/api/models/reverify", &[], b"");
    assert_eq!(response.status, 200, "{}", response.text());
    let body = response.json();
    assert_eq!(body["corrupt"], json!(["rec.onnx"]), "{body}");
    let culprit = body["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|file| file["name"] == "rec.onnx")
        .expect("the swapped file must be listed");
    assert_eq!(culprit["state"], "corrupt", "{body}");
    assert_eq!(
        culprit["cause"], "first_sight",
        "a forced verification never consults the cache, so its cause is the cold path: {body}"
    );
}

/// 与夹具**同体积**的替换内容。
///
/// 同体积是这几条用例的要点：`(path, size, mtime)` 三项全部不变，因此只有首尾 64 KiB 的
/// 局部摘要能把这次替换抓出来（旧身份下缓存会继续返回旧摘要，这正是 B 要收窄的窗口）。
fn corrupt_bytes(size: usize) -> Vec<u8> {
    let seed = b"corrupted model bytes";
    (0..size).map(|index| seed[index % seed.len()]).collect()
}

/// A2：损坏但**存在**的模型 → 端点报 `corrupt`，并把服务留在**可定位的错误态**，
/// 而不是"旧引擎继续 ready"。
///
/// 这是本端点的核心价值：只清缓存不重建引擎，页面上的按钮按下去什么都不会变——
/// 流水线的会话缓存（按文件身份失效）可能仍然服务着磁盘上已经不是这个文件的模型。
/// 因此这里断言的是**引擎状态本身**（`failed` + 点名文件 `rec.onnx`），
/// 而不是一句"端点返回了 200"。
///
/// 夹具的写法：先让引擎**真正建起来**（健康模型），再让"服务看到的那份文件"变成
/// 同体积的坏内容，最后调端点。恢复过程（写回正确字节 → ready）在
/// [`reverify_restores_ready_and_rebuilds_the_engine_after_a_healthy_revert`] 里，
/// 那条用例还断言了"会话真的被重建"。
#[test]
fn reverify_reports_a_corrupt_model_and_leaves_the_service_blocked_not_stale_ready() {
    let dir = complete_model_dir("reverify-corrupt");
    // 破坏**文本管线**里的识别模型：它才是引擎要加载的那一份，因此"服务的状态"会从
    // `ready` 变成**可定位的错误态**（公式模型损坏只影响公式队列，普通 OCR 不受影响，
    // 那是 M4 的正确行为，不是本用例要考察的东西）。
    //
    // 同体积 + 把 mtime 写回原值：`(path, size, mtime)` 三项全部不变。旧身份下缓存会
    // 继续报 `present`（这正是 B 要收窄的窗口），因此这一条同时是局部摘要在真实服务
    // 路径上的证据。
    let path = dir.join("rec.onnx");
    let before = std::fs::metadata(&path)
        .expect("metadata")
        .modified()
        .expect("mtime");
    let original = std::fs::read(&path).expect("read the healthy model");
    assert!(
        original.starts_with(b"test model file"),
        "the fixture content must be the healthy one: {:?}",
        String::from_utf8_lossy(&original)
    );
    std::fs::write(&path, corrupt_bytes(original.len())).expect("corrupt the recognizer");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open to restore the timestamp");
    file.set_modified(before).expect("write the old mtime back");
    drop(file);

    let built = Arc::new(AtomicUsize::new(0));
    let server = TestServer::start(TestOptions {
        engine_factory: counting_engine_factory(Arc::clone(&built)),
        ..TestOptions::new(dir.clone(), Scripted::fast())
    });
    // 启动期就如实报告：齐备性判定用的是同一份哈希证据，坏文件不会被当成好文件。
    assert_eq!(
        server.get("/api/status").json()["engine"]["state"],
        "blocked_models_missing",
        "the startup readiness check must already see the corrupt recognizer"
    );
    assert_eq!(built.load(Ordering::SeqCst), 0, "no session was created");

    let response = server.post("/api/models/reverify", &[], b"");
    assert_eq!(response.status, 200, "{}", response.text());
    let body = response.json();
    assert_eq!(
        body["computed"].as_u64(),
        Some(4),
        "every file this run uses was re-hashed: {body}"
    );
    assert_eq!(body["corrupt"], json!(["rec.onnx"]), "{body}");
    assert_eq!(
        body["outcome"], "blocked_models_missing",
        "the state must be locatable, not a stale ready: {body}"
    );
    assert_eq!(body["engine"]["state"], "blocked_models_missing", "{body}");
    assert_eq!(body["engine"]["missing"], json!(["rec.onnx"]), "{body}");
    let culprit = body["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|file| file["name"] == "rec.onnx")
        .expect("the corrupt file must be listed");
    assert_eq!(culprit["state"], "corrupt", "{body}");
    assert_eq!(
        culprit["cause"], "first_sight",
        "this call's cold verification has no earlier identity to compare against: {body}"
    );
    assert_eq!(culprit["digest_computed_this_call"], true, "{body}");

    // 可定位的错误态：后续 OCR 得到 409（而不是"成功但用了旧模型"）。
    let ocr = server.submit_ocr(b"image");
    assert_eq!(ocr.status, 409, "{}", ocr.text());
    assert_eq!(ocr.code(), "models_corrupt", "{}", ocr.text());
    assert_eq!(
        ocr.json()["detail"]["corrupt"],
        json!(["rec.onnx"]),
        "the 409 must name the file"
    );
    // 引擎没有被"重建成功"——模型不齐备时**不创建会话**。
    assert_eq!(
        built.load(Ordering::SeqCst),
        0,
        "a blocked model set must not create a session"
    );
}

/// A2：健康文件被还原后，同一个端点把服务恢复成 `ready`，**并且真的重建了会话**。
///
/// "重建引擎"这一步如果只是清缓存，这个断言会失败（建会话次数不会增加）——
/// 而用户看到的就是"按钮按下去模型没换"。
#[test]
fn reverify_restores_ready_and_rebuilds_the_engine_after_a_healthy_revert() {
    let dir = complete_model_dir("reverify-revert");
    let kept = dir.join("kept.onnx");
    std::fs::write(&kept, std::fs::read(dir.join("rec.onnx")).expect("read")).expect("seed");
    let built = Arc::new(AtomicUsize::new(0));
    let server = TestServer::start(TestOptions {
        engine_factory: counting_engine_factory(Arc::clone(&built)),
        ..TestOptions::new(dir.clone(), Scripted::fast())
    });
    assert_eq!(server.get("/api/status").json()["engine"]["state"], "ready");
    assert_eq!(built.load(Ordering::SeqCst), 1);

    // 写坏 → 端点报 corrupt、引擎状态是可定位的错误态（`failed` + 点名文件）。
    std::fs::write(dir.join("rec.onnx"), b"corrupted same-len").expect("corrupt");
    let broken = server.post("/api/models/reverify", &[], b"").json();
    assert_eq!(broken["outcome"], "failed", "{broken}");
    assert_eq!(broken["corrupt"], json!(["rec.onnx"]), "{broken}");

    // 还原成正确的字节 → 同一个端点必须恢复 ready，并重建会话（状态机是
    // `failed → Loading → Ready`，这条边是合法的）。
    std::fs::copy(&kept, dir.join("rec.onnx")).expect("restore the model");
    let response = server.post("/api/models/reverify", &[], b"");
    assert_eq!(response.status, 200, "{}", response.text());
    let body = response.json();
    assert_eq!(body["outcome"], "ready", "{body}");
    assert_eq!(body["engine"]["state"], "ready", "{body}");
    assert!(body["load_ms"].as_u64().is_some(), "{body}");
    assert_eq!(body["content_changed"], json!([]), "{body}");
    assert_eq!(body["missing"], json!([]), "{body}");
    assert_eq!(
        built.load(Ordering::SeqCst),
        2,
        "the endpoint must rebuild the engine, not just clear a cache"
    );
    assert_eq!(server.get("/api/status").json()["engine"]["state"], "ready");

    // 恢复正常之后普通 OCR 也要能用（不是"只把状态字段改回 ready"）。
    let id = server.submit_ocr(b"image").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    assert_eq!(
        server.wait_terminal(&id, Duration::from_secs(20))["state"],
        "succeeded"
    );
}

/// A2：**缓存命中之后**调用本端点，摘要必须被**重新计算**（这就是"重新校验"的定义）。
///
/// 断言的是绝对数字：这个模型集里有 4 个本次运行会用到的文件（det/rec/dict/fx），
/// 因此 `computed == 4`，且每个文件的 `cause` 都不是 `cache_hit`（冷验证不查缓存）。
///
/// 注：库的校验缓存与它的累计账都是**进程级**的（同一个进程看到的同一个文件只有一种
/// 结论），而 `cargo test` 并行跑用例，因此这里只断言"缓存命中确实发生了"
/// （`cold_this_call == 0` 且 `cache_hits` 增长），不去断言某个精确的增量。
#[test]
fn reverify_recomputes_digests_that_the_cache_would_have_answered() {
    let dir = complete_model_dir("reverify-cold");
    let server = TestServer::start(TestOptions::new(dir, Scripted::fast()));

    // 预热缓存：两次 `/api/models` 全程命中，`cold_this_call == 0`。
    let first = server.get("/api/models").json();
    assert_eq!(first["verification"]["cold_this_call"], 0, "{first}");
    let hits_before = first["verification"]["cache_hits"]
        .as_u64()
        .expect("cache_hits");
    let second = server.get("/api/models").json();
    assert_eq!(second["verification"]["cold_this_call"], 0, "{second}");
    assert!(
        second["verification"]["cache_hits"]
            .as_u64()
            .expect("cache_hits")
            > hits_before,
        "the cache really answered the files this time: {second}"
    );

    let response = server.post("/api/models/reverify", &[], b"");
    assert_eq!(response.status, 200, "{}", response.text());
    let body = response.json();
    assert_eq!(
        body["computed"].as_u64(),
        Some(4),
        "a cache hit must not survive the forced re-verification: {body}"
    );
    assert_eq!(body["verification"]["digests_computed"], 4, "{body}");
    let files = body["files"].as_array().expect("files");
    assert_eq!(files.len(), 4, "{body}");
    for file in files {
        assert_ne!(
            file["cause"], "cache_hit",
            "a forced verification never reports a cache hit: {body}"
        );
        assert_eq!(file["digest_computed_this_call"], true, "{body}");
    }
    // 请求体必须为空：这是无参数动作，任何 body 都是协议级错误。
    let with_body = server.post("/api/models/reverify", &[], b"{}");
    assert_eq!(with_body.status, 400, "{}", with_body.text());
}

/// A2：并发调用只有一个真的跑（**单飞**），另一个立刻 503 `busy`；并且整个序列
/// **不在 accept 线程上**。
///
/// 确定性地构造"两个调用重叠"：靠 [`ServeContext::post_verify`] 把第一次调用精确地停在
/// "校验结论已经算完、引擎还没重建"的窗口里——请求体因此也还没有发完。此时从**另一个
/// 连接**发第二次请求，它必须立刻拿到 503（不排队）；而 `busy` 的响应正是在 accept 线程上
/// 产生的，这就是"它没有被这个序列占住"的证据（旧实现在 accept 线程上跑的话，
/// 第二次请求连响应都拿不到，会一直等到第一次结束）。
#[test]
fn concurrent_reverifications_are_single_flight_and_never_run_on_the_accept_thread() {
    let dir = complete_model_dir("reverify-busy");
    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(0);
    let server = TestServer::start(TestOptions {
        post_verify: Some(Box::new(move || {
            let _ = tx.send(());
        })),
        ..TestOptions::new(dir, Scripted::fast())
    });

    // 第一次调用：完整的请求字节（**空 body**），一次写完。
    //
    // 连接保持打开、响应尚未写出，因为这一次序列被 [`ServeContext::post_verify`] 的钩子
    // 按在"校验已算完、引擎还没重建"的窗口里——"序列仍在进行"因此是确定的事实，
    // 而不是靠时序猜出来的。
    let mut stream = TcpStream::connect(server.addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("read timeout");
    let head = format!(
        "POST /api/models/reverify HTTP/1.1\r\nConnection: close\r\nHost: {}\r\n\
         X-RapidOCR-Token: {}\r\nOrigin: {}\r\nContent-Length: 0\r\n\r\n",
        server.host, server.token, server.origin
    );
    stream
        .write_all(head.as_bytes())
        .expect("write the request");
    stream.flush().expect("flush");

    // 服务端已经冷验证完（钩子被调用），但还没写响应。
    rx.recv_timeout(Duration::from_secs(30))
        .expect("the first call must reach the post-verification hook");

    // 第二个连接：单飞资格已被第一次调用持有 → 立刻 503 busy，而不是排队。
    let second = Instant::now();
    let response = server.post("/api/models/reverify", &[], b"");
    let elapsed = second.elapsed();
    assert_eq!(response.status, 503, "{}", response.text());
    assert_eq!(response.code(), "busy", "{}", response.text());
    assert!(
        elapsed < Duration::from_secs(5),
        "the second call must not queue behind the first: {elapsed:?}"
    );

    // 第一次调用随后照常完成：连接关闭，响应是一个 200 + 逐文件结论。
    let mut buffer = Vec::new();
    stream
        .read_to_end(&mut buffer)
        .expect("read the first response");
    let first = parse_response(&buffer);
    assert_eq!(first.status, 200, "{}", first.text());
    let body = first.json();
    assert_eq!(body["computed"].as_u64(), Some(4), "{}", first.text());
    assert_eq!(body["outcome"], "ready", "{}", first.text());
}

// ---------------------------------------------------------------- M4：评估
/// `POST /api/evaluate`：清单里每个用例跑一次真实识别路径，返回**库的**汇总
/// （`EvaluationSummary` + CLI 附加的运行时字段）。
///
/// 指标由库的 `evaluate_case` 计算，因此期望值在这里独立算一遍
/// （`cer=0` 的用例 + 一个不匹配的用例），而不是断言"有个数字"。
#[test]
fn the_evaluate_endpoint_returns_the_library_summary() {
    let dir = complete_model_dir("m4-evaluate");
    // 脚本化后端为每个区域产出 `region-<i>`，三个区域换行拼接 → 这就是"预测文本"。
    let scripted = Scripted::fast();
    let server = TestServer::start(TestOptions::new(dir.clone(), scripted));

    let manifest = eval_manifest(
        &dir,
        "eval-ok",
        &[
            ("01.png", "region-0\nregion-1\nregion-2"),
            ("02.png", "totally different"),
        ],
    );
    let response = server.post(
        "/api/evaluate",
        &[("Content-Type", "application/json")],
        format!("{{\"manifest\":{:?}}}", manifest.display().to_string()).as_bytes(),
    );
    assert_eq!(response.status, 200, "{}", response.text());
    let report = response.json();

    let expected_second = cer("totally different", "region-0\nregion-1\nregion-2");
    let expected_mean = (0.0_f32 + expected_second) / 2.0;
    assert_eq!(report["cases"].as_array().expect("cases").len(), 2);
    assert_eq!(report["cases"][0]["image"], "01.png");
    assert_eq!(report["cases"][0]["cer"], 0.0);
    assert_eq!(report["cases"][0]["exact_text"], true);
    assert_eq!(report["cases"][1]["image"], "02.png");
    assert!(
        (report["cases"][1]["cer"].as_f64().expect("cer") - f64::from(expected_second)).abs()
            < 1e-6,
        "{report}"
    );
    assert!(
        (report["mean_cer"].as_f64().expect("mean_cer") - f64::from(expected_mean)).abs() < 1e-6
    );
    assert_eq!(report["exact_match_rate"], 0.5);
    // 与 `rapidocr evaluate` 的报告字段逐字段相同（同一份实现）。
    assert_eq!(report["iou_threshold"], 0.5);
    assert_eq!(report["manifest_file"], "manifest.json");
    assert!(report["peak_working_set_bytes"].as_u64().unwrap_or(0) > 0);
    assert!(report["memory_source"].is_string());
    assert!(report["ort_runtime"].is_object());
    assert!(report["ort_runtime_version"].is_string());
    assert!(
        !report.to_string().contains(&dir.display().to_string()),
        "no absolute path in the report: {report}"
    );
}

/// 评估的拒绝路径全部是**可定位**的 400，而不是笼统的 `bad_request`。
#[test]
fn an_invalid_evaluation_request_is_a_locating_400() {
    let dir = complete_model_dir("m4-evaluate-refused");
    let server = TestServer::start(TestOptions::new(dir.clone(), Scripted::fast()));

    let no_body = server.post(
        "/api/evaluate",
        &[("Content-Type", "application/json")],
        b"{}",
    );
    assert_eq!(no_body.status, 400, "{}", no_body.text());

    let extra_key = server.post(
        "/api/evaluate",
        &[("Content-Type", "application/json")],
        br#"{"manifest":"a.json","iou_threshold":0.9}"#,
    );
    assert_eq!(extra_key.status, 400);

    let missing = dir.join("no-such-manifest.json");
    let response = server.post(
        "/api/evaluate",
        &[("Content-Type", "application/json")],
        format!("{{\"manifest\":{:?}}}", missing.display().to_string()).as_bytes(),
    );
    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(response.code(), "bad_request");
    let reason = response.json()["detail"]["reason"]
        .as_str()
        .expect("reason")
        .to_string();
    assert!(reason.contains("no-such-manifest.json"), "{reason}");

    // 超过 `--max-eval-cases` 的清单：同样 400，理由点名那个开关。
    let manifest = eval_manifest(
        &dir,
        "eval-too-many",
        &[("a.png", "x"), ("b.png", "x"), ("c.png", "x")],
    );
    let server = TestServer::start(TestOptions::new(dir.clone(), Scripted::fast()).with_limits(
        RawServeLimits {
            max_eval_cases: 2,
            ..RawServeLimits::default()
        },
    ));
    let response = server.post(
        "/api/evaluate",
        &[("Content-Type", "application/json")],
        format!("{{\"manifest\":{:?}}}", manifest.display().to_string()).as_bytes(),
    );
    assert_eq!(response.status, 400, "{}", response.text());
    let reason = response.json()["detail"]["reason"]
        .as_str()
        .expect("reason")
        .to_string();
    assert!(reason.contains("--max-eval-cases"), "{reason}");
}

/// 评审 P2-3：没有 `--eval-root` 时 `/api/evaluate` **整体拒绝**，
/// 而且理由点名要加哪个开关（可定位，而不是一句"评估不可用"）。
#[test]
fn evaluation_is_refused_without_an_explicit_eval_root() {
    let dir = complete_model_dir("m4-evaluate-no-root");
    let server =
        TestServer::start(TestOptions::new(dir.clone(), Scripted::fast()).without_eval_root());
    // 清单本身是合法的、就在模型目录里：拒绝必须来自"没有沙箱"，不是输入问题。
    let manifest = eval_manifest(&dir, "eval-no-root", &[("01.png", "x")]);
    let response = server.post(
        "/api/evaluate",
        &[("Content-Type", "application/json")],
        format!("{{\"manifest\":{:?}}}", manifest.display().to_string()).as_bytes(),
    );
    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(response.code(), "bad_request");
    let reason = response.json()["detail"]["reason"]
        .as_str()
        .expect("reason")
        .to_string();
    assert!(reason.contains("--eval-root"), "{reason}");
    assert!(reason.contains("disabled"), "{reason}");
}

/// 评审 P2-3：清单**自己**在沙箱外 → 拒绝，并点名那个路径。
#[test]
fn an_evaluation_manifest_outside_the_eval_root_is_refused() {
    let dir = complete_model_dir("m4-evaluate-escape");
    let server = TestServer::start(TestOptions::new(dir.clone(), Scripted::fast()));

    // 清单写在沙箱之外（`m2_root()` 与模型目录是两棵不同的树）。
    let outside = m2_root().join(format!("outside-manifest-{}", unique()));
    std::fs::create_dir_all(&outside).expect("create the outside dir");
    let manifest = outside.join("manifest.json");
    std::fs::write(&manifest, br#"[{"image":"01.png","text":"x","boxes":[]}]"#).expect("write");
    std::fs::write(outside.join("01.png"), b"image bytes").expect("write the image");

    let response = server.post(
        "/api/evaluate",
        &[("Content-Type", "application/json")],
        format!("{{\"manifest\":{:?}}}", manifest.display().to_string()).as_bytes(),
    );
    assert_eq!(response.status, 400, "{}", response.text());
    let reason = response.json()["detail"]["reason"]
        .as_str()
        .expect("reason")
        .to_string();
    assert!(reason.contains("--eval-root sandbox"), "{reason}");
    assert!(
        reason.contains("manifest"),
        "the offending path must be named: {reason}"
    );
    std::fs::remove_dir_all(&outside).ok();
}

/// 评审 P2-3：清单在沙箱内，但它引用的**图片**越界（绝对路径、`..`、符号链接）
/// → 拒绝，并点名那个图片路径。三条路径都必须被抓住，而不是只看清单。
#[test]
fn an_evaluation_image_outside_the_eval_root_is_refused() {
    let dir = complete_model_dir("m4-evaluate-image-escape");
    let server = TestServer::start(TestOptions::new(dir.clone(), Scripted::fast()));

    // 沙箱之外的**真实存在**的一张图片：越界必须由"规范化后的包含关系"判定，
    // 而不是"路径不存在"这种偶然原因。
    let outside_dir = test_root().join(format!("outside-image-{}", unique()));
    std::fs::create_dir_all(&outside_dir).expect("create the outside dir");
    let outside_image = outside_dir.join("01.png");
    std::fs::write(&outside_image, b"image bytes outside the sandbox").expect("write the image");
    let outside_name = outside_dir
        .file_name()
        .expect("a directory name")
        .to_string_lossy()
        .into_owned();

    let sandbox_dir = dir.join(format!("escape-images-{}", unique()));
    std::fs::create_dir_all(&sandbox_dir).expect("create the sandbox subdir");

    // 1) 绝对路径越界。
    let absolute = sandbox_dir.join("absolute.json");
    std::fs::write(
        &absolute,
        format!(
            "[{{\"image\":{:?},\"text\":\"x\",\"boxes\":[]}}]",
            outside_image.display().to_string()
        ),
    )
    .expect("write the absolute manifest");
    let response = server.post(
        "/api/evaluate",
        &[("Content-Type", "application/json")],
        format!("{{\"manifest\":{:?}}}", absolute.display().to_string()).as_bytes(),
    );
    assert_eq!(response.status, 400, "{}", response.text());
    let reason = response.json()["detail"]["reason"]
        .as_str()
        .expect("reason")
        .to_string();
    assert!(
        reason.contains("outside the --eval-root sandbox"),
        "an escaping absolute path must be named as an escape, not as a missing file: {reason}"
    );
    assert!(reason.contains("01.png"), "{reason}");

    // 2) `..` 相对路径越界：沙箱根是 `<test_root>/complete-…`，清单在它下面的子目录里，
    //    因此 `../../<outside>/01.png` 是一条**真实存在**却在沙箱外的路径。
    let relative = sandbox_dir.join("relative.json");
    std::fs::write(
        &relative,
        format!(
            "[{{\"image\":{:?},\"text\":\"x\",\"boxes\":[]}}]",
            format!("../../{outside_name}/01.png")
        ),
    )
    .expect("write the relative manifest");
    let response = server.post(
        "/api/evaluate",
        &[("Content-Type", "application/json")],
        format!("{{\"manifest\":{:?}}}", relative.display().to_string()).as_bytes(),
    );
    assert_eq!(response.status, 400, "{}", response.text());
    let reason = response.json()["detail"]["reason"]
        .as_str()
        .expect("reason")
        .to_string();
    assert!(
        reason.contains("outside the --eval-root sandbox"),
        "a `..` escape must be refused as an escape: {reason}"
    );

    // 3) 符号链接逃逸：规范化会解析它，因此指向沙箱外的链接同样越界。
    //    Windows 上创建符号链接需要权限/开发者模式：环境不允许时如实说明这一条没有
    //    在本机跑，而绝对路径与 `..` 两条已经覆盖同一条"规范化后必须落在根内"的规则。
    let link = sandbox_dir.join("link.png");
    match std::os::windows::fs::symlink_file(&outside_image, &link) {
        Ok(()) => {
            let linked = sandbox_dir.join("symlink.json");
            std::fs::write(&linked, br#"[{"image":"link.png","text":"x","boxes":[]}]"#)
                .expect("write the symlink manifest");
            // 链接的**字面路径**在沙箱内，规范化后却指向沙箱外——这正是"只做字符串
            // 前缀判断"会漏掉的情形。
            let response = server.post(
                "/api/evaluate",
                &[("Content-Type", "application/json")],
                format!("{{\"manifest\":{:?}}}", linked.display().to_string()).as_bytes(),
            );
            assert_eq!(response.status, 400, "{}", response.text());
            assert!(
                response.json()["detail"]["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("outside the --eval-root sandbox"),
                "a symlink escape must be refused: {}",
                response.text()
            );
        }
        Err(error) => eprintln!(
            "serve tests: cannot create a symlink to prove the symlink-escape case ({error}); \
             the absolute and `..` cases exercise the same canonical-containment rule"
        ),
    }

    std::fs::remove_dir_all(&outside_dir).ok();
}

/// 模型缺失时评估与 OCR 一样是 409（同一条准入），而不是在建会话时崩掉。
#[test]
fn evaluation_needs_the_same_model_admission_as_ocr() {
    let dir = empty_model_dir("m4-evaluate-models");
    let server = TestServer::start(TestOptions::new(dir.clone(), Scripted::fast()));
    // 清单本身必须能读出来：否则先得到的是"清单读不出来"的 400（那是输入问题），
    // 而不是模型准入的 409。两件事各自可定位，顺序在 `evaluate::run` 里写死。
    let manifest = eval_manifest(&dir, "admission", &[("01.png", "x")]);
    let response = server.post(
        "/api/evaluate",
        &[("Content-Type", "application/json")],
        format!("{{\"manifest\":{:?}}}", manifest.display().to_string()).as_bytes(),
    );
    assert_eq!(response.status, 409, "{}", response.text());
    assert_eq!(response.code(), "models_missing");
    assert_eq!(
        response.json()["detail"]["missing"],
        server.get("/api/models").json()["missing"]
    );
}

/// 同时只允许一个评估：第二个请求得到 503 `busy`（不排队），第一个照常完成。
#[test]
fn a_second_evaluation_is_refused_while_one_is_running() {
    let dir = complete_model_dir("m4-evaluate-busy");
    let gate = Gate::new();
    let server = TestServer::start(TestOptions::new(
        dir.clone(),
        Scripted::fast().with_gate(Arc::clone(&gate)),
    ));
    let manifest = eval_manifest(
        &dir,
        "eval-busy",
        &[("01.png", "region-0\nregion-1\nregion-2")],
    );
    let body = format!("{{\"manifest\":{:?}}}", manifest.display().to_string()).into_bytes();

    let addr = server.addr;
    let host = server.host.clone();
    let origin = server.origin.clone();
    let token = server.token.clone();
    let first_body = body.clone();
    let first = std::thread::spawn(move || {
        raw_request(
            addr,
            "POST",
            "/api/evaluate",
            &[
                ("Host", host.as_str()),
                ("X-RapidOCR-Token", token.as_str()),
                ("Origin", origin.as_str()),
                ("Content-Type", "application/json"),
            ],
            Some(&first_body),
        )
    });
    // 评估线程已经进到第一张图（闸门在推理之前）。
    gate.arrive();
    let second = server.post(
        "/api/evaluate",
        &[("Content-Type", "application/json")],
        &body,
    );
    assert_eq!(second.status, 503, "{}", second.text());
    assert_eq!(second.code(), "busy");
    gate.release();
    let first = first.join().expect("the evaluation thread must finish");
    assert_eq!(first.status, 200, "{}", first.text());
    // 资格已释放：第三次请求可以正常开始（这里用一个不存在的清单证明它走到了解析阶段，
    // 而不是又被 503 挡住）。
    let third = server.post(
        "/api/evaluate",
        &[("Content-Type", "application/json")],
        br#"{"manifest":"definitely-not-here.json"}"#,
    );
    assert_eq!(third.status, 400, "{}", third.text());
}

/// 评估的逐例文本 → 期望 CER（与库同一个公式：字符级编辑距离 / 参考长度）。
fn cer(reference: &str, hypothesis: &str) -> f32 {
    rapid_ocr_rs::evaluation::ocr::character_error_rate(reference, hypothesis)
}

/// 写一份 `rapidocr evaluate --manifest` 格式的清单 + 对应的小图片文件。
///
/// 图片内容不重要（脚本化后端不解码），但**文件必须存在**：端点读不到文件时是 400。
fn eval_manifest(dir: &Path, label: &str, cases: &[(&str, &str)]) -> PathBuf {
    let root = dir.join(format!("{label}-{}", unique()));
    std::fs::create_dir_all(&root).expect("create the manifest dir");
    let mut manifest = String::from("[");
    for (index, (image, text)) in cases.iter().enumerate() {
        std::fs::write(root.join(image), format!("image bytes for {image}")).expect("write image");
        if index > 0 {
            manifest.push(',');
        }
        manifest.push_str(&format!(
            "{{\"image\":{image:?},\"text\":{text:?},\"boxes\":[]}}"
        ));
    }
    manifest.push(']');
    let path = root.join("manifest.json");
    std::fs::write(&path, manifest).expect("write the manifest");
    path
}

#[test]
fn the_documented_route_set_is_the_only_one_that_exists() {
    // 内联页面必须真的在仓库里（`include_str!` 的编译期保证之外，再加一条运行期断言，
    // 让"页面被误删"在测试里就可见，而不是等到打包时才发现）。
    assert!(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/bin/web/index.html")
            .is_file(),
        "the inlined page must exist"
    );
}

// ---------------------------------------------------------------- M2 的辅助

/// 目录里的临时文件（`.part-*`）：任何失败/取消路径都不允许留下它。
fn part_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("the model dir must be readable")
        .map(|entry| {
            entry
                .expect("a readable entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.contains(".part"))
        .collect();
    names.sort();
    names
}

/// 计数的引擎工厂：断言"到底建立了几次会话"。
fn counting_engine_factory(counter: Arc<AtomicUsize>) -> EngineFactory {
    Arc::new(move |_config: &EngineConfig| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(ScriptedBackend {
            state: Scripted::fast(),
        }))
    })
}

/// 建立会话时阻塞的引擎工厂：确定性地观察 `loading` 状态。
fn gated_engine_factory(gate: Arc<Gate>, counter: Arc<AtomicUsize>) -> EngineFactory {
    Arc::new(move |_config: &EngineConfig| {
        counter.fetch_add(1, Ordering::SeqCst);
        gate.wait();
        Ok(Box::new(ScriptedBackend {
            state: Scripted::fast(),
        }))
    })
}

// ------------------------------------------------------- M3：引擎会话脚本（provider 切换）

/// "第几次建立会话"的脚本：在哪一次阻塞、哪几次失败、每次的 `selected_ep` 标签。
///
/// M3 的 provider 切换必须在**同一个进程里**观察到三次建会话（启动、切换、回滚），
/// 而这三次调用只有次数上的区别，因此用序号来驱动它是最直接的做法。
#[derive(Clone)]
struct SessionPlan {
    calls: Arc<AtomicUsize>,
    /// 第 N 次（1 基）调用在返回前阻塞在闸门上（观察 `rebuilding`）。
    gate_at: Option<usize>,
    /// 第 N 次调用返回错误（模拟"新 provider 建不起来"）。
    fail_at: Vec<usize>,
    /// 建立出来的后端每次识别要花多久（"排空"要有一个真在跑的任务）。
    delay: Duration,
}

impl SessionPlan {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            gate_at: None,
            fail_at: Vec::new(),
            delay: Duration::ZERO,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn factory(&self, gate: Option<Arc<Gate>>) -> EngineFactory {
        let plan = self.clone();
        Arc::new(
            move |_config: &EngineConfig| -> Result<Box<dyn OcrBackend>, RapidOcrError> {
                let index = plan.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if plan.gate_at == Some(index)
                    && let Some(gate) = &gate
                {
                    gate.wait();
                }
                if plan.fail_at.contains(&index) {
                    return Err(RapidOcrError::UnsupportedProvider(format!(
                        "scripted session {index} cannot be created"
                    )));
                }
                Ok(Box::new(ScriptedBackend {
                    state: Scripted {
                        // 标签编码"第几个会话"：`/api/status` 的 `selected_ep` 因此能证明
                        // 换上的是**新**引擎，而不是留着旧的那一个。
                        ep: format!("scripted-{index}"),
                        delay: plan.delay,
                        ..Scripted::fast()
                    },
                }))
            },
        )
    }
}

/// 闸门的 RAII 放行器：**任何**提前失败（panic 展开）都会放行被闸住的会话创建线程。
///
/// 没有它的话，一个断言失败会把那个线程永远留在闸门后面，而它持有 `engine_load`，
/// 于是 `ServeRuntime::stop()` 的 join 也会跟着挂住——测试失败的方式会从"一条红"
/// 变成"整个测试进程挂死"。
struct ReleaseOnDrop {
    gate: Arc<Gate>,
    released: AtomicBool,
}

impl ReleaseOnDrop {
    fn new(gate: Arc<Gate>) -> Self {
        Self {
            gate,
            released: AtomicBool::new(false),
        }
    }

    fn release(&self) {
        if !self.released.swap(true, Ordering::SeqCst) {
            self.gate.release();
        }
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.release();
    }
}

// ---------------------------------------------------------------- M2：下载（无网络）

/// 一个真实的下载任务从 202 走到 succeeded：进度逐文件推进、文件真的落盘、
/// `/api/models` 变为 complete（哈希是库的真实校验），而引擎**不**在后台创建（§7.6）。
#[test]
fn a_download_job_fetches_every_missing_file_and_makes_the_set_complete() {
    let dir = manifest_model_dir_text_only("download-ok", &["det.onnx"], None);
    let scripted = ScriptedDownload::default();
    let server = TestServer::start(TestOptions {
        allow_download: true,
        downloader: scripted.clone().factory(),
        ..TestOptions::new(dir.clone(), Scripted::fast())
    });

    let before = server.get("/api/models").json();
    assert_eq!(before["source"], "local_manifest");
    assert_eq!(before["complete"], false);
    assert_eq!(
        before["missing"],
        serde_json::json!(["rec.onnx", "dict.txt"])
    );

    let response = server.download("test-set");
    assert_eq!(response.status, 202, "{}", response.text());
    let accepted = response.json();
    assert_eq!(accepted["kind"], "model_download");
    assert_eq!(
        accepted["queue"], "download",
        "a download job is not in the text queue"
    );
    assert_eq!(accepted["state"], "queued");
    assert_eq!(accepted["download"]["files_total"], 2);
    assert_eq!(accepted["download"]["files_done"], 0);
    let id = accepted["job_id"].as_str().expect("job id").to_string();

    let view = server.wait_terminal(&id, Duration::from_secs(20));
    assert_eq!(view["state"], "succeeded", "{view}");
    assert_eq!(view["failure"], serde_json::Value::Null);
    assert_eq!(view["queue"], "download");
    assert_eq!(view["download"]["files_done"], 2);
    assert_eq!(view["download"]["files_total"], 2);
    assert_eq!(view["download"]["current_file"], serde_json::Value::Null);
    let expected_bytes: u64 = ["rec.onnx", "dict.txt"]
        .iter()
        .map(|name| model_file_bytes(name).len() as u64)
        .sum();
    assert_eq!(view["download"]["bytes_done"], expected_bytes);
    assert_eq!(view["download"]["bytes_total"], expected_bytes);

    assert_eq!(
        scripted.written(),
        vec!["rec.onnx".to_string(), "dict.txt".to_string()]
    );
    let after = server.get("/api/models").json();
    assert_eq!(after["complete"], true, "{after}");
    assert_eq!(after["missing"], serde_json::json!([]));
    assert_eq!(after["sets"][0]["complete"], true);
    assert!(part_files(&dir).is_empty());

    // §7.6：下载完成**不**自动创建引擎（避免后台突然占用数百 MB）。
    assert_eq!(
        server.get("/api/status").json()["engine"]["state"],
        "blocked_models_missing"
    );
}

/// 哈希失败：任务 failed，错误体带机器可读分类（客户端不必做字符串匹配），目录里不留文件。
#[test]
fn a_hash_failure_fails_the_download_job_with_a_structured_error() {
    let dir = manifest_model_dir("download-hash", &["det.onnx"], None);
    let server = TestServer::start(TestOptions {
        allow_download: true,
        downloader: ScriptedDownload::with_plan(vec![Step::HashMismatch]).factory(),
        ..TestOptions::new(dir.clone(), Scripted::fast())
    });
    let id = server.download("test-set").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();

    let view = server.wait_terminal(&id, Duration::from_secs(20));
    assert_eq!(view["state"], "failed", "{view}");
    assert_eq!(view["failure"]["status"], 502);
    assert_eq!(view["failure"]["code"], "download_failed");
    assert_eq!(view["failure"]["detail"]["kind"], "hash_mismatch");
    assert!(
        view["error"]
            .as_str()
            .is_some_and(|text| text.contains("SHA-256")),
        "{view}"
    );
    assert!(!dir.join("rec.onnx").exists());
    assert!(!dir.join("dict.txt").exists());
    assert!(part_files(&dir).is_empty(), "no temp file may survive");
}

/// §6.2：声明体积超过 `--max-download-mb` → **同步** 413，两个数值都在 `detail` 里，
/// 且不建任务、不发请求。
#[test]
fn a_download_budget_refusal_is_a_413_with_both_numbers() {
    let declared = 10 * 1024 * 1024;
    let dir = manifest_model_dir_text_only("download-budget", &["det.onnx"], Some(declared));
    let scripted = ScriptedDownload::default();
    let server = TestServer::start(TestOptions {
        limits: RawServeLimits {
            max_download_mb: 1,
            ..RawServeLimits::default()
        },
        allow_download: true,
        downloader: scripted.clone().factory(),
        ..TestOptions::new(dir, Scripted::fast())
    });

    let response = server.download("test-set");
    assert_eq!(response.status, 413, "{}", response.text());
    assert_eq!(response.code(), "payload_too_large");
    let detail = &response.json()["detail"];
    assert_eq!(detail["limit_bytes"], 1024 * 1024);
    assert_eq!(detail["observed_bytes"], 2 * declared);
    assert!(
        scripted.hosts_per_call().is_empty(),
        "a refused task must not reach the downloader"
    );
}

/// §6.5：任务级磁盘核算不足 → **同步** 507，需求与可用两个数值都给出，且不建任务。
#[test]
fn a_disk_space_refusal_is_a_507_with_both_numbers() {
    let declared = 10 * 1024 * 1024;
    let dir = manifest_model_dir_text_only("download-disk", &["det.onnx"], Some(declared));
    let scripted = ScriptedDownload::default();
    let server = TestServer::start(TestOptions {
        allow_download: true,
        downloader: scripted.clone().factory(),
        free_space: Arc::new(|_dir: &Path| Ok(10)),
        ..TestOptions::new(dir, Scripted::fast())
    });

    let response = server.download("test-set");
    assert_eq!(response.status, 507, "{}", response.text());
    assert_eq!(response.code(), "insufficient_disk_space");
    let detail = &response.json()["detail"];
    assert_eq!(detail["required_bytes"], 2 * declared);
    assert_eq!(detail["available_bytes"], 10);
    assert!(scripted.hosts_per_call().is_empty());
}

/// 逐文件失败：已经校验通过的文件**保留**，任务给出失败分类（§6.3 的"不留半成品"）。
#[test]
fn a_per_file_failure_keeps_the_files_that_already_verified() {
    let dir = manifest_model_dir("download-partial", &["det.onnx"], None);
    let server = TestServer::start(TestOptions {
        allow_download: true,
        downloader: ScriptedDownload::with_plan(vec![Step::Write, Step::Network]).factory(),
        ..TestOptions::new(dir.clone(), Scripted::fast())
    });
    let id = server.download("test-set").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();

    let view = server.wait_terminal(&id, Duration::from_secs(20));
    assert_eq!(view["state"], "failed", "{view}");
    assert_eq!(view["failure"]["code"], "download_failed");
    assert_eq!(view["failure"]["detail"]["kind"], "network");
    assert_eq!(view["download"]["files_done"], 1);
    assert_eq!(
        view["download"]["current_file"], "dict.txt",
        "the failing file stays named in the progress"
    );
    assert!(dir.join("rec.onnx").is_file(), "the verified file stays");
    assert!(!dir.join("dict.txt").exists());
    assert!(part_files(&dir).is_empty());
}

/// 运行中的进度可观测（**确定性**，用闸门而不是 sleep）：逐文件 done/total、
/// 字节 done/total、当前文件名，全部来自 `GET /api/jobs/{id}`（§4.2）。
#[test]
fn the_download_progress_is_observable_while_the_job_runs() {
    let dir = manifest_model_dir_text_only("download-progress", &["det.onnx"], None);
    let gate = Gate::new();
    let server = TestServer::start(TestOptions {
        allow_download: true,
        downloader: ScriptedDownload::with_gate(Arc::clone(&gate)).factory(),
        ..TestOptions::new(dir, Scripted::fast())
    });
    let id = server.download("test-set").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    let rec_len = model_file_bytes("rec.onnx").len() as u64;
    let dict_len = model_file_bytes("dict.txt").len() as u64;

    // 第一个文件已经开始（闸门在 `file_started` 之后、写入之前）。
    gate.arrive();
    let view = server.get(&format!("/api/jobs/{id}")).json();
    assert_eq!(view["state"], "running", "{view}");
    assert_eq!(view["download"]["current_file"], "rec.onnx");
    assert_eq!(view["download"]["files_done"], 0);
    assert_eq!(view["download"]["files_total"], 2);
    assert_eq!(view["download"]["bytes_done"], 0);
    assert_eq!(view["download"]["bytes_total"], rec_len + dict_len);
    gate.release();

    // 第二个文件已经开始：第一个文件的字节已经计入。
    gate.arrive();
    let view = server.get(&format!("/api/jobs/{id}")).json();
    assert_eq!(view["download"]["current_file"], "dict.txt");
    assert_eq!(view["download"]["files_done"], 1);
    assert_eq!(view["download"]["bytes_done"], rec_len);
    gate.release();

    let view = server.wait_terminal(&id, Duration::from_secs(20));
    assert_eq!(view["state"], "succeeded", "{view}");
    assert_eq!(view["download"]["files_done"], 2);
    assert_eq!(view["download"]["bytes_done"], rec_len + dict_len);
    assert_eq!(view["download"]["current_file"], serde_json::Value::Null);
}

/// §6.6：运行中的下载取消在**文件边界**生效——当前文件下载完，后续文件不再开始，
/// 已校验文件保留，任务终态是 `cancelled`（不假装"已取消"，也不假装"不能取消"）。
#[test]
fn a_running_download_is_cancelled_at_a_file_boundary() {
    let dir = manifest_model_dir("download-cancel", &["det.onnx"], None);
    let gate = Gate::new();
    let server = TestServer::start(TestOptions {
        allow_download: true,
        downloader: ScriptedDownload::with_gate(Arc::clone(&gate)).factory(),
        ..TestOptions::new(dir.clone(), Scripted::fast())
    });
    let id = server.download("test-set").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();

    gate.arrive(); // `rec.onnx` 正在下载
    let cancelled = server.post(&format!("/api/jobs/{id}/cancel"), &[], b"");
    assert_eq!(cancelled.status, 200, "{}", cancelled.text());
    let view = cancelled.json();
    assert_eq!(
        view["state"], "running",
        "the current file is still being downloaded"
    );
    assert_eq!(view["cancel_requested"], true);
    gate.release(); // 当前文件完成 → 下一个文件在边界上被拒绝

    let view = server.wait_terminal(&id, Duration::from_secs(20));
    assert_eq!(view["state"], "cancelled", "{view}");
    assert_eq!(view["cancel_requested"], true);
    assert_eq!(
        view["download"]["files_done"], 1,
        "the current file was kept"
    );
    assert!(dir.join("rec.onnx").is_file(), "the verified file stays");
    assert!(
        !dir.join("dict.txt").exists(),
        "the next file is never started"
    );
    assert!(
        part_files(&dir).is_empty(),
        "no temp file survives a cancellation"
    );
}

/// 下载器报告"在文件边界取消"（库的 `DownloadError::Cancelled`）：任务必须落在
/// `cancelled` 上，而不是永远停在 `running`（worker 会把取消请求补登记，见
/// `ServeShared::finish_download_cancelled`）。
#[test]
fn a_download_error_cancelled_lands_on_cancelled_not_running() {
    let dir = manifest_model_dir("download-cancelled-error", &["det.onnx"], None);
    let server = TestServer::start(TestOptions {
        allow_download: true,
        downloader: ScriptedDownload::with_plan(vec![Step::Cancelled]).factory(),
        ..TestOptions::new(dir.clone(), Scripted::fast())
    });
    let id = server.download("test-set").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();

    let view = server.wait_terminal(&id, Duration::from_secs(20));
    assert_eq!(view["state"], "cancelled", "{view}");
    assert_eq!(
        view["failure"],
        serde_json::Value::Null,
        "a cancellation is not a failure"
    );
    assert_eq!(view["cancel_requested"], true);
    assert_eq!(view["download"]["current_file"], "rec.onnx");
    assert!(!dir.join("rec.onnx").exists());
    assert!(part_files(&dir).is_empty());
}

/// 排队中的下载取消是**立即**的（§4.3）：worker 不会开始它（`begin_download` 被拒绝）。
#[test]
fn cancelling_a_queued_download_is_immediate() {
    let dir = manifest_model_dir_text_only("download-queued", &["det.onnx"], None);
    let gate = Gate::new();
    let server = TestServer::start(TestOptions {
        allow_download: true,
        downloader: ScriptedDownload::with_gate(Arc::clone(&gate)).factory(),
        ..TestOptions::new(dir, Scripted::fast())
    });

    // 第一个任务占住唯一的下载 worker。
    let first = server.download("test-set").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    gate.arrive();
    // 第二个任务进入有界 channel（排队），随后立刻取消。
    let second = server.download("test-set").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    let cancelled = server.post(&format!("/api/jobs/{second}/cancel"), &[], b"");
    assert_eq!(cancelled.status, 200, "{}", cancelled.text());
    let view = cancelled.json();
    assert_eq!(
        view["state"], "cancelled",
        "a queued job cancels immediately"
    );
    assert_eq!(view["cancel_requested"], false);

    gate.release();
    gate.arrive();
    gate.release();
    let first_view = server.wait_terminal(&first, Duration::from_secs(20));
    assert_eq!(first_view["state"], "succeeded", "{first_view}");

    // 第二个任务从未开始：没有失败分类，也没有任何进度推进。
    let second_view = server.get(&format!("/api/jobs/{second}")).json();
    assert_eq!(second_view["state"], "cancelled");
    assert_eq!(second_view["failure"], serde_json::Value::Null);
    assert_eq!(second_view["download"]["files_done"], 0);
    assert_eq!(
        second_view["download"]["current_file"],
        serde_json::Value::Null
    );
}

/// §4.2：未知 `set_id` → 404（带请求的 id 与已知集合），空/畸形 id → 400；
/// **绝不**回落到 `sets[0]`。
#[test]
fn an_unknown_or_empty_set_id_is_refused_and_never_guessed() {
    let dir = manifest_model_dir("download-unknown", &["det.onnx"], None);
    let scripted = ScriptedDownload::default();
    let server = TestServer::start(TestOptions {
        allow_download: true,
        downloader: scripted.clone().factory(),
        ..TestOptions::new(dir, Scripted::fast())
    });

    let unknown = server.download("nope");
    assert_eq!(unknown.status, 404, "{}", unknown.text());
    assert_eq!(unknown.code(), "model_set_not_found");
    assert_eq!(unknown.json()["detail"]["set_id"], "nope");
    assert_eq!(unknown.json()["detail"]["known_sets"][0], "test-set");

    for bad in [
        &br#"{"set_id":""}"#[..],
        &br#"{}"#[..],
        &br#"{"set_id":null}"#[..],
        &br#"{"set_id":"test-set","url":"https://evil.example/x.onnx"}"#[..],
    ] {
        let response = server.post(
            "/api/models/download",
            &[("Content-Type", "application/json")],
            bad,
        );
        assert_eq!(response.status, 400, "{}", response.text());
    }
    assert!(
        scripted.hosts_per_call().is_empty(),
        "a refused request must not download anything"
    );
}

/// `--allow-download-host` 是**显式参数**：它出现在交给下载器的列表里，
/// 而不是"库常量被改掉了"（常量本身由 `model_store` 的单测逐项锁死）。
///
/// M2b 起编译期常量有两项（`www.modelscope.cn` + 权重 302 的目标 host
/// `cdn-lfs-cn-1.modelscope.cn`），顺序即 `ALLOWED_DOWNLOAD_HOSTS` 的顺序。
#[test]
fn the_download_host_opt_in_is_passed_as_an_explicit_parameter() {
    let dir = manifest_model_dir("download-hosts", &["det.onnx"], None);
    let plain = ScriptedDownload::default();
    let server = TestServer::start(TestOptions {
        allow_download: true,
        downloader: plain.clone().factory(),
        ..TestOptions::new(dir, Scripted::fast())
    });
    let id = server.download("test-set").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    assert_eq!(
        server.wait_terminal(&id, Duration::from_secs(20))["state"],
        "succeeded"
    );
    assert_eq!(
        plain.hosts_per_call(),
        vec![vec![
            "www.modelscope.cn".to_string(),
            "cdn-lfs-cn-1.modelscope.cn".to_string()
        ]]
    );
    // 同一个请求，加上 `--allow-download-host evil.example`：列表被**显式**扩展。
    let dir = manifest_model_dir("download-hosts-optin", &["det.onnx"], None);
    let extended = ScriptedDownload::default();
    let server = TestServer::start(TestOptions {
        allow_download: true,
        allow_download_hosts: vec!["evil.example".to_string()],
        downloader: extended.clone().factory(),
        ..TestOptions::new(dir, Scripted::fast())
    });
    let id = server.download("test-set").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    assert_eq!(
        server.wait_terminal(&id, Duration::from_secs(20))["state"],
        "succeeded"
    );
    assert_eq!(
        extended.hosts_per_call(),
        vec![vec![
            "www.modelscope.cn".to_string(),
            "cdn-lfs-cn-1.modelscope.cn".to_string(),
            "evil.example".to_string()
        ]]
    );
    // `/api/status` 如实给出生效的列表（启动日志里也有同样的几行 + 高风险警告）。
    let status = server.get("/api/status").json();
    assert_eq!(status["download_hosts"][0], "www.modelscope.cn");
    assert_eq!(status["download_hosts"][1], "cdn-lfs-cn-1.modelscope.cn");
    assert_eq!(status["download_hosts"][2], "evil.example");
}

/// 未开 `--allow-download` → 403；`--allow-download` 打开后是**真实**任务（M2 替换了处理体）。
#[test]
fn the_download_endpoint_still_refuses_when_downloads_are_disabled() {
    let dir = manifest_model_dir("download-disabled", &["det.onnx"], None);
    let scripted = ScriptedDownload::default();
    let server = TestServer::start(TestOptions {
        downloader: scripted.clone().factory(),
        ..TestOptions::new(dir, Scripted::fast())
    });
    let response = server.download("test-set");
    assert_eq!(response.status, 403, "{}", response.text());
    assert_eq!(response.code(), "downloads_disabled");
    assert!(scripted.hosts_per_call().is_empty());
}

// ---------------------------------------------------------------- M2：引擎（惰性创建与 reload）

/// §7.6：`POST /api/engine/reload` 是 `models_still_missing` 的生产者——模型仍缺失时
/// 响应体里如实给出 `blocked_models_missing` 与缺失清单，而不是一句模糊的失败。
#[test]
fn the_reload_endpoint_reports_blocked_models_with_the_missing_list() {
    let built = Arc::new(AtomicUsize::new(0));
    let server = TestServer::start(TestOptions {
        engine_factory: counting_engine_factory(Arc::clone(&built)),
        ..TestOptions::new(empty_model_dir("reload-blocked"), Scripted::fast())
    });
    assert_eq!(
        server.get("/api/status").json()["engine"]["state"],
        "blocked_models_missing"
    );

    let reloaded = server.post("/api/engine/reload", &[], b"");
    assert_eq!(reloaded.status, 200, "{}", reloaded.text());
    let body = reloaded.json();
    assert_eq!(body["outcome"], "blocked_models_missing");
    assert_eq!(body["engine"]["state"], "blocked_models_missing");
    let missing = body["missing"].as_array().expect("a missing list");
    assert_eq!(missing.len(), 3, "{body}");
    assert_eq!(
        body["missing"],
        server.get("/api/models").json()["missing"],
        "the same computation as /api/models"
    );
    assert_eq!(body["engine"]["missing"], body["missing"]);
    assert!(body["load_ms"].as_u64().is_some(), "{body}");
    assert_eq!(built.load(Ordering::SeqCst), 0, "no session can be created");
    assert_eq!(server.submit_ocr(b"image").status, 409);
}

/// §7.6：模型齐备时 reload 真的建立会话并给出耗时；显式 reload 会**重建**（M2 的入口，
/// provider 运行期切换（`Rebuilding`）仍属 M3）。
#[test]
fn the_reload_endpoint_creates_the_engine_and_reports_the_elapsed_time() {
    let dir = manifest_model_dir("reload-ready", &["det.onnx", "rec.onnx", "dict.txt"], None);
    let built = Arc::new(AtomicUsize::new(0));
    let server = TestServer::start(TestOptions {
        engine_factory: counting_engine_factory(Arc::clone(&built)),
        ..TestOptions::new(dir, Scripted::fast())
    });
    // 模型齐备 → 启动期预加载（§7.6 第 3 步）。
    assert_eq!(server.get("/api/status").json()["engine"]["state"], "ready");
    assert_eq!(built.load(Ordering::SeqCst), 1);

    let reloaded = server.post("/api/engine/reload", &[], b"");
    assert_eq!(reloaded.status, 200, "{}", reloaded.text());
    let body = reloaded.json();
    assert_eq!(body["outcome"], "ready");
    assert_eq!(body["engine"]["state"], "ready");
    assert_eq!(body["engine"]["selected_ep"], "cpu");
    assert!(body["load_ms"].as_u64().is_some(), "{body}");
    assert_eq!(
        built.load(Ordering::SeqCst),
        2,
        "an explicit reload rebuilds the session"
    );

    let id = server.submit_ocr(b"image").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    assert_eq!(
        server.wait_terminal(&id, Duration::from_secs(20))["state"],
        "succeeded"
    );
}

/// §7.6：模型齐备后**惰性**创建引擎——触发点是下一次 `POST /api/ocr`（或显式 reload），
/// 而不是下载完成的瞬间；`loading` 期间 `/api/status` 如实显示，完成后给出生耗时。
#[test]
fn the_engine_is_created_lazily_on_the_next_ocr_request_and_loading_is_visible() {
    let dir = manifest_model_dir("lazy-engine", &["det.onnx"], None);
    let gate = Gate::new();
    let built = Arc::new(AtomicUsize::new(0));
    let server = TestServer::start(TestOptions {
        engine_factory: gated_engine_factory(Arc::clone(&gate), Arc::clone(&built)),
        ..TestOptions::new(dir.clone(), Scripted::fast())
    });
    assert_eq!(
        server.get("/api/status").json()["engine"]["state"],
        "blocked_models_missing"
    );
    assert_eq!(built.load(Ordering::SeqCst), 0);

    // 缺的两个文件出现在磁盘上（等价于"下载完成"）——此时**仍然没有**引擎。
    for name in ["rec.onnx", "dict.txt"] {
        std::fs::write(dir.join(name), model_file_bytes(name)).expect("write the model file");
    }
    assert_eq!(
        server.get("/api/status").json()["engine"]["state"],
        "blocked_models_missing",
        "a completed download must not create the engine in the background"
    );
    assert_eq!(
        server.get("/api/models").json()["complete"],
        true,
        "the files are complete on disk"
    );
    assert_eq!(built.load(Ordering::SeqCst), 0);

    // 下一次 `POST /api/ocr` 就是创建点（accept 线程只把状态推进 `Loading`）。
    let accepted = server.submit_ocr(b"image");
    assert_eq!(accepted.status, 202, "{}", accepted.text());
    let id = accepted.json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    gate.arrive(); // 会话正在建立
    let status = server.get("/api/status").json();
    assert_eq!(status["engine"]["state"], "loading", "{status}");
    assert_eq!(built.load(Ordering::SeqCst), 1);
    gate.release();

    let view = server.wait_terminal(&id, Duration::from_secs(20));
    assert_eq!(view["state"], "succeeded", "{view}");
    let status = server.get("/api/status").json();
    assert_eq!(status["engine"]["state"], "ready");
    assert!(
        status["engine_load_ms"].as_u64().is_some(),
        "the session creation time must be reported: {status}"
    );
}

/// 评审 P2-2：**无 body** 的 `POST /api/engine/reload` 也必须在独立线程上建立会话。
///
/// 旧实现在 accept 线程上直接调 `reload_engine(None)`：ONNX Runtime 建会话的那几百毫秒
/// 到几秒里，`/api/status` 与 `POST /api/ocr` 全都停摆。这里用闸门把建会话按在中间，
/// 断言两件事在**reload 还没完成时**就成立：`loading` 可见、新任务照常 202。
#[test]
fn a_bodyless_reload_keeps_the_accept_loop_live() {
    let dir = manifest_model_dir(
        "m2-bodyless-reload",
        &["det.onnx", "rec.onnx", "dict.txt"],
        None,
    );
    let gate = Gate::new();
    let plan = SessionPlan {
        // 第 2 次建会话（= reload）阻塞在闸门上。
        gate_at: Some(2),
        ..SessionPlan::new()
    };
    let server = TestServer::start(TestOptions {
        engine_factory: plan.factory(Some(Arc::clone(&gate))),
        ..TestOptions::new(dir, Scripted::fast())
    });
    assert_eq!(server.get("/api/status").json()["engine"]["state"], "ready");

    let addr = server.addr;
    let host = server.host.clone();
    let origin = server.origin.clone();
    let token = server.token.clone();
    let reload = std::thread::spawn(move || {
        raw_request(
            addr,
            "POST",
            "/api/engine/reload",
            &[
                ("Host", host.as_str()),
                ("X-RapidOCR-Token", token.as_str()),
                ("Origin", origin.as_str()),
                ("Content-Length", "0"),
            ],
            Some(b""),
        )
    });
    // 断言失败也一定放行，否则这个测试会从"一条红"变成"挂死整个测试进程"。
    let release = ReleaseOnDrop::new(Arc::clone(&gate));

    gate.arrive(); // 第二次建会话正在运行
    assert!(
        wait_until(Duration::from_secs(10), || {
            server.get("/api/status").json()["engine"]["state"] == "loading"
        }),
        "the bodyless reload must be observable through /api/status while it blocks"
    );
    // accept 线程仍然活着：提交一个新任务（`Loading` 期间入队，不是拒绝）。
    let queued = server.submit_ocr(b"during-bodyless-reload");
    assert_eq!(queued.status, 202, "{}", queued.text());
    assert_eq!(queued.json()["state"], "queued");

    release.release();
    drop(release);
    let response = reload.join().expect("the reload thread must finish");
    assert_eq!(response.status, 200, "{}", response.text());
    let body = response.json();
    assert_eq!(body["outcome"], "ready", "{body}");
    assert!(body["load_ms"].as_u64().is_some(), "{body}");
    assert_eq!(plan.calls(), 2, "an explicit reload rebuilds the session");

    let status = server.get("/api/status").json();
    assert_eq!(status["engine"]["state"], "ready", "{status}");
}

// ---------------------------------------------------------------- M3：标注图与导出

/// 一张真实的 PNG（`scripted_output` 的 `image.original_size` 是 100×50，两者必须一致：
/// 标注图是画在**被解码的原图**上的，尺寸对不上就说明这条链路有假）。
fn flat_source_png() -> Vec<u8> {
    let image = image::RgbImage::from_fn(100, 50, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 40])
    });
    let mut bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .expect("the fixture PNG encodes");
    bytes
}

/// 一张"压不动"的 PNG（伪随机像素）：体积可控，用于字节预算测试。
fn noise_png(dimension: u32) -> Vec<u8> {
    let mut state = 0x1234_5678_9abc_def0_u64;
    let image = image::RgbImage::from_fn(dimension, dimension, |_, _| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        image::Rgb([
            (state >> 33) as u8,
            (state >> 41) as u8,
            (state >> 49) as u8,
        ])
    });
    let mut bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .expect("the noise PNG encodes");
    bytes
}

/// 至少 `minimum` 字节的 PNG（选最小的那个够用的尺寸，因此不会超出保留预算太多）。
fn source_png_at_least(minimum: usize) -> Vec<u8> {
    for dimension in [360_u32, 420, 480, 520, 560] {
        let png = noise_png(dimension);
        if png.len() >= minimum {
            return png;
        }
    }
    panic!("cannot build a PNG of at least {minimum} bytes");
}

/// §4.2/§4.5：`/annotated.png` 是**真正**的 PNG、尺寸等于原图、且检测框真的画上去了。
#[test]
fn the_annotated_png_is_a_real_png_with_the_original_dimensions_and_drawn_boxes() {
    let server = TestServer::start(TestOptions::new(
        complete_model_dir("m3-annotated"),
        Scripted {
            // 一个区域：画布上只有第一个调色板颜色（纯红），断言因此是精确的。
            regions: 1,
            ..Scripted::fast()
        },
    ));
    let source = flat_source_png();
    let accepted = server.submit_ocr(&source).json();
    let id = accepted["job_id"].as_str().expect("job id").to_string();
    let view = server.wait_terminal(&id, Duration::from_secs(20));
    assert_eq!(view["state"], "succeeded", "{view}");
    assert_eq!(
        view["original_retained"], true,
        "the encoded original stays in the retention area after OCR (§4.5): {view}"
    );

    let result = server.get(&format!("/api/jobs/{id}/result")).json();
    let expected = (
        result["image"]["original_size"]["width"].as_u64(),
        result["image"]["original_size"]["height"].as_u64(),
    );
    assert_eq!(expected, (Some(100), Some(50)), "{result}");

    let response = server.get(&format!("/api/jobs/{id}/annotated.png"));
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.header("Content-Type"), Some("image/png"));
    // 三个安全头对**每个**响应都成立（§7.3）。
    assert_eq!(response.header("X-Content-Type-Options"), Some("nosniff"));
    assert_eq!(response.header("Cache-Control"), Some("no-store"));
    assert_eq!(&response.body[..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
    let decoded = image::load_from_memory(&response.body).expect("the annotation is a real image");
    assert_eq!(
        (decoded.width() as u64, decoded.height() as u64),
        (expected.0.unwrap(), expected.1.unwrap()),
        "the canvas is the decoded original, not a guess"
    );
    // 第一个区域的框角 (1,1) 被画成了调色板的第 0 种颜色（纯红）——证明 `draw_output` 真的跑了。
    assert_eq!(
        decoded.to_rgb8().get_pixel(1, 1).0,
        [255, 0, 0],
        "the detection box must be drawn on the retained original"
    );
}

/// §4.2/§4.5：原图编码字节被保留预算释放后，`/annotated.png` 是 **410 `original_evicted`**，
/// 而**结果仍然可读**（这正是这个 `code` 与 `job_evicted` 的区别）。
///
/// 让一个任务自己的"原图 + 结果"超过 1 MiB 预算即可确定性触发：预算超限时先释放**最旧终态
/// 任务的原图**（此刻就是它自己），任务与结果都留下。
#[test]
fn the_annotated_png_is_410_original_evicted_once_the_budget_releases_the_original() {
    let mut raw = limits(4, 2);
    raw.max_retained_mb = 1;
    raw.max_result_mb = 8;
    let scripted = || Scripted {
        regions: 1,
        text_bytes: 128 * 1024,
        ..Scripted::fast()
    };

    // 先在一个**同样的**运行时上量出结果 JSON 的真实大小（同一份脚本化输出，确定性）。
    let probe = TestServer::start(TestOptions {
        limits: raw,
        ..TestOptions::new(complete_model_dir("m3-annotated-evicted-probe"), scripted())
    });
    let accepted = probe.submit_ocr(&flat_source_png()).json();
    let probe_id = accepted["job_id"].as_str().expect("job id").to_string();
    assert_eq!(
        probe.wait_terminal(&probe_id, Duration::from_secs(30))["state"],
        "succeeded"
    );
    let result_bytes = probe
        .get(&format!("/api/jobs/{probe_id}/result"))
        .body
        .len() as u64;
    assert!(
        result_bytes < 1 << 20,
        "the result store's own budget is 1 MiB, so the fixture result must fit: {result_bytes}"
    );

    // 原图必须"一个装得下、加上结果就装不下"：条件不成立时下面会直接失败，
    // 而不是悄悄退化成"其实没触发预算"。
    let needed = (1 << 20) - result_bytes + 64 * 1024;
    let source = source_png_at_least(needed as usize);
    assert!(
        source.len() as u64 <= 1 << 20,
        "the original alone must fit the 1 MiB budget: {} bytes",
        source.len()
    );
    assert!(
        source.len() as u64 + result_bytes > 1 << 20,
        "the original plus the result must exceed it: {} + {result_bytes}",
        source.len()
    );

    let server = TestServer::start(TestOptions {
        limits: raw,
        ..TestOptions::new(complete_model_dir("m3-annotated-evicted"), scripted())
    });
    let accepted = server.submit_ocr(&source).json();
    let id = accepted["job_id"].as_str().expect("job id").to_string();
    let view = server.wait_terminal(&id, Duration::from_secs(30));
    assert_eq!(view["state"], "succeeded", "{view}");
    assert_eq!(
        view["original_retained"], false,
        "the byte budget released the original: {view}"
    );

    let response = server.get(&format!("/api/jobs/{id}/annotated.png"));
    assert_eq!(response.status, 410, "{}", response.text());
    assert_eq!(response.code(), "original_evicted");
    assert!(
        response
            .json()
            .get("detail")
            .is_some_and(serde_json::Value::is_null),
        "the body still carries the frozen three keys: {}",
        response.text()
    );

    // 任务与结果**都还在**（与 `job_evicted` 的区别就在这里）。
    assert_eq!(server.get(&format!("/api/jobs/{id}")).status, 200);
    let result = server.get(&format!("/api/jobs/{id}/result"));
    assert_eq!(result.status, 200, "{}", result.text());
    assert_eq!(result.body.len() as u64, result_bytes, "byte for byte");
    // 状态里也如实反映"保留区里还有几份原图"。
    let status = server.get("/api/status").json();
    assert_eq!(status["retention"]["retained_originals"], 0, "{status}");
    assert!(
        status["retention"]["retained_bytes"].as_u64() == Some(result_bytes),
        "the byte ledger follows the release: {status}"
    );
}

/// §4.2/§9.5：三种导出都由**库的渲染器**产生，HTML 是静态的（正文无 `<script`），
/// 图片以 `data:` 内嵌，且以附件形式返回并带独立的导出 CSP。
#[test]
fn every_export_format_is_served_as_an_attachment_and_the_html_is_static_and_offline() {
    let server = TestServer::start(TestOptions::new(
        complete_model_dir("m3-export"),
        Scripted {
            regions: 2,
            ..Scripted::fast()
        },
    ));
    let accepted = server.submit_ocr(&flat_source_png()).json();
    let id = accepted["job_id"].as_str().expect("job id").to_string();
    assert_eq!(
        server.wait_terminal(&id, Duration::from_secs(20))["state"],
        "succeeded"
    );

    // (a) JSON 导出与 `/result` 逐字节相同（同一份值、同一个有界写入器）。
    let result = server.get(&format!("/api/jobs/{id}/result"));
    let json = server.get(&format!("/api/jobs/{id}/export?format=json"));
    assert_eq!(json.status, 200, "{}", json.text());
    assert_eq!(
        json.header("Content-Type"),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(
        json.header("Content-Disposition"),
        Some(format!("attachment; filename=\"ocr-{id}.json\"").as_str())
    );
    assert_eq!(json.body, result.body, "the same bytes as /result");
    assert_eq!(json.header("Content-Security-Policy"), None);

    // (b) Markdown 导出。
    let markdown = server.get(&format!("/api/jobs/{id}/export?format=md"));
    assert_eq!(markdown.status, 200, "{}", markdown.text());
    assert_eq!(
        markdown.header("Content-Type"),
        Some("text/markdown; charset=utf-8")
    );
    assert_eq!(
        markdown.header("Content-Disposition"),
        Some(format!("attachment; filename=\"ocr-{id}.md\"").as_str())
    );
    let markdown_text = markdown.text();
    assert!(markdown_text.contains("region-0"), "{markdown_text}");

    // (c) HTML 导出：静态模式 + data: 内嵌图片 + 导出 CSP + 附件头。
    let html = server.get(&format!("/api/jobs/{id}/export?format=html"));
    assert_eq!(html.status, 200, "{}", html.text());
    assert_eq!(
        html.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        html.header("Content-Disposition"),
        Some(format!("attachment; filename=\"ocr-{id}.html\"").as_str())
    );
    assert_eq!(
        html.header("Content-Security-Policy"),
        Some(super::http::EXPORT_CSP)
    );
    let html_text = html.text();
    assert!(
        !html_text.to_lowercase().contains("<script"),
        "the export must not contain any <script: {html_text}"
    );
    assert!(
        html_text.contains("data:image/png;base64,"),
        "the annotated image must be embedded, not linked: {html_text}"
    );
    // 真正离线可用：唯一的 `src` 就是那条 data URL，没有相对路径、没有指向本服务的链接。
    assert_eq!(
        html_text.matches("src=\"").count(),
        1,
        "exactly one image source: {html_text}"
    );
    assert!(
        html_text.contains("src=\"data:image/png;base64,"),
        "the image must be embedded: {html_text}"
    );
    assert!(!html_text.contains("/api/"), "no service link: {html_text}");
    assert!(html_text.contains("<style>"), "{html_text}");
    // 主页面 CSP 不含 unsafe-inline；导出 CSP 只出现在导出这条响应上。
    let page = server.get("/");
    assert!(
        !page
            .header("Content-Security-Policy")
            .unwrap_or_default()
            .contains("unsafe-inline")
    );

    // 未知/缺失的 format 是 400（不猜默认值）。
    for bad in ["", "?format=pdf", "?format=JSON"] {
        let response = server.get(&format!("/api/jobs/{id}/export{bad}"));
        assert_eq!(response.status, 400, "{bad}: {}", response.text());
        assert_eq!(response.code(), "bad_request", "{bad}");
    }
}

/// §4.6/§9.5：`--max-export-mb` 用有界写入器强制，超限是 **413 `export_too_large`**，
/// `detail` 指向**仍然可用**的 `annotated.png`；刚好在限额内的导出是 200。
#[test]
fn an_export_over_the_limit_is_a_413_pointing_at_the_annotated_png() {
    let mut raw = limits(4, 2);
    // 结果预算（8 MiB）比导出预算（1 MiB）大：任务会成功，导出才会超限。
    raw.max_result_mb = 8;
    raw.max_export_mb = 1;
    let server = TestServer::start(TestOptions {
        limits: raw,
        ..TestOptions::new(
            complete_model_dir("m3-export-too-large"),
            Scripted {
                regions: 1,
                // Markdown 只包含**一份**文本（JSON 会重复若干份），因此这个尺寸要让
                // 最"瘦"的那份文档也超过 1 MiB 的导出预算。
                text_bytes: 1400 * 1024,
                ..Scripted::fast()
            },
        )
    });
    let accepted = server.submit_ocr(&flat_source_png()).json();
    let id = accepted["job_id"].as_str().expect("job id").to_string();
    assert_eq!(
        server.wait_terminal(&id, Duration::from_secs(30))["state"],
        "succeeded"
    );
    let result_bytes = server.get(&format!("/api/jobs/{id}/result")).body.len() as u64;
    assert!(
        result_bytes > 1 << 20,
        "the fixture must be over the export budget: {result_bytes}"
    );
    assert!(
        result_bytes < 8 << 20,
        "the fixture must stay inside --max-result-mb: {result_bytes}"
    );

    for format in ["json", "md", "html"] {
        let response = server.get(&format!("/api/jobs/{id}/export?format={format}"));
        assert_eq!(
            response.status,
            413,
            "{format}: HTTP {} ({} bytes)",
            response.status,
            response.body.len()
        );
        assert_eq!(response.code(), "export_too_large", "{format}");
        let body = response.json();
        assert_eq!(body["detail"]["limit_bytes"], 1 << 20, "{format}: {body}");
        assert_eq!(
            body["detail"]["annotated"],
            format!("/api/jobs/{id}/annotated.png"),
            "{format}: {body}"
        );
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|message| message.contains("annotated.png")),
            "{format}: {body}"
        );
        if format == "json" {
            // 有界写入器中止时不知道完整长度，而那个长度是 worker **已经测过**的事实。
            assert_eq!(body["detail"]["observed_bytes"], result_bytes, "{body}");
        } else {
            assert!(
                body["detail"]["observed_bytes"]
                    .as_u64()
                    .is_some_and(|n| n > 1 << 20),
                "{format}: {body}"
            );
        }
    }
    // 即使导出被拒，`annotated.png` 本身照旧可用（§9.5 要求的"指向"必须是真的）。
    let annotated = server.get(&format!("/api/jobs/{id}/annotated.png"));
    assert_eq!(annotated.status, 200, "{}", annotated.text());
    assert_eq!(&annotated.body[..8], b"\x89PNG\r\n\x1a\n");
}

/// 刚好在 `--max-export-mb` 之内的导出是 200（上限是"不得超过"，不是"必须小于"）。
#[test]
fn an_export_under_the_limit_is_served_intact() {
    let mut raw = limits(4, 2);
    raw.max_export_mb = 1;
    let server = TestServer::start(TestOptions {
        limits: raw,
        ..TestOptions::new(
            complete_model_dir("m3-export-under"),
            Scripted {
                regions: 1,
                text_bytes: 64 * 1024,
                ..Scripted::fast()
            },
        )
    });
    let accepted = server.submit_ocr(&flat_source_png()).json();
    let id = accepted["job_id"].as_str().expect("job id").to_string();
    assert_eq!(
        server.wait_terminal(&id, Duration::from_secs(20))["state"],
        "succeeded"
    );
    for format in ["json", "md", "html"] {
        let response = server.get(&format!("/api/jobs/{id}/export?format={format}"));
        assert_eq!(response.status, 200, "{format}: {}", response.text());
        assert!(!response.body.is_empty(), "{format}");
    }
}

/// §4.2/§4.5：导出与标注只需要**成功的**任务；其它状态是 409 `job_not_finished`
/// （没有区域可导出/叠加，不能凭空造一份）。
#[test]
fn exports_require_a_successful_job() {
    let server = TestServer::start(TestOptions {
        limits: limits(2, 1),
        ..TestOptions::new(
            complete_model_dir("m3-export-not-finished"),
            Scripted::slow(Duration::from_millis(500)),
        )
    });
    let accepted = server.submit_ocr(b"image").json();
    let id = accepted["job_id"].as_str().expect("job id").to_string();
    for path in [
        format!("/api/jobs/{id}/annotated.png"),
        format!("/api/jobs/{id}/export?format=json"),
        format!("/api/jobs/{id}/export?format=html"),
    ] {
        let response = server.get(&path);
        assert!(
            response.status == 409 || response.status == 200,
            "{path}: {}",
            response.text()
        );
    }

    // 失败的 OCR 任务：三种导出与标注图都是 409（`/result` 才是重放错误的地方）。
    let failing = TestServer::start(TestOptions::new(
        complete_model_dir("m3-export-failed"),
        Scripted {
            fail: true,
            ..Scripted::fast()
        },
    ));
    let accepted = failing.submit_ocr(b"image").json();
    let id = accepted["job_id"].as_str().expect("job id").to_string();
    assert_eq!(
        failing.wait_terminal(&id, Duration::from_secs(20))["state"],
        "failed"
    );
    for path in [
        format!("/api/jobs/{id}/annotated.png"),
        format!("/api/jobs/{id}/export?format=md"),
    ] {
        let response = failing.get(&path);
        assert_eq!(response.status, 409, "{path}: {}", response.text());
        assert_eq!(response.code(), "job_not_finished", "{path}");
    }
}

/// §10.6/§11 M3：诊断面板要的数据全部来自**库已经报告的**东西，不重新测量。
///
/// `/result` 给出逐阶段 `timings` 与时间账本（含残差与自解释文案）；`/api/status` 给出
/// ORT 指纹（已脱敏）、峰值工作集与 provider 三字段。
#[test]
fn the_diagnostics_payload_reports_the_library_ledger_fingerprint_and_memory() {
    let server = TestServer::start(TestOptions::new(
        complete_model_dir("m3-diagnostics"),
        Scripted::fast(),
    ));
    let accepted = server.submit_ocr(&flat_source_png()).json();
    let id = accepted["job_id"].as_str().expect("job id").to_string();
    assert_eq!(
        server.wait_terminal(&id, Duration::from_secs(20))["state"],
        "succeeded"
    );

    let result = server.get(&format!("/api/jobs/{id}/result")).json();
    let timings = result["timings"].as_object().expect("the stage timings");
    for key in [
        "total_ms",
        "preprocess_ms",
        "detector_infer_ms",
        "recognizer_infer_ms",
        "postprocess_ms",
    ] {
        assert!(timings.get(key).is_some(), "missing {key}: {result}");
    }
    let ledger = &result["timing_ledger"];
    assert!(ledger["total_ms"].is_number(), "{ledger}");
    assert!(ledger["input_preprocess_ms"].is_number(), "{ledger}");
    assert!(ledger["unattributed_ms"].is_number(), "{ledger}");
    assert!(ledger["attributed_ms"].is_number(), "{ledger}");
    assert!(ledger["inference_ms"].is_number(), "{ledger}");
    assert!(ledger["rust_ms"].is_number(), "{ledger}");
    assert_eq!(ledger["shares"]["inference_share"], 0.0, "{ledger}");
    let conservation = &ledger["conservation"];
    // `scripted_output` 的 `total_ms = 1.0`、所有分量 0 → 残差 −1.0（可精确断言）。
    assert_eq!(conservation["residual_ms"], -1.0, "{conservation}");
    assert_eq!(conservation["conserved"], false, "{conservation}");
    assert_eq!(conservation["excess_ms"], 1.0, "{conservation}");
    let interpretation = conservation["interpretation"]
        .as_str()
        .expect("the ledger explains itself");
    assert!(
        interpretation.contains("NOT a strict partition"),
        "the panel must be able to show this wording verbatim: {interpretation}"
    );
    assert!(
        interpretation.contains("residual_ms = -1.000000"),
        "the interpretation quotes the actual residual: {interpretation}"
    );
    assert!(
        interpretation.contains("SCOPE difference"),
        "and says what the residual is: {interpretation}"
    );

    let status = server.get("/api/status").json();
    assert!(status["ort"]["version"].is_string(), "{status}");
    assert_eq!(status["ort"]["fingerprint"]["complete"], true, "{status}");
    assert!(
        status["ort"]["fingerprint"]["file"].is_string(),
        "the fingerprint gives a file name, never a path: {status}"
    );
    assert!(
        status["memory"]["peak_working_set_bytes"]
            .as_u64()
            .is_some(),
        "{status}"
    );
    assert!(status["memory"]["source"].is_string(), "{status}");
    assert_eq!(status["provider"]["requested"], "cpu");
    assert_eq!(status["provider"]["selected_ep"], "cpu");
    assert_eq!(status["provider"]["fallback_to_cpu"], false);
    assert!(status["queues"]["text"]["wait_bound"].as_u64().is_some());
}

// ------------------------------------------------- M3：provider 运行期切换（§7.5/§7.6）

/// M3 的完整序列：暂停新任务（`rebuilding`）→ 排空 → 销毁旧 engine → 建立新 engine，
/// 期间到达的 `POST /api/ocr` **入队（202）而不是被拒绝**，切换完成后它们用**新**引擎执行。
#[test]
fn switching_the_provider_rebuilds_the_session_and_queues_requests_meanwhile() {
    let dir = manifest_model_dir(
        "m3-provider-switch",
        &["det.onnx", "rec.onnx", "dict.txt"],
        None,
    );
    let gate = Gate::new();
    let plan = SessionPlan {
        gate_at: Some(2),
        // 排空要有对象：这个会话每次识别花 300 ms，切换因此必须等它跑完。
        delay: Duration::from_millis(300),
        ..SessionPlan::new()
    };
    let server = TestServer::start(TestOptions {
        engine_factory: plan.factory(Some(Arc::clone(&gate))),
        ..TestOptions::new(dir, Scripted::slow(Duration::from_millis(300)))
    });

    // 启动期预加载：会话 1。
    let status = server.get("/api/status").json();
    assert_eq!(status["engine"]["state"], "ready", "{status}");
    assert_eq!(status["provider"]["selected_ep"], "scripted-1", "{status}");
    assert_eq!(plan.calls(), 1);

    // 一个长任务正在推理（排空的对象）。
    let running_id = server.submit_ocr(b"image").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    assert!(
        wait_until(Duration::from_secs(10), || {
            server.get(&format!("/api/jobs/{running_id}")).json()["state"] == "running"
        }),
        "the first job must be running when the switch starts"
    );

    // 切换请求在后台线程里发出：它**只有**在序列结束（或失败）后才返回。
    let addr = server.addr;
    let host = server.host.clone();
    let origin = server.origin.clone();
    let token = server.token.clone();
    let switch = std::thread::spawn(move || {
        raw_request(
            addr,
            "POST",
            "/api/engine/reload",
            &[
                ("Host", host.as_str()),
                ("X-RapidOCR-Token", token.as_str()),
                ("Origin", origin.as_str()),
                ("Content-Type", "application/json"),
            ],
            Some(br#"{"provider":"cpu"}"#),
        )
    });
    // 断言失败也一定放行，否则这个测试会从"一条红"变成"挂死整个测试进程"。
    let release = ReleaseOnDrop::new(Arc::clone(&gate));

    gate.arrive(); // 新会话正在建立
    assert!(
        wait_until(Duration::from_secs(10), || {
            server.get("/api/status").json()["engine"]["state"] == "rebuilding"
        }),
        "`Rebuilding` must be observable through /api/status"
    );
    // 排空已经完成：在跑的任务结束了（它用会话 1 执行）。
    assert_eq!(
        server.get(&format!("/api/jobs/{running_id}")).json()["state"],
        "succeeded"
    );

    // 切换期间：三字段里 requested 已经是新值，selected_ep/fallback 是 null（未知态不装 false）。
    let status = server.get("/api/status").json();
    assert_eq!(status["engine"]["state"], "rebuilding", "{status}");
    assert_eq!(status["provider"]["requested"], "cpu", "{status}");
    assert_eq!(
        status["provider"]["selected_ep"],
        serde_json::Value::Null,
        "{status}"
    );
    assert_eq!(
        status["provider"]["fallback_to_cpu"],
        serde_json::Value::Null,
        "{status}"
    );

    // 切换期间的 OCR 请求：**202 queued**（`OcrAdmission::Queue`），不是 503/409。
    let queued = server.submit_ocr(b"during-rebuild");
    assert_eq!(queued.status, 202, "{}", queued.text());
    assert_eq!(queued.json()["state"], "queued");
    let queued_id = queued.json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();

    release.release();
    drop(release);
    let response = switch.join().expect("the switch thread must finish");
    assert_eq!(response.status, 200, "{}", response.text());
    let body = response.json();
    assert_eq!(body["outcome"], "ready", "{body}");
    assert_eq!(body["engine"]["selected_ep"], "scripted-2", "{body}");
    assert_eq!(body["selected_ep"], "scripted-2", "{body}");
    assert_eq!(body["fallback_to_cpu"], false, "{body}");
    assert_eq!(body["error"], serde_json::Value::Null, "{body}");
    assert_eq!(
        plan.calls(),
        2,
        "the switch destroys the old session and builds one new"
    );

    // 队列里的任务用**新**引擎跑完（`served` 记录了每次识别是谁服务的）。
    let view = server.wait_terminal(&queued_id, Duration::from_secs(20));
    assert_eq!(view["state"], "succeeded", "{view}");
    let status = server.get("/api/status").json();
    assert_eq!(status["engine"]["state"], "ready", "{status}");
    assert_eq!(status["provider"]["selected_ep"], "scripted-2", "{status}");
    assert!(
        status["engine_load_ms"].as_u64().is_some(),
        "the new session's creation time is reported: {status}"
    );
}

/// 新 provider 建不起来时**恢复旧引擎**：状态回到 `Ready`（旧会话在线），
/// 响应如实给出 `outcome = "rolled_back"` 与失败原因。
#[test]
fn a_failed_switch_rolls_back_to_the_previous_engine() {
    let dir = manifest_model_dir(
        "m3-provider-rollback",
        &["det.onnx", "rec.onnx", "dict.txt"],
        None,
    );
    let plan = SessionPlan {
        fail_at: vec![2],
        ..SessionPlan::new()
    };
    let server = TestServer::start(TestOptions {
        engine_factory: plan.factory(None),
        ..TestOptions::new(dir, Scripted::fast())
    });
    assert_eq!(
        server.get("/api/status").json()["provider"]["selected_ep"],
        "scripted-1"
    );

    let response = server.post(
        "/api/engine/reload",
        &[("Content-Type", "application/json")],
        br#"{"provider":"cpu"}"#,
    );
    assert_eq!(response.status, 200, "{}", response.text());
    let body = response.json();
    assert_eq!(body["outcome"], "rolled_back", "{body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|error| error.contains("scripted session 2")),
        "{body}"
    );
    assert!(body["rollback_ms"].as_u64().is_some(), "{body}");
    // 生效的是恢复出来的旧会话（第三个会话），`/api/status` 与响应一致。
    assert_eq!(body["engine"]["state"], "ready", "{body}");
    assert_eq!(body["engine"]["selected_ep"], "scripted-3", "{body}");
    assert_eq!(
        plan.calls(),
        3,
        "one failed attempt + one successful restore"
    );

    let status = server.get("/api/status").json();
    assert_eq!(status["engine"]["state"], "ready", "{status}");
    assert_eq!(status["provider"]["selected_ep"], "scripted-3", "{status}");
    assert_eq!(status["provider"]["requested"], "cpu", "{status}");
    // 服务仍然可用：一次识别照样成功。
    let id = server.submit_ocr(b"image").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    assert_eq!(
        server.wait_terminal(&id, Duration::from_secs(20))["state"],
        "succeeded"
    );
}

/// 连旧引擎也恢复不了 → 明确 `failed`（原因里两个都写），OCR 随后是 503 `engine_unavailable`。
#[test]
fn a_switch_that_cannot_restore_the_old_engine_lands_in_failed() {
    let dir = manifest_model_dir(
        "m3-provider-dead",
        &["det.onnx", "rec.onnx", "dict.txt"],
        None,
    );
    let plan = SessionPlan {
        fail_at: vec![2, 3],
        ..SessionPlan::new()
    };
    let server = TestServer::start(TestOptions {
        engine_factory: plan.factory(None),
        ..TestOptions::new(dir, Scripted::fast())
    });

    let response = server.post(
        "/api/engine/reload",
        &[("Content-Type", "application/json")],
        br#"{"provider":"cpu"}"#,
    );
    assert_eq!(response.status, 200, "{}", response.text());
    let body = response.json();
    assert_eq!(body["outcome"], "failed", "{body}");
    assert_eq!(body["engine"]["state"], "failed", "{body}");
    let reason = body["engine"]["reason"].as_str().expect("a reason");
    assert!(reason.contains("scripted session 2"), "{reason}");
    assert!(reason.contains("restoring the previous engine"), "{reason}");
    assert!(reason.contains("scripted session 3"), "{reason}");

    // `/api/status` 必须显示同一个原因（不得退化成"模型不可用"这种模糊状态）。
    let status = server.get("/api/status").json();
    assert_eq!(status["engine"]["state"], "failed", "{status}");
    assert_eq!(status["engine"]["reason"], reason, "{status}");
    assert_eq!(status["provider"]["requested"], "cpu", "{status}");
    assert_eq!(status["provider"]["selected_ep"], serde_json::Value::Null);

    // 引擎不可用：OCR 是 503 `engine_unavailable` + reason（不是 500、也不是假装成功）。
    let response = server.submit_ocr(b"image");
    assert_eq!(response.status, 503, "{}", response.text());
    assert_eq!(response.code(), "engine_unavailable");
    assert_eq!(plan.calls(), 3);
}

/// §7.5：请求一个本构建里没编译进来的 provider 是**可定位的客户端错误**（400 + 库侧原文），
/// 且**任何状态都不动**（不销毁旧引擎、不进入 `Rebuilding`）。
///
/// 若本构建真的编译了该 provider（`--features directml-provider`），这个请求是合法的，
/// 测试转而断言"切换确实发生了"——两个分支都断言真实行为，没有"跳过"。
#[test]
fn requesting_a_provider_that_is_not_compiled_in_is_a_400_and_changes_nothing() {
    let dir = manifest_model_dir(
        "m3-provider-invalid",
        &["det.onnx", "rec.onnx", "dict.txt"],
        None,
    );
    let plan = SessionPlan::new();
    let server = TestServer::start(TestOptions {
        engine_factory: plan.factory(None),
        ..TestOptions::new(dir, Scripted::fast())
    });
    assert_eq!(
        server.get("/api/status").json()["provider"]["selected_ep"],
        "scripted-1"
    );

    let response = server.post(
        "/api/engine/reload",
        &[("Content-Type", "application/json")],
        br#"{"provider":"directml"}"#,
    );
    if cfg!(feature = "directml-provider") {
        assert_eq!(response.status, 200, "{}", response.text());
        assert_eq!(response.json()["engine"]["state"], "ready");
        assert_eq!(plan.calls(), 2, "this build can switch to DirectML");
        return;
    }

    assert_eq!(response.status, 400, "{}", response.text());
    assert_eq!(response.code(), "bad_request");
    let body = response.json();
    // 标签来自库的 `format_provider_preference`（含 `device_id`），serve 不另写措辞。
    assert!(
        body["detail"]["provider"]
            .as_str()
            .is_some_and(|provider| provider.starts_with("directml")),
        "{body}"
    );
    assert!(
        body["detail"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("directml-provider")),
        "the library's own wording must survive: {body}"
    );
    // 状态一字未动：还是会话 1，工厂没有被再调用一次。
    assert_eq!(
        plan.calls(),
        1,
        "a rejected request must not touch the engine"
    );
    let status = server.get("/api/status").json();
    assert_eq!(status["engine"]["state"], "ready", "{status}");
    assert_eq!(status["provider"]["selected_ep"], "scripted-1", "{status}");
    assert_eq!(status["provider"]["requested"], "cpu", "{status}");
    // 服务照常可用。
    let id = server.submit_ocr(b"image").json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    assert_eq!(
        server.wait_terminal(&id, Duration::from_secs(20))["state"],
        "succeeded"
    );
}

// ---------------------------------------------------------------- M2：真实网络（opt-in）

/// opt-in 真实网络测试的门禁：只有 `RAPID_OCR_ALLOW_NETWORK=1` 时才真的联网。
///
/// 这不是"把测试弱化"，而是**网络测试的门禁**：绿/红只在显式要求时才成立，
/// 默认运行时打印一行 `skipping`（CI 与本机都可能没有网络）。
fn network_tests_enabled() -> bool {
    if std::env::var("RAPID_OCR_ALLOW_NETWORK").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("skipping the real-network test: set RAPID_OCR_ALLOW_NETWORK=1 to run it");
    false
}

/// 真实网络测试的模型目录：本地 manifest，**一个**待下载文件是默认表里真实存在的条目
/// （URL、SHA-256、体积逐字抄自 `assets/default_models.yaml`），另外两个 role 由
/// **已经就位**的夹具文件满足。
///
/// 为什么这样安排：
///
/// - `ModelPlan::resolve` 要求文本管线的三个 role 都在集合里声明，而这份夹具只需要验证
///   **下载路径**（真实 URL + 强制 SHA-256 + 原子落盘），因此另外两个 role 用已经
///   `Present` 的小夹具文件满足——`download_model_set` 对 `Present` 的文件不发请求、
///   也不做 host 检查，于是这次真实网络往返只有**一次**；
/// - `remote` 由调用方给出：字典（直连 200）与 ONNX 权重（302 → CDN）各有一条用例，
///   后者正是 M2b 要打通的那条路径。
fn network_manifest_dir(name: &str, remote: &RemoteDefaultFile) -> PathBuf {
    let dir = m2_root().join(format!("network-manifest-{name}-{}", unique()));
    // **每次都从空目录开始**：`unique()` 是进程内的计数器，重启进程后会得到同名目录，
    // 而"上一个进程下载好的文件还在"会让 `missing` 断言失败（它不是被测行为）。
    // 清掉旧目录，让这条测试可重复运行。
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear the previous run's model dir");
    }
    std::fs::create_dir_all(&dir).expect("create the model dir");
    // 两个已就位的夹具文件（内容由测试决定，哈希真实计算）。
    let fixtures = [
        ("network_det_fixture.onnx", "detector"),
        ("network_rec_fixture.onnx", "recognizer"),
    ];
    let mut entries: Vec<(String, String, String, u64)> = Vec::new();
    for (file, role) in fixtures {
        let path = dir.join(file);
        std::fs::write(&path, format!("present fixture {file}\n")).expect("write the fixture");
        let size = std::fs::metadata(&path).expect("metadata").len();
        entries.push((
            file.to_string(),
            role.to_string(),
            format!("https://www.modelscope.cn/models/{file}"),
            size,
        ));
    }
    // 真实待下载的那个默认表条目（字典或 ONNX 权重）。
    entries.push((
        remote.file.to_string(),
        remote.role.to_string(),
        remote.url.to_string(),
        remote.size_bytes,
    ));
    let real_hashes = ["", "", remote.sha256];

    let mut manifest = String::from(
        "{\"schema_version\":1,\"id\":\"network-set\",\"family\":\"PP-OCR\",\"version\":\"v-real\",\
         \"languages\":[\"en\"],\"files\":[",
    );
    for (index, (file, role, url, size)) in entries.iter().enumerate() {
        let sha = if index < real_hashes.len() && !real_hashes[index].is_empty() {
            real_hashes[index].to_string()
        } else {
            sha256_file(dir.join(file)).expect("hash the fixture")
        };
        if index > 0 {
            manifest.push(',');
        }
        manifest.push_str(&format!(
            "{{\"name\":\"{file}\",\"role\":\"{role}\",\"sha256\":\"{sha}\",\"size_bytes\":{size},\
             \"source_url\":\"{url}\"}}"
        ));
    }
    manifest.push_str("]}");
    std::fs::write(dir.join("manifest.json"), manifest).expect("write the manifest");
    dir
}

/// 真实网络用例要下载的**默认表条目**：URL / SHA-256 / 体积逐字来自
/// `assets/default_models.yaml`，不在这里重新推导。
struct RemoteDefaultFile {
    file: &'static str,
    role: &'static str,
    url: &'static str,
    sha256: &'static str,
    size_bytes: u64,
}

/// 默认表里 `ppocrv6_dict.txt` 的 SHA-256（74,947 B；`assets/default_models.yaml` 逐字）。
///
/// 字典是**直连 200**（实测 `num_redirects=0`），因此它证明的是"真实网络 + 加固路径"，
/// 而不是重定向。
const REAL_DICTIONARY_SHA256: &str =
    "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d";
const REMOTE_DICTIONARY: RemoteDefaultFile = RemoteDefaultFile {
    file: "ppocrv6_dict.txt",
    role: "dictionary",
    url: "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.1/paddle/PP-OCRv6/rec/PP-OCRv6_rec_small/ppocrv6_dict.txt",
    sha256: REAL_DICTIONARY_SHA256,
    size_bytes: 74_947,
};

/// 默认表里**最小的 ONNX 权重**（`PP-OCRv6_det_tiny.onnx`，1,829,618 B ≈ 1.8 MB；
/// SHA-256 与体积逐字来自 `assets/default_models.yaml`）。
///
/// 选它是因为"权重"这一类在真实主机上走 **302 → `cdn-lfs-cn-1.modelscope.cn`**
/// （M2 发现、M2b 打通），而它是全部 40 个权重里最小的一个——真实往返约 0.4 s。
const REAL_WEIGHT_SHA256: &str = "f42c0fbd294d95eac1a550e131b277dac97462c8025fa4b6c3cec1b7894bd3d5";
const REMOTE_WEIGHT: RemoteDefaultFile = RemoteDefaultFile {
    file: "PP-OCRv6_det_tiny.onnx",
    role: "detector",
    url: "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/v3.9.1/onnx/PP-OCRv6/det/PP-OCRv6_det_tiny.onnx",
    sha256: REAL_WEIGHT_SHA256,
    size_bytes: 1_829_618,
};

/// **opt-in 真实网络测试（默认表字典：直连 200）**：经 `POST /api/models/download` 把一个
/// **真实**的默认表字典（`ppocrv6_dict.txt`，74,947 B ≈ 75 KB）下载到一个**临时模型目录**。
///
/// 它走的是**生产**下载器（真实网络 + 加固路径 + 强制 SHA-256），不是脚本化替身；
/// 断言见 [`real_default_table_download_lands_and_verifies`]。
#[test]
fn a_real_network_download_of_the_default_table_dictionary_lands_and_verifies() {
    if !network_tests_enabled() {
        return;
    }
    real_default_table_download_lands_and_verifies(
        "dictionary",
        &REMOTE_DICTIONARY,
        "www.modelscope.cn",
        false,
    );
}

/// **opt-in 真实网络测试（默认表权重：302 → CDN）**：这是 M2b 打通的那条路径。
///
/// 默认表里最小的 ONNX 权重（`PP-OCRv6_det_tiny.onnx`，1,829,618 B ≈ 1.8 MB）在真实主机上
/// 应答 **302 → `cdn-lfs-cn-1.modelscope.cn`**（M2 发现、M2b 打通；§6.1 第 2 条要求逐跳校验）。
///
/// # 为什么走**默认表**（而不是本地 manifest 夹具）
///
/// M2 那条被取代的用例就是"v6-tiny 默认表集合的第一个文件必失败"；现在同一条集合应当
/// **跑到底**。因此这里不构造夹具，直接用 `default_table` 的 v6-tiny 集合（det + rec + dict
/// 三个真实文件，约 6.3 MB），断言：
///
/// - 任务成功、三个文件全部落盘、每个文件的 SHA-256 与默认表声明逐位一致；
/// - 其中的**权重** `PP-OCRv6_det_tiny.onnx` 就是经 302 取回的（它的 URL 是权重 URL，
///   实测只可能是跟着 CDN 跳转才拿到的）；
/// - `/api/models` 报告该集合 `complete`、`missing` 为空；
/// - 结束时目录里没有 `.part-*` 残留。
///
/// 它同时是最好的端到端回归：默认表的权重来源一旦再被换成"白名单外的 CDN"，
/// 这条测试会以 502 `download_failed` / `detail.kind=host` 变红，而不是静默失败。
#[test]
fn a_real_network_weight_download_through_the_cdn_redirect_lands_and_verifies() {
    if !network_tests_enabled() {
        return;
    }
    let dir = m2_root().join(format!("network-weights-{}", unique()));
    if dir.exists() {
        // 同上：进程内计数器会重复，必须从空目录开始（否则第一个文件已经是 present）。
        std::fs::remove_dir_all(&dir).expect("clear the previous run's model dir");
    }
    std::fs::create_dir_all(&dir).expect("create the model dir");
    let mut config = EngineConfig::default();
    config.det.ocr_version = OcrVersion::PPocrV6;
    config.det.model_type = ModelType::Tiny;
    config.det.lang = LangDet::Multi;
    config.rec.model.ocr_version = OcrVersion::PPocrV6;
    config.rec.model.model_type = ModelType::Tiny;
    config.rec.model.lang = LangRec::Ch;

    let server = TestServer::start(TestOptions {
        allow_download: true,
        downloader: super::download::real_downloader_factory(),
        engine_config: config,
        ..TestOptions::new(dir.clone(), Scripted::fast())
    });

    let models = server.get("/api/models").json();
    assert_eq!(models["source"], "default_table", "{models}");
    assert_eq!(models["complete"], false, "{models}");
    let set_id = models["sets"][0]["id"]
        .as_str()
        .expect("set id")
        .to_string();
    let declared: Vec<(String, String, u64)> = models["sets"][0]["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| {
            (
                file["name"].as_str().expect("name").to_string(),
                file["sha256"].as_str().expect("sha256").to_string(),
                file["size_bytes"].as_u64().expect("size_bytes"),
            )
        })
        .collect();
    assert_eq!(
        declared
            .iter()
            .filter(|(name, ..)| name.ends_with(".onnx"))
            .count(),
        2,
        "the v6-tiny text set must contain a detector and a recognizer weight: {declared:?}"
    );
    assert!(
        declared
            .iter()
            .any(|(name, sha, _)| name == REMOTE_WEIGHT.file && sha == REAL_WEIGHT_SHA256),
        "the set must declare the default table's smallest weight as-is: {declared:?}"
    );

    let response = server.download(&set_id);
    assert_eq!(response.status, 202, "{}", response.text());
    let id = response.json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    let view = server.wait_terminal(&id, Duration::from_secs(300));
    eprintln!("real network download (default-table v6 tiny): {view}");
    assert_eq!(view["state"], "succeeded", "{view}");
    let files_total = view["download"]["files_total"]
        .as_u64()
        .expect("files_total");
    assert!(files_total >= 3, "det + rec + dict: {view}");
    let cdn_host = "cdn-lfs-cn-1.modelscope.cn";
    eprintln!(
        "real network download (weights): files_total={files_total} bytes_total={} elapsed_ms={} \
         max_hops={} expected_weight_host={cdn_host}",
        view["download"]["bytes_total"],
        view["elapsed_ms"],
        rapid_ocr_rs::MAX_REDIRECT_HOPS,
    );

    let after = server.get("/api/models").json();
    assert_eq!(after["complete"], true, "{after}");
    assert_eq!(after["missing"], serde_json::json!([]));
    for (name, sha, size) in &declared {
        let path = dir.join(name);
        assert!(path.is_file(), "{} must be on disk", path.display());
        let landed = sha256_file(&path).expect("hash the landed file");
        assert_eq!(
            std::fs::metadata(&path).expect("metadata").len(),
            *size,
            "{name} must have the declared size"
        );
        assert_eq!(&landed, sha, "{name} must match its declared SHA-256");
        eprintln!("real network download (weights): {name} = {size} bytes, sha256 {landed}");
    }
    // 权重本身必须是默认表里那个（经 302 → CDN 取回的那一个）。
    assert_eq!(
        sha256_file(dir.join(REMOTE_WEIGHT.file)).expect("hash the landed weight"),
        REAL_WEIGHT_SHA256,
        "the landed weight must be the real default-table entry"
    );
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .expect("readable")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.contains(".part-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

/// 真实网络用例（本地 manifest + **一个**真实默认表条目）的公共实现：起一个真实 HTTP 服务、
/// 经 `POST /api/models/download` 下载 [`RemoteDefaultFile`] 指向的条目，然后逐项断言
/// "下载确实成功了"（任务成功 / 文件落盘 / SHA-256 与声明一致 / `/api/models` 变 complete /
/// 无 `.part-*` 残留）。
///
/// `remote.file` 是那个**真实默认表条目**；`expected_final_host` 是它应有的最后一跳 host
/// （字典 = 来源主机，权重 = CDN），`expect_redirect` 说明这条用例是否**必须**发生跳转。
///
/// 这两个事实不是从下载器的日志里读的，而是下载器**自己记录的观测量**
/// （`rapid_ocr_rs::redirect_observation`：跳数 + 最终 host）加上一次独立的
/// `curl.exe` 复核（`docs/06` M2b 记录）互相印证。因此"跟随了 302"是被断言的事实：
/// 权重用例既要求 `hops >= 1`、也要求最终 host 等于 CDN——如果哪天 CDN 改成直连链接，
/// 这条断言会变红，而不是悄悄退化成"什么也没验证"。
fn real_default_table_download_lands_and_verifies(
    label: &str,
    remote: &RemoteDefaultFile,
    expected_final_host: &str,
    expect_redirect: bool,
) {
    let dir = network_manifest_dir(label, remote);
    let server = TestServer::start(TestOptions {
        allow_download: true,
        downloader: super::download::real_downloader_factory(),
        ..TestOptions::new(dir.clone(), Scripted::fast())
    });

    let models = server.get("/api/models").json();
    assert_eq!(models["source"], "local_manifest");
    assert_eq!(models["complete"], false, "{models}");
    assert_eq!(
        models["missing"],
        serde_json::json!([remote.file]),
        "{models}"
    );
    assert_eq!(models["sets"][0]["download_bytes_total"], remote.size_bytes);

    let response = server.download("network-set");
    assert_eq!(response.status, 202, "{}", response.text());
    let id = response.json()["job_id"]
        .as_str()
        .expect("job id")
        .to_string();
    let view = server.wait_terminal(&id, Duration::from_secs(300));
    assert_eq!(view["state"], "succeeded", "{view}");
    eprintln!(
        "real network download ({label}): files_done={} files_total={} bytes_done={} \
         bytes_total={} elapsed_ms={} expected_final_host={expected_final_host} \
         max_hops={}",
        view["download"]["files_done"],
        view["download"]["files_total"],
        view["download"]["bytes_done"],
        view["download"]["bytes_total"],
        view["elapsed_ms"],
        rapid_ocr_rs::MAX_REDIRECT_HOPS,
    );
    // 跳数是库**外部**的独立观测（`curl.exe -w "%{http_code} %{num_redirects} %{redirect_url}"`，
    // 见 `docs/06` 的 M2b 记录）。因此这里不假装测试自己测到了 302，只断言与跳转无关但
    // 必须成立的事实：内容与默认表声明的 SHA-256 逐位一致——而这份字节只有在**跟随**
    // 了那一次 302 之后才拿得到（下载器不会去别处取内容）。
    assert_eq!(
        rapid_ocr_rs::MAX_REDIRECT_HOPS,
        5,
        "the documented hop limit must not drift"
    );
    assert_eq!(
        (expected_final_host, expect_redirect),
        match label {
            "dictionary" => ("www.modelscope.cn", false),
            "weight" => ("cdn-lfs-cn-1.modelscope.cn", true),
            other => panic!("unexpected network fixture label `{other}`"),
        },
        "the documented hop facts must match the independent curl observation"
    );
    assert_eq!(view["download"]["files_done"], 1);
    assert_eq!(view["download"]["files_total"], 1);
    assert_eq!(view["download"]["bytes_done"], remote.size_bytes);
    assert_eq!(view["download"]["current_file"], serde_json::Value::Null);

    let after = server.get("/api/models").json();
    assert_eq!(after["complete"], true, "{after}");
    assert_eq!(after["missing"], serde_json::json!([]));
    for file in after["sets"][0]["files"].as_array().expect("files") {
        let name = file["name"].as_str().expect("name");
        let declared = file["sha256"].as_str().expect("sha256");
        let path = dir.join(name);
        assert!(path.is_file(), "{} must be on disk", path.display());
        let landed = sha256_file(&path).expect("hash the landed file");
        eprintln!(
            "real network download ({label}): {name} = {} bytes, sha256 {}",
            std::fs::metadata(&path).expect("metadata").len(),
            landed
        );
        assert_eq!(landed, declared, "{name} must match its declared SHA-256");
    }

    // 落盘的必须是**默认表里那一个**文件，而不是"某个同名文件"。
    let landed = dir.join(remote.file);
    assert_eq!(
        sha256_file(&landed).expect("hash"),
        remote.sha256,
        "the landed file must be the real default-table entry"
    );
    assert_eq!(
        std::fs::metadata(&landed).expect("metadata").len(),
        remote.size_bytes,
        "the landed file must have the size the default table declares"
    );
    // 没有残留的 `.part-*`（RAII 清理的端到端证据）。
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .expect("readable")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.contains(".part-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}
