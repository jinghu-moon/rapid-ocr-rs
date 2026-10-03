//! 测试用的仓库外资产定位。
//!
//! 模型权重和公开测试集体积很大，不随 crate 提交，只能通过环境变量引用：
//!
//! - `RAPID_OCR_MODEL_ROOT`：模型根目录（例如 `<workspace>/OCR-Model`），也可以直接
//!   指向 `pp_formulanet_plus_m.onnx`；
//! - `RAPID_OCR_FORMULA_MODEL`：直接指向公式 ONNX，优先于根目录；
//! - `RAPID_OCR_FORMULA_TEST_ROOT`：公式测试集根目录（例如 `<workspace>/Formula-TestSet`）。
//!
//! 缺失资产时测试必须 **skip**，不允许 panic，也不允许回落到开发机绝对路径，
//! 否则干净 clone / CI 无法复现测试结论。需要在缺少资产时失败的流水线可以设置
//! `RAPID_OCR_REQUIRE_EXTERNAL_ASSETS=1`，此时缺失资产会 panic 并说明原因。

use std::path::{Path, PathBuf};

/// 公式模型在模型根目录下的标准相对路径。
pub const FORMULA_MODEL_RELATIVE: &str =
    "Formula-Recognition-Models/onnx/pp_formulanet_plus_m.onnx";

fn env_path(name: &str) -> Option<PathBuf> {
    let raw = std::env::var(name).ok()?;
    let trimmed = raw.trim().trim_matches('"');
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn require_external_assets() -> bool {
    std::env::var("RAPID_OCR_REQUIRE_EXTERNAL_ASSETS")
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
}

/// 返回可用资产，或在缺失时跳过/失败。
///
/// 返回值语义：`Some` 表示资产可用；`None` 表示当前环境缺少该资产，调用方必须
/// 直接 `return` 跳过测试。
pub fn asset(description: &str, value: Option<PathBuf>) -> Option<PathBuf> {
    match value {
        Some(path) => Some(path),
        None => {
            if require_external_assets() {
                panic!(
                    "missing external test asset: {description}; set the documented \
                     RAPID_OCR_MODEL_ROOT / RAPID_OCR_FORMULA_TEST_ROOT environment variables"
                );
            }
            eprintln!("skipping test: {description} is not available in this environment");
            None
        }
    }
}

/// 仓库内 fixture 目录，随仓库提交，任何环境都必须可用。
pub fn fixture_dir(kind: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(kind)
}

/// PP-FormulaNet_plus-M ONNX 路径；缺失时返回 `None`。
pub fn formula_model_path() -> Option<PathBuf> {
    let candidate = env_path("RAPID_OCR_FORMULA_MODEL")
        .filter(|path| path.is_file())
        .or_else(|| {
            let root = env_path("RAPID_OCR_MODEL_ROOT")?;
            if root.is_file() {
                return Some(root);
            }
            let direct = root.join(FORMULA_MODEL_RELATIVE);
            direct.is_file().then_some(direct)
        });
    asset(
        "PP-FormulaNet_plus-M onnx (set RAPID_OCR_MODEL_ROOT or RAPID_OCR_FORMULA_MODEL)",
        candidate,
    )
}

/// `pix2text-mfd-1.5.onnx` 页面公式检测模型路径；缺失时返回 `None`。
pub fn formula_detector_path() -> Option<PathBuf> {
    let candidate = env_path("RAPID_OCR_FORMULA_DETECT_MODEL")
        .filter(|path| path.is_file())
        .or_else(|| {
            let root = env_path("RAPID_OCR_MODEL_ROOT")?;
            let direct = root.join("Formula-Detection-Model/pix2text-mfd-1.5.onnx");
            direct.is_file().then_some(direct)
        });
    asset(
        "pix2text-mfd-1.5 onnx (set RAPID_OCR_MODEL_ROOT or RAPID_OCR_FORMULA_DETECT_MODEL)",
        candidate,
    )
}

/// 普通 OCR 模型根目录（`OCR-Model`）；缺失时返回 `None`。
pub fn ocr_model_root() -> Option<PathBuf> {
    let candidate = env_path("RAPID_OCR_MODEL_ROOT").filter(|path| path.is_dir());
    asset("OCR model root (set RAPID_OCR_MODEL_ROOT)", candidate)
}

/// 页面级集成测试用的真实页面图片。
///
/// 位置从模型根目录的**同级目录**推导（`<workspace>/OCR-test-image`），也可以通过
/// `RAPID_OCR_TEST_IMAGES` 覆盖；两者都不可用时返回 `None` 并跳过测试。
pub fn page_fixture(name: &str) -> Option<PathBuf> {
    let candidate = env_path("RAPID_OCR_TEST_IMAGES")
        .map(|root| root.join(name))
        .filter(|path| path.is_file())
        .or_else(|| {
            let root = env_path("RAPID_OCR_MODEL_ROOT")?;
            let root = root.parent()?.join("OCR-test-image").join(name);
            root.is_file().then_some(root)
        });
    asset(
        &format!("page fixture `{name}` (set RAPID_OCR_TEST_IMAGES or RAPID_OCR_MODEL_ROOT)"),
        candidate,
    )
}

/// 公式测试集根目录；缺失时返回 `None`。
pub fn formula_dataset_root() -> Option<PathBuf> {
    let candidate = env_path("RAPID_OCR_FORMULA_TEST_ROOT").filter(|path| path.is_dir());
    asset(
        "formula test set (set RAPID_OCR_FORMULA_TEST_ROOT)",
        candidate,
    )
}

/// 单元测试用的临时目录：进程内唯一，`Drop` 时删除。
///
/// 模型清单的逐文件状态、来源选择都需要真实文件系统行为（存在 / 缺失 / 哈希不匹配），
/// 因此这些测试用同一个小工具建目录，而不是各自实现一遍临时目录命名与清理。
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rapid-ocr-rs-{label}-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("temp dir should be creatable");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&self, name: &str, contents: &[u8]) {
        std::fs::write(self.path.join(name), contents).expect("fixture should be writable");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// 单测用的本机 HTTP fixture 服务器
// ---------------------------------------------------------------------------

/// 测试用的 HTTP 服务器（`127.0.0.1:0`，手写响应，不引入任何依赖）。
///
/// 加固下载器的测试**不得依赖公网**：网络行为（重定向、没有 `Content-Length` 的
/// 分块/关闭定界响应、故意卡住的连接、连接计数）全部由这个服务器在环回地址上复现。
/// 它也负责"单飞只发一次请求"的证据：[`HttpFixture::request_count`] 统计的是**接受的连接数**
/// （每个响应都带 `Connection: close`，因此一次请求恰好一条连接）。
pub struct HttpFixture {
    addr: std::net::SocketAddr,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
}

/// 收到的一次请求（测试关心的最小集合）。
pub struct FixtureRequest {
    /// 该连接在该服务器上的序号（从 0 开始）。
    pub index: usize,
    pub method: String,
    /// 请求行里的路径（含 query）。
    pub target: String,
    pub headers: Vec<(String, String)>,
}

impl FixtureRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// fixture 服务器的一种响应。
pub enum FixtureResponse {
    /// `200 OK` + `Content-Length` + body。
    Body(Vec<u8>),
    /// 任意状态行 + 头 + body（没有显式 `Content-Length` 时自动补上）。
    Status {
        status: u16,
        reason: &'static str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// `Transfer-Encoding: chunked`：**没有** `Content-Length`（流式上限场景）。
    Chunked(Vec<u8>),
    /// 既没有 `Content-Length` 也不 chunked：写完 body 直接关闭（关闭定界帧）。
    CloseDelimited(Vec<u8>),
    /// 先发出这些字节（通常是带 `Content-Length` 的响应头），然后停住不关连接。
    Stall {
        head: Vec<u8>,
        hold: std::time::Duration,
    },
    /// 接受连接后什么都不发。
    Silent { hold: std::time::Duration },
}

impl FixtureResponse {
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self::Body(body.into())
    }

    pub fn status(status: u16, reason: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self::Status {
            status,
            reason,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn redirect(location: &str) -> Self {
        Self::Status {
            status: 302,
            reason: "Found",
            headers: vec![("Location".to_string(), location.to_string())],
            body: Vec::new(),
        }
    }

    pub fn chunked(body: impl Into<Vec<u8>>) -> Self {
        Self::Chunked(body.into())
    }

    pub fn close_delimited(body: impl Into<Vec<u8>>) -> Self {
        Self::CloseDelimited(body.into())
    }

    /// 声明 `content_length` 字节的响应体，但只发出响应头就停住（读取超时场景）。
    pub fn stall_after_head(content_length: u64, hold: std::time::Duration) -> Self {
        Self::Stall {
            head: format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
            )
            .into_bytes(),
            hold,
        }
    }

    /// 接受连接但不发任何响应（等待响应/连接预算耗尽的场景）。
    pub fn silent(hold: std::time::Duration) -> Self {
        Self::Silent { hold }
    }
}

impl HttpFixture {
    /// 启动服务器；`handler` 按连接被调用（可为 `Fn`，内部用 `Arc` 共享）。
    pub fn start<F>(handler: F) -> Self
    where
        F: Fn(&FixtureRequest) -> FixtureResponse + Send + Sync + 'static,
    {
        use std::{
            net::TcpListener,
            sync::{
                Arc,
                atomic::{AtomicBool, AtomicUsize},
            },
            thread,
            time::Duration,
        };

        let listener =
            TcpListener::bind("127.0.0.1:0").expect("the fixture server must bind a loopback port");
        let addr = listener
            .local_addr()
            .expect("a bound listener must expose its address");
        listener
            .set_nonblocking(true)
            .expect("the fixture listener must be non-blocking so it can be stopped");
        let requests = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(handler);

        let accept = {
            let requests = Arc::clone(&requests);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let index = requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let handler = Arc::clone(&handler);
                            thread::spawn(move || {
                                serve_connection(stream, index, handler.as_ref())
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        // Windows 上 `accept` 也会因为"客户端在 accept 之前就放弃连接"
                        // （WSAECONNABORTED / WSAECONNRESET）而报错；那不是"服务器该退出"，
                        // 因此只当作一次空转，绝不结束 accept 循环。
                        Err(_) => thread::sleep(Duration::from_millis(2)),
                    }
                }
            })
        };

        Self {
            addr,
            requests,
            shutdown,
            accept: Some(accept),
        }
    }

    /// 指向该服务器的一次性 URL（明文 `http://`，见 `DownloadPolicy` 的测试策略说明）。
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// 实际监听的环回地址（测试要直接连它时用）。
    pub const fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    /// 已接受的连接数（= 已处理的请求数，因为每个响应都 `Connection: close`）。
    pub fn request_count(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }
}

fn serve_connection(
    mut stream: std::net::TcpStream,
    index: usize,
    handler: &(dyn Fn(&FixtureRequest) -> FixtureResponse + Send + Sync),
) {
    // **必须显式改回阻塞模式**：Windows 上 `accept()` 的套接字会继承监听套接字的
    // 非阻塞属性（监听套接字为了可关闭被设成非阻塞）。若不改回来，请求稍晚到达时
    // `read` 会立刻返回 `WouldBlock`，服务器就会在没有响应的情况下关闭连接，
    // 客户端看到的是 `WSAECONNABORTED (10053)`——一个只在时序巧合下出现的假失败。
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_nodelay(true);
    let Some(request) = read_request(&mut stream, index) else {
        return;
    };
    let response = handler(&request);
    write_response(&mut stream, response);
}

fn read_request(stream: &mut std::net::TcpStream, index: usize) -> Option<FixtureRequest> {
    use std::io::Read as _;

    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") || buffer.len() > 64 * 1024 {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(_) => return None,
        }
    }

    let text = String::from_utf8_lossy(&buffer).into_owned();
    let mut lines = text.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();
    let headers = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect();

    Some(FixtureRequest {
        index,
        method,
        target,
        headers,
    })
}

fn write_response(stream: &mut std::net::TcpStream, response: FixtureResponse) {
    use std::io::Write as _;

    match response {
        FixtureResponse::Body(body) => {
            write_head(stream, 200, "OK", &[], &body);
        }
        FixtureResponse::Status {
            status,
            reason,
            headers,
            body,
        } => {
            write_head(stream, status, reason, &headers, &body);
        }
        FixtureResponse::Chunked(body) => {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            );
            for chunk in body.chunks(1024) {
                let _ = stream.write_all(format!("{:x}\r\n", chunk.len()).as_bytes());
                let _ = stream.write_all(chunk);
                let _ = stream.write_all(b"\r\n");
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        }
        FixtureResponse::CloseDelimited(body) => {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(&body);
        }
        FixtureResponse::Stall { head, hold } => {
            let _ = stream.write_all(&head);
            let _ = stream.flush();
            std::thread::sleep(hold);
        }
        FixtureResponse::Silent { hold } => {
            std::thread::sleep(hold);
        }
    }
    let _ = stream.flush();
}

fn write_head(
    stream: &mut std::net::TcpStream,
    status: u16,
    reason: &str,
    headers: &[(String, String)],
    body: &[u8],
) {
    use std::io::Write as _;

    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    let declares_length = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"));
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("Connection: close\r\n");
    if !declares_length {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read as _, Write as _},
        net::TcpStream,
        time::Duration,
    };

    use super::{FixtureResponse, HttpFixture};

    /// fixture 服务器自己的回归测试：**接受的连接必须等待请求**。
    ///
    /// 这条测试锁住一个真实踩过的坑：Windows 上 `accept()` 的套接字继承监听套接字的
    /// 非阻塞属性，若不在连接处理线程里改回阻塞模式，请求稍晚到达时服务器会立刻以
    /// `WouldBlock` 判定"读不到请求"，然后在没有响应的情况下关闭连接——客户端看到
    /// 的是随机的 `WSAECONNABORTED (10053)`。因此这里故意先连上、等 150 ms 再发请求。
    #[test]
    fn an_accepted_connection_waits_for_a_late_request() {
        let server = HttpFixture::start(|_| FixtureResponse::ok(b"pong".to_vec()));
        let mut stream = TcpStream::connect(server.addr()).expect("the fixture must accept");
        std::thread::sleep(Duration::from_millis(150));
        stream
            .write_all(b"GET /late HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .expect("the request must be writable");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("the fixture must answer a late request");
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("pong"), "{response}");
        assert_eq!(server.request_count(), 1);
    }
}
