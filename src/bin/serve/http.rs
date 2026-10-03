//! HTTP 层：路由、准入顺序、响应头与端点分发（§4.2、§4.4、§7、§9.5）。
//!
//! # 这是**唯一**引用 `tiny_http` 的地方
//!
//! `tiny_http` 是 optional 依赖且不进 `default`（`Cargo.toml` 的 `serve` feature），
//! 库侧依旧零 HTTP 依赖。`src/bin/serve/mod.rs` 的边界测试会扫描整个 `serve/` 子树：
//! 只有本文件可以出现 `tiny_http` 的路径引用。
//!
//! # 线程模型
//!
//! accept loop 跑在一个具名线程上（`serve-accept`），主线程在它上面 join（`run.rs`）。
//! 循环用 `recv_timeout` + 关闭标志，退出时用 `Server::unblock()` 立即唤醒；
//! 静态/校验类请求**就地**处理（廉价，无 I/O 阻塞），OCR 请求只做准入 + 有界读取 +
//! 入队，**推理绝不在本线程执行**（§8.2）。
//!
//! # 准入顺序（§4.4，逐条对应 `admit.rs` 的冻结顺序）
//!
//! 1. 路由 → 404/405（本模块的 `route_of` 给出结论）；
//! 2. token → 401；3. Host → 421、Origin（仅 `POST/PUT/DELETE`）→ 403；
//! 4. 队列容量预检 → 503（**尚未读取请求体**）；5. `Content-Length` 预检 → 413；
//! 6. 有界流式读取（总字节上限 + 读取超时）；7. 通过后才建任务。

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tiny_http::{Header, Request, Response, Server, StatusCode};

use super::admit::{
    self, AdmissionError, BodySource, ChunkOutcome, HttpMethod, RequestDescriptor, RouteDecision,
};
use super::error::ServeError;
use super::run::ServeStartError;
use super::security::{self, SECURITY_HEADERS};
use super::server::{Body, READ_CHUNK_BYTES, ServeContext, ServeRuntime, ServeShared};

/// accept loop 的等待上限（§8.1：`recv_timeout` + 关闭标志）。
const ACCEPT_TIMEOUT: Duration = Duration::from_millis(200);
/// 请求体读取的整体上限（§4.4 第 6 步的"读取超时"）。
///
/// **已知边界**：`tiny_http` 不暴露 socket 读超时，因此一次已经发起、对端却不发数据的
/// `read` 无法被中断；本实现保证的是"**每次尝试读取前**检查截止时间"，
/// 以及 chunked/无长度体的总字节上限。见 M1 报告的"未覆盖风险"。
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// `?max_side=` 的允许上界（与库内 `clamp(32.0, 8192.0)` 的量级一致，留出余量）。
const MAX_SIDE_CEILING: u32 = 32_768;

/// 主页面 CSP（§9.5 第 3 条；**不含** `unsafe-inline`，nonce 与页面属性逐字节相同）。
pub(super) fn page_csp(nonce: &str) -> String {
    format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; img-src 'self' \
         blob: data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors \
         'none'"
    )
}

/// 路由表（§4.2 的 M1 子集 + M2 的 `POST /api/engine/reload`）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    Page,
    Status,
    Models,
    ModelsDownload,
    EngineReload,
    Ocr,
    Job(String),
    JobResult(String),
    JobCancel(String),
}

impl Route {
    fn method(&self) -> HttpMethod {
        match self {
            Self::Page | Self::Status | Self::Models | Self::Job(_) | Self::JobResult(_) => {
                HttpMethod::Get
            }
            Self::ModelsDownload | Self::EngineReload | Self::Ocr | Self::JobCancel(_) => {
                HttpMethod::Post
            }
        }
    }

    fn allow(&self) -> &'static str {
        match self.method() {
            HttpMethod::Get => "GET",
            _ => "POST",
        }
    }

    /// §7.2：令牌对**所有 `/api/*`** 必需，`GET /`（令牌的下发者本身）除外。
    fn requires_token(&self) -> bool {
        !matches!(self, Self::Page)
    }
}

/// 已绑定的监听端口（§7.6 第 1 步的产物：绑定与断言都在这里完成）。
pub(super) struct BoundServer {
    server: Arc<Server>,
    local_addr: SocketAddr,
}

impl BoundServer {
    /// 绑定**硬编码**的 loopback 地址（`security::bind_address`：签名里没有地址参数），
    /// 并断言实际绑定地址属于 loopback（§7.1）。
    pub fn bind(port: u16) -> Result<Self, ServeStartError> {
        let requested = security::bind_address(port);
        let server = Server::http(requested).map_err(|error| ServeStartError::Bind {
            address: requested,
            detail: error.to_string(),
        })?;
        let bound = server.server_addr().to_ip().ok_or(ServeStartError::Bind {
            address: requested,
            detail: "the listener does not expose an IPv4 address".to_string(),
        })?;
        let local_addr = security::assert_loopback(bound)?;
        Ok(Self {
            server: Arc::new(server),
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

/// 运行期句柄：绑定的 `Server` + 运行期 + accept 线程。
pub(super) struct ServeHandle {
    server: Arc<Server>,
    runtime: ServeRuntime,
    accept: Option<JoinHandle<()>>,
    local_addr: SocketAddr,
}

impl ServeHandle {
    /// 建运行期并启动 accept 线程（绑定必须已经完成）。
    pub fn start(bound: BoundServer, context: ServeContext) -> Result<Self, ServeStartError> {
        let BoundServer { server, local_addr } = bound;
        let runtime = ServeRuntime::new(context).map_err(ServeStartError::Runtime)?;
        let accept = {
            let server = Arc::clone(&server);
            let shared = Arc::clone(runtime.shared());
            thread::Builder::new()
                .name("serve-accept".to_string())
                .spawn(move || accept_loop(&server, &shared))
                .map_err(ServeStartError::Runtime)?
        };
        Ok(Self {
            server,
            runtime,
            accept: Some(accept),
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn shared(&self) -> &Arc<ServeShared> {
        self.runtime.shared()
    }

    /// 阻塞直到 accept loop 退出（进程正常运行时即整个服务生命周期）。
    pub fn accept_loop(&mut self) {
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

/// 关闭是**无条件**的：设置标志 → `Server::unblock()`（立刻唤醒 accept 线程）→
/// 等 accept 与三个 worker 退出。
///
/// 放在 `Drop` 而不是一个必须被记得调用的方法上：句柄被丢弃（正常返回、`?` 提前返回、
/// panic 展开）时都必须收敛，否则监听端口与 worker 会一直留着。
impl Drop for ServeHandle {
    fn drop(&mut self) {
        self.shared().shutdown();
        self.server.unblock();
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        self.runtime.stop();
    }
}

/// accept loop（§8.1）。
fn accept_loop(server: &Arc<Server>, shared: &Arc<ServeShared>) {
    while !shared.is_shutting_down() {
        match server.recv_timeout(ACCEPT_TIMEOUT) {
            Ok(Some(request)) => handle(shared, request),
            Ok(None) => {}
            Err(error) => {
                // tiny_http 用 `unblock()` 打破 recv 时也会走到这里；关闭中就不再报错。
                if !shared.is_shutting_down() {
                    eprintln!("serve: accept failed: {error}");
                }
            }
        }
    }
}

/// 一次请求：路由 → 准入 → 端点。
fn handle(shared: &Arc<ServeShared>, mut request: Request) {
    // `HttpMethod::parse` 是 M0c 里方法解析的唯一实现（大小写不敏感）。
    let method = HttpMethod::parse(request.method().as_str());
    // `Request::url()` 借用请求本身，而后面还要可变借用它来读 body：先拷成自有字符串。
    let url = request.url().to_string();
    let (path, query) = split_url(&url);
    let (decision, route) = route_of(method, path);

    // §4.4 第 1 步：路由结论先于一切（404/405 不读 body、不校验 token）。
    if decision != RouteDecision::Matched {
        let headers = match (&route, decision) {
            (Some(known), RouteDecision::MethodNotAllowed) => {
                vec![("Allow".to_string(), known.allow().to_string())]
            }
            _ => Vec::new(),
        };
        let _ = respond(request, route_error(decision, method, path), &headers);
        return;
    }
    let route = route.expect("a matched route decision always carries its route");

    // 第 4 步的预检对象。`queue` 参数在这里**宽容解析**：非法取值不改变判定顺序
    // （token → Host/Origin → 队列 → 长度），它会在准入之后由 `dispatch` 以 400 回答。
    let class = match route {
        Route::Ocr => shared.routing().class_for(query_param(query, "queue")).ok(),
        _ => None,
    };

    // 头字段先取成自有字符串：`RequestDescriptor` 借用的是这份局部数据，而不是
    // `Request` 本身，否则下面读 body 需要的可变借用会与之冲突。
    let token_header = header(&request, "X-RapidOCR-Token").map(str::to_string);
    let host_header = header(&request, "Host").map(str::to_string);
    let origin_header = header(&request, "Origin").map(str::to_string);
    let content_type_header = header(&request, "Content-Type").map(str::to_string);
    let content_length = request.body_length().map(|length| length as u64);
    let is_chunked = header(&request, "Transfer-Encoding").is_some();

    let descriptor = RequestDescriptor {
        method,
        path,
        route: decision,
        // §7.2：`GET /` 是令牌的**下发者**，它自己不可能要求令牌；其余路由一律要求。
        has_token: !route.requires_token()
            || token_header
                .as_deref()
                .is_some_and(|candidate| shared.token().matches(candidate)),
        host: host_header.as_deref(),
        origin: origin_header.as_deref(),
        // 第 4 步：队列容量（**尚未读取 body**）。下载 channel 的容量由 `try_send` 兜底。
        queue_full: class.is_some_and(|class| shared.queue_full(class)),
        content_length,
        // tiny_http 不暴露"预读进缓冲区的字节数"；上限由第 6 步的流式账本兜住。
        body_so_far: 0,
        content_type: content_type_header.as_deref(),
        is_chunked,
    };

    let admitted = match admit::admit(&descriptor, shared.local(), shared.limits().max_body_bytes) {
        Ok(admitted) => admitted,
        Err(error) => {
            // 第 1 步的路由结论由 `admit` 原样回传（M0c 不伪造 code）；第 2–5 步的失败
            // 就是 §11.1 的 `ServeError`。
            match &error {
                AdmissionError::Route {
                    path: route_path,
                    method: route_method,
                    decision: route_decision,
                } => {
                    let body = route_error(*route_decision, *route_method, route_path);
                    let _ = respond(request, body, &[]);
                }
                AdmissionError::Rejected(_) => {
                    let serve_error = error
                        .rejected()
                        .expect("a Rejected admission error always carries a ServeError");
                    let body = error_body(shared, &route, serve_error);
                    let _ = respond(request, body, &[]);
                }
            }
            return;
        }
    };

    let outcome = dispatch(shared, &mut request, &route, query, &descriptor, admitted);
    let (body, headers) = match outcome {
        Ok(result) => result,
        Err(error) => (error_body(shared, &route, &error), Vec::new()),
    };
    let _ = respond(request, body, &headers);
}

/// 端点分发（准入已通过，这里才允许读 body）。
fn dispatch(
    shared: &Arc<ServeShared>,
    request: &mut Request,
    route: &Route,
    query: &str,
    descriptor: &RequestDescriptor<'_>,
    admitted: admit::Admit,
) -> Result<(Body, Vec<(String, String)>), ServeError> {
    match route {
        Route::Page => Ok((
            Body::html(shared.page().as_bytes().to_vec()),
            vec![(
                "Content-Security-Policy".to_string(),
                page_csp(shared.nonce()),
            )],
        )),
        Route::Status => Ok((json_body(200, shared.status_json())?, Vec::new())),
        Route::Models => Ok((json_body(200, shared.models_json())?, Vec::new())),
        Route::Ocr => {
            descriptor.check_content_type()?;
            let bytes = read_body(request, admitted)?;
            let max_side = parse_max_side(query_param(query, "max_side"), shared.min_side_len())?;
            let class = shared.routing().class_for(query_param(query, "queue"))?;
            let value = shared.submit_ocr(bytes, class, max_side)?;
            // §4.2：任务提交返回 **202**（异步任务，不存在"同步返回结果"的第二套语义）。
            Ok((json_body(202, value)?, Vec::new()))
        }
        Route::ModelsDownload => {
            check_json_content_type(descriptor.content_type)?;
            let bytes = read_body(request, admitted)?;
            let set_id = parse_set_id(&bytes)?;
            let value = shared.submit_download(&set_id)?;
            Ok((json_body(202, value)?, Vec::new()))
        }
        Route::EngineReload => {
            // 显式创建/重建引擎（§4.2、§7.6）。没有请求体，也不读 body。
            Ok((json_body(200, shared.reload_engine())?, Vec::new()))
        }
        Route::Job(id) => Ok((json_body(200, shared.job_view(id)?)?, Vec::new())),
        Route::JobResult(id) => Ok((shared.job_result(id)?, Vec::new())),
        Route::JobCancel(id) => Ok((json_body(200, shared.cancel_job(id)?)?, Vec::new())),
    }
}

/// 错误响应：OCR 的 409 带回与 `/api/models` 一致的清单（§7.6）。
///
/// `models_missing` 与 `models_corrupt` 由 [`ServeShared::models_missing_error`] 决定
/// （有损坏文件时报 `models_corrupt`），`detail` 复用 `/api/models` 的字段名与值。
fn error_body(shared: &ServeShared, route: &Route, error: &ServeError) -> Body {
    match (route, error) {
        (Route::Ocr, ServeError::ModelsMissing | ServeError::ModelsCorrupt) => {
            let error = shared.models_missing_error();
            Body::error_with_detail(&error, shared.models_missing_detail())
        }
        _ => Body::error(error),
    }
}

/// 404 / 405 的响应体（§4.4 第 1 步）。
///
/// 这两个状态码**不是** `ServeError` 的变体（§11.1 没有为它们定义 `code`），因此由路由层
/// 用固定的 `code` 回答；`detail` 里带上路由结论，便于定位"到底哪个路径/方法没匹配"。
fn route_error(decision: RouteDecision, method: HttpMethod, path: &str) -> Body {
    let (status, code, message) = match decision {
        RouteDecision::NotFound => (404, "not_found", "no route matches this path".to_string()),
        RouteDecision::MethodNotAllowed => (
            405,
            "method_not_allowed",
            format!("{} is not allowed on this path", method.name()),
        ),
        RouteDecision::Matched => (
            500,
            "internal",
            "the router returned no route for a matched path".to_string(),
        ),
    };
    let body = json!({
        "code": code,
        "message": message,
        "detail": { "path": path, "method": method.name(), "route": decision.name() },
    });
    Body::json(
        status,
        serde_json::to_vec(&body).expect("route error serialization cannot fail"),
    )
}

/// 端点的 JSON 响应（状态码由调用方给出：提交是 202，其余是 200）。
fn json_body(status: u16, value: Value) -> Result<Body, ServeError> {
    let bytes = serde_json::to_vec(&value).map_err(|_| ServeError::Internal)?;
    Ok(Body::json(status, bytes))
}

/// 有界读取（§4.4 第 6 步）：总字节上限 + 截止时间。
fn read_body(request: &mut Request, admitted: admit::Admit) -> Result<Vec<u8>, ServeError> {
    let mut source = HttpBody {
        request,
        deadline: Instant::now() + BODY_READ_TIMEOUT,
    };
    admit::read_body(&mut source, admitted.expected_body, admitted.max_body)
}

/// `tiny_http` 的请求体 → `BodySource`（超时判定用注入的截止时间）。
struct HttpBody<'a> {
    request: &'a mut Request,
    deadline: Instant,
}

impl BodySource for HttpBody<'_> {
    fn read_timed_out(&self) -> bool {
        Instant::now() >= self.deadline
    }

    fn next_chunk(&mut self) -> ChunkOutcome {
        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
        match self.request.as_reader().read(&mut buffer) {
            Ok(0) => ChunkOutcome::End,
            Ok(read) => {
                buffer.truncate(read);
                ChunkOutcome::Data(buffer)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                ChunkOutcome::TimedOut
            }
            Err(error) => ChunkOutcome::Failed(error.to_string()),
        }
    }
}

/// `?max_side=`（§13 的参考命令）：正整数、且不低于引擎的 `min_side_len`
/// （低于它会让预处理区间上下界颠倒）。
fn parse_max_side(raw: Option<&str>, min_side: u32) -> Result<Option<u32>, ServeError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value: u32 = raw.trim().parse().map_err(|_| ServeError::BadRequest)?;
    if value < min_side || value > MAX_SIDE_CEILING {
        return Err(ServeError::BadRequest);
    }
    Ok(Some(value))
}

/// `POST /api/models/download` 的请求体：**只有** `set_id`（§7.2 禁止 URL）。
///
/// 白名单式校验：`Content-Type`（若给出）必须是 `application/json`，请求体必须是**恰好**
/// 一个 `set_id` 键的 JSON 对象——任何额外键（尤其 `url`）都会被拒绝，而不是被忽略。
fn parse_set_id(bytes: &[u8]) -> Result<String, ServeError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| ServeError::BadRequest)?;
    let object = value.as_object().ok_or(ServeError::BadRequest)?;
    if object.len() != 1 {
        return Err(ServeError::BadRequest);
    }
    let set_id = object
        .get("set_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or(ServeError::BadRequest)?;
    Ok(set_id.to_string())
}

/// JSON 请求体的媒体类型校验（缺省按 JSON 处理：本机工具不强制客户端声明类型）。
fn check_json_content_type(content_type: Option<&str>) -> Result<(), ServeError> {
    let Some(raw) = content_type else {
        return Ok(());
    };
    let essence = raw
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if essence == "application/json" {
        return Ok(());
    }
    Err(ServeError::BadRequest)
}

/// 路由表：路径 → 路由；方法不匹配 → `MethodNotAllowed`。
fn route_of(method: HttpMethod, path: &str) -> (RouteDecision, Option<Route>) {
    let candidate = match path {
        "/" => Some(Route::Page),
        "/api/status" => Some(Route::Status),
        "/api/models" => Some(Route::Models),
        "/api/models/download" => Some(Route::ModelsDownload),
        "/api/engine/reload" => Some(Route::EngineReload),
        "/api/ocr" => Some(Route::Ocr),
        _ => job_route(path),
    };
    match candidate {
        None => (RouteDecision::NotFound, None),
        Some(route) => {
            let expected = route.method();
            // HEAD 只对 GET 路由放行（tiny_http 会自动省略响应体）。
            let matched =
                method == expected || (method == HttpMethod::Head && expected == HttpMethod::Get);
            if matched {
                (RouteDecision::Matched, Some(route))
            } else {
                (RouteDecision::MethodNotAllowed, Some(route))
            }
        }
    }
}

/// `/api/jobs/{id}`、`/api/jobs/{id}/result`、`/api/jobs/{id}/cancel`。
fn job_route(path: &str) -> Option<Route> {
    let rest = path.strip_prefix("/api/jobs/")?;
    let mut parts = rest.split('/');
    let id = parts.next()?;
    if id.is_empty() {
        return None;
    }
    match (parts.next(), parts.next()) {
        (None, None) => Some(Route::Job(id.to_string())),
        (Some("result"), None) => Some(Route::JobResult(id.to_string())),
        (Some("cancel"), None) => Some(Route::JobCancel(id.to_string())),
        _ => None,
    }
}

/// `path?query` 拆分（不做百分号解码：M1 的参数都是 ASCII 的端口/整数/枚举值）。
fn split_url(url: &str) -> (&str, &str) {
    match url.split_once('?') {
        Some((path, query)) => (path, query),
        None => (url, ""),
    }
}

/// 取一个查询参数（重复出现时取第一个）。
fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

/// 请求头（大小写不敏感，`tiny_http` 的 `HeaderField::equiv` 即唯一实现）。
fn header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}

/// 写响应：安全头**每一个**响应都带（§7.3），额外头按需附加。
///
/// 这里**绝不**发送 `Access-Control-Allow-Origin`（§7.2）：CORS 头只可能让浏览器里的
/// 第三方页面读走本机结果，而本页是同源的，不需要它。
fn respond(request: Request, body: Body, extra: &[(String, String)]) -> io::Result<()> {
    let mut response = Response::from_data(body.bytes).with_status_code(StatusCode(body.status));
    response.add_header(header_of("Content-Type", body.content_type));
    for (name, value) in SECURITY_HEADERS {
        response.add_header(header_of(name, value));
    }
    for (name, value) in extra {
        response.add_header(header_of(name, value));
    }
    request.respond(response)
}

/// 内部响应头构造：所有取值都是本模块的常量（唯一动态项是十六进制 nonce）。
fn header_of(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .unwrap_or_else(|()| panic!("serve: header `{name}` is not ASCII"))
}

#[cfg(test)]
mod tests {
    use super::{
        HttpMethod, Route, RouteDecision, check_json_content_type, page_csp, parse_max_side,
        parse_set_id, query_param, route_of, split_url,
    };
    use crate::serve::error::ServeError;

    #[test]
    fn the_router_matches_the_m1_subset_and_nothing_else() {
        let cases = [
            (HttpMethod::Get, "/", Route::Page),
            (HttpMethod::Get, "/api/status", Route::Status),
            (HttpMethod::Get, "/api/models", Route::Models),
            (
                HttpMethod::Post,
                "/api/models/download",
                Route::ModelsDownload,
            ),
            (HttpMethod::Post, "/api/engine/reload", Route::EngineReload),
            (HttpMethod::Post, "/api/ocr", Route::Ocr),
            (
                HttpMethod::Get,
                "/api/jobs/job-1",
                Route::Job("job-1".into()),
            ),
            (
                HttpMethod::Get,
                "/api/jobs/job-1/result",
                Route::JobResult("job-1".into()),
            ),
            (
                HttpMethod::Post,
                "/api/jobs/job-1/cancel",
                Route::JobCancel("job-1".into()),
            ),
        ];
        for (method, path, expected) in cases {
            let (decision, route) = route_of(method, path);
            assert_eq!(decision, RouteDecision::Matched, "{path}");
            assert_eq!(route.as_ref(), Some(&expected), "{path}");
        }

        // M3/M4 的端点**不存在**：必须 404，而不是"看起来能用"。
        for path in [
            "/api/jobs/job-1/annotated.png",
            "/api/jobs/job-1/export",
            "/api/models/download/cancel",
            "/favicon.ico",
            "/api",
            "/api/jobs/",
        ] {
            let (decision, route) = route_of(HttpMethod::Get, path);
            assert_eq!(decision, RouteDecision::NotFound, "{path}");
            assert!(route.is_none(), "{path}");
        }

        // 路径存在但方法不对 → 405（并给出 Allow）。
        let (decision, route) = route_of(HttpMethod::Get, "/api/ocr");
        assert_eq!(decision, RouteDecision::MethodNotAllowed);
        assert_eq!(route.expect("known path").allow(), "POST");
        let (decision, route) = route_of(HttpMethod::Post, "/api/status");
        assert_eq!(decision, RouteDecision::MethodNotAllowed);
        assert_eq!(route.expect("known path").allow(), "GET");

        // HEAD 只对 GET 路由放行。
        assert_eq!(route_of(HttpMethod::Head, "/").0, RouteDecision::Matched);
        assert_eq!(
            route_of(HttpMethod::Head, "/api/ocr").0,
            RouteDecision::MethodNotAllowed
        );
    }

    #[test]
    fn urls_and_query_parameters_are_split_without_decoding() {
        assert_eq!(
            split_url("/api/ocr?max_side=2000"),
            ("/api/ocr", "max_side=2000")
        );
        assert_eq!(split_url("/"), ("/", ""));
        assert_eq!(
            query_param("max_side=2000&queue=text", "max_side"),
            Some("2000")
        );
        assert_eq!(
            query_param("max_side=2000&queue=text", "queue"),
            Some("text")
        );
        assert_eq!(query_param("a=1", "b"), None);
        assert_eq!(query_param("noseparator", "noseparator"), None);
    }

    #[test]
    fn the_documented_max_side_parameter_is_validated() {
        assert_eq!(parse_max_side(None, 30).expect("ok"), None);
        assert_eq!(parse_max_side(Some("2000"), 30).expect("ok"), Some(2000));
        assert_eq!(parse_max_side(Some(" 736 "), 30).expect("ok"), Some(736));
        for bad in ["0", "29", "abc", "-1", "999999999999999999999"] {
            assert!(parse_max_side(Some(bad), 30).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_download_body_carries_only_a_set_id() {
        assert_eq!(
            parse_set_id(br#"{"set_id":"PP-OCRv6-small-ch"}"#).expect("ok"),
            "PP-OCRv6-small-ch"
        );
        // §7.2：请求体**不得**携带 URL；白名单式校验拒绝任何额外键。
        for bad in [
            &br#"{"url":"https://evil.example/x.onnx"}"#[..],
            &br#"{"set_id":"a","url":"https://evil.example/x.onnx"}"#[..],
            &br#"{"set_id":""}"#[..],
            &br#"{"set_id":null}"#[..],
            &br#"not json"#[..],
            &br#"{}"#[..],
            &br#"[]"#[..],
        ] {
            assert!(
                matches!(parse_set_id(bad), Err(ServeError::BadRequest)),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }

        // JSON 体只接受 application/json（或完全不声明）。
        assert!(check_json_content_type(None).is_ok());
        assert!(check_json_content_type(Some("application/json; charset=utf-8")).is_ok());
        assert!(check_json_content_type(Some("application/octet-stream")).is_err());
        assert!(check_json_content_type(Some("multipart/form-data")).is_err());
    }

    #[test]
    fn the_main_page_csp_is_nonce_based_and_never_allows_inline() {
        let csp = page_csp("abc123");
        assert!(csp.contains("script-src 'nonce-abc123'"), "{csp}");
        assert!(csp.contains("style-src 'nonce-abc123'"), "{csp}");
        assert!(!csp.contains("unsafe-inline"), "{csp}");
        assert!(csp.contains("default-src 'none'"), "{csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    }
}
