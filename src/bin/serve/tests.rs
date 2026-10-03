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

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rapid_ocr_rs::{
    CoordinateSpace, DetectionOutcome, EngineConfig, EngineInfo, GenericProviderPreference,
    ImageInfo, ImageSize, LangDet, LangRec, ModelType, OcrOutput, OcrRegion, OcrRequest,
    OcrTimings, OcrVersion, Polygon, ProviderInfo, ProviderResolutionInfo, RapidOcrError,
    RecognitionOutcome, RegionSource, ResolvedProvider, StageReports, sha256_file,
};
use serde_json::Value;

use super::engine::{BackendProvider, EngineFactory, OcrBackend};
use super::http::{BoundServer, ServeHandle};
use super::limits::RawServeLimits;
use super::model_plan::ModelPlan;
use super::queue::QueueClass;
use super::run::render_page;
use super::security::{LocalOrigin, ServeToken, generate_nonce};
use super::server::{OcrRouting, ServeContext, ServeShared};
use super::state::ServeStartup;

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
        }
    }

    fn slow(delay: Duration) -> Self {
        Self {
            delay,
            ..Self::fast()
        }
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
    fn recognize(&mut self, _request: OcrRequest) -> Result<OcrOutput, RapidOcrError> {
        self.state.calls.fetch_add(1, Ordering::SeqCst);
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
            selected_ep: "cpu".to_string(),
            fallback_to_cpu: false,
        }
    }
}

/// 一份结构完整、可被 `to_output_json` 序列化的输出。
fn scripted_output(regions: usize, text_bytes: usize) -> OcrOutput {
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
    routing: OcrRouting,
    engine_factory: EngineFactory,
    model_dir: PathBuf,
    engine_config: EngineConfig,
}

impl TestOptions {
    fn new(model_dir: PathBuf, scripted: Scripted) -> Self {
        Self {
            limits: RawServeLimits::default(),
            allow_download: false,
            routing: OcrRouting::text_only(),
            engine_factory: scripted.factory(),
            model_dir,
            engine_config: test_engine_config(),
        }
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
        let token = ServeToken::generate();
        let nonce = generate_nonce();
        let page = render_page(&token, &nonce).expect("the page must inject cleanly");
        let startup =
            ServeStartup::validate(options.limits, options.engine_config, None, None, false)
                .expect("the test limits must be valid");
        let model_plan =
            ModelPlan::resolve(&options.model_dir, &startup.plan.engine).expect("model plan");
        let snapshot = model_plan.snapshot();
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
            routing: options.routing,
            engine_factory: options.engine_factory,
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
                send(self.addr, &bytes)
            }
            None => {
                raw.push_str("\r\n");
                send(self.addr, raw.as_bytes())
            }
        }
    }

    fn submit_ocr(&self, body: &[u8]) -> RawResponse {
        self.post(
            "/api/ocr",
            &[("Content-Type", "application/octet-stream")],
            body,
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

    let head_end =
        find(&buffer, b"\r\n\r\n").expect("the response must contain a header block") + 4;
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

/// 空的模型目录（默认表来源，全部文件缺失）。
fn empty_model_dir(name: &str) -> PathBuf {
    let dir = test_root().join(format!("empty-{name}-{}", unique()));
    std::fs::create_dir_all(&dir).expect("create the empty model dir");
    dir
}

/// 齐备的模型目录：本地 `manifest.json` + 三个内容为 `name` 的文件（SHA-256 真实计算）。
fn complete_model_dir(name: &str) -> PathBuf {
    let dir = test_root().join(format!("complete-{name}-{}", unique()));
    std::fs::create_dir_all(&dir).expect("create the model dir");
    let files = [
        ("det.onnx", "detector"),
        ("rec.onnx", "recognizer"),
        ("dict.txt", "dictionary"),
    ];
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

    // M3/M4 的端点不存在 → 404（不是 200，也不是 500）。
    for path in [
        "/api/jobs/job-0/annotated.png",
        "/api/jobs/job-0/export?format=json",
        "/api/engine/reload",
        "/favicon.ico",
    ] {
        let response = server.get(path);
        assert_eq!(response.status, 404, "{path}: {}", response.text());
        assert_eq!(response.code(), "not_found", "{path}");
    }

    let response = server.get("/api/ocr");
    assert_eq!(response.status, 405, "{}", response.text());
    assert_eq!(response.code(), "method_not_allowed");
    assert_eq!(response.header("Allow"), Some("POST"));
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
    // 空目录：默认表来源，三个文件全缺。
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
    assert_eq!(sets.len(), 1, "{value}");
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
    let missing: Vec<&str> = value["missing"]
        .as_array()
        .expect("missing")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(missing.len(), 3);

    // 引擎在空模型目录下是 BlockedModelsMissing，而**服务**是 Ready（§7.6）。
    let status = server.get("/api/status").json();
    assert_eq!(status["state"], "ready");
    assert_eq!(status["engine"]["state"], "blocked_models_missing");

    // OCR 409：字段与 /api/models **逐字节一致**。
    let rejected = server.submit_ocr(b"image bytes");
    assert_eq!(rejected.status, 409, "{}", rejected.text());
    assert_eq!(rejected.code(), "models_missing");
    let detail = &rejected.json()["detail"];
    assert_eq!(detail["missing"], value["missing"], "{detail}");
    assert_eq!(detail["corrupt"], value["corrupt"]);
    assert_eq!(detail["source"], value["source"]);
    assert_eq!(detail["model_dir"], value["model_dir"]);
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

#[test]
fn the_download_endpoint_degrades_visibly() {
    let dir = complete_model_dir("download");
    let disabled = TestServer::start(TestOptions::new(dir.clone(), Scripted::fast()));
    let response = disabled.post(
        "/api/models/download",
        &[("Content-Type", "application/json")],
        br#"{"set_id":"test-set"}"#,
    );
    assert_eq!(response.status, 403, "{}", response.text());
    assert_eq!(response.code(), "downloads_disabled");

    // 打开 --allow-download：任务被创建，但 M1 的处理体如实判失败（不含任何网络 I/O）。
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
    let id = accepted["job_id"].as_str().expect("job id").to_string();
    let view = enabled.wait_terminal(&id, Duration::from_secs(20));
    assert_eq!(view["state"], "failed", "{view}");
    assert_eq!(view["kind"], "model_download");
    let error = view["error"].as_str().unwrap_or_default();
    assert!(error.contains("M2"), "{error}");
    assert!(error.contains("no network request"), "{error}");

    // 未知 set_id 与带 URL 的请求体都必须被拒绝，而不是被忽略。
    let unknown = enabled.post(
        "/api/models/download",
        &[("Content-Type", "application/json")],
        br#"{"set_id":"nope"}"#,
    );
    assert_eq!(unknown.status, 400, "{}", unknown.text());
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
    let server = TestServer::start(TestOptions {
        limits: limits(4, 2),
        routing: OcrRouting { formula: true },
        ..TestOptions::new(complete_model_dir("fair-formula"), Scripted::slow(delay))
    });
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
    let server = TestServer::start(TestOptions {
        limits: limits(4, 2),
        routing: OcrRouting { formula: true },
        ..TestOptions::new(complete_model_dir("fair-text"), Scripted::slow(delay))
    });
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
