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
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tiny_http::{Header, Request, Response, Server, StatusCode};

use rapid_ocr_rs::ProviderPreference;

use super::admit::{
    self, AdmissionError, BodySource, ChunkOutcome, HttpMethod, QueueAdmission, RequestDescriptor,
    RouteDecision,
};
use super::cli::ProviderChoice;
use super::error::ServeError;
use super::evaluate;
use super::export::ExportFormat;
use super::queue::QueueClass;
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

/// 导出响应的 CSP（§9.5 第 2 条，**独立**于主页面的 nonce CSP）。
///
/// 逐字来自 `docs/05` §9.5：
/// `default-src 'none'; style-src 'unsafe-inline'; img-src data:; script-src 'none'; sandbox`。
/// 三点含义：导出文档**不允许脚本**（`ReportMode::Static` 的正文里也一个 `<script` 都没有）、
/// 样式内联允许（报告的全部样式都是内联 `<style>`）、图片只允许 `data:`（导出的标注图就是
/// 内嵌的 data URL，因此脱离服务仍可查看）。
///
/// `style-src 'unsafe-inline'` **只**出现在这条导出响应上；主页面 CSP 仍是 nonce。
pub(super) const EXPORT_CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; img-src data:; script-src 'none'; sandbox";

/// 主页面 CSP（§9.5 第 3 条；**不含** `unsafe-inline`，nonce 与页面属性逐字节相同）。
pub(super) fn page_csp(nonce: &str) -> String {
    format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; img-src 'self' \
         blob: data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors \
         'none'"
    )
}

/// 路由表（§4.2 的全部端点；M3 起包含 `annotated.png` 与 `export`）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    Page,
    Status,
    Models,
    ModelsDownload,
    /// `POST /api/models/reverify`（A2）：清缓存 → 冷验证 → 重建引擎。
    ModelsReverify,
    EngineReload,
    Ocr,
    /// `POST /api/evaluate`（M4）：批量评估一份已标注清单。
    Evaluate,
    Job(String),
    JobResult(String),
    JobCancel(String),
    /// `GET /api/jobs/{id}/annotated.png`（M3）。
    JobAnnotated(String),
    /// `GET /api/jobs/{id}/export?format=…`（M3）。
    JobExport(String),
}

impl Route {
    fn method(&self) -> HttpMethod {
        match self {
            Self::Page
            | Self::Status
            | Self::Models
            | Self::Job(_)
            | Self::JobResult(_)
            | Self::JobAnnotated(_)
            | Self::JobExport(_) => HttpMethod::Get,
            Self::ModelsDownload
            | Self::ModelsReverify
            | Self::EngineReload
            | Self::Ocr
            | Self::Evaluate
            | Self::JobCancel(_) => HttpMethod::Post,
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
    // 第 4 步的**原子预留**入口（评审 P2-1）：`admit` 在判定容量的同一次加锁里占位，
    // 因此两个并发请求不可能都通过预检、都读入大 body。
    let slot_source = class.map(|class| QueueSlotSource { shared, class });

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
        // 第 4 步：队列容量（**尚未读取 body**）。判定与预留是同一次加锁；
        // 下载 channel 的容量由 `try_send` 兜底。
        queue: match &slot_source {
            Some(source) => QueueAdmission::Reserve(source),
            None => QueueAdmission::NotQueued,
        },
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
                    let body = shared.error_body(serve_error);
                    let _ = respond(request, body, &[]);
                }
            }
            return;
        }
    };

    // §4.4 第 4 步之后、第 6 步（读 body）之前：公式队列的模型**可用性**预检。
    //
    // 判定用的是与 `/api/models` **同一份**哈希状态（库的身份键控校验缓存：键 = 路径 +
    // 体积 + mtime）。首次见到某个身份时真的读盘（启动快照已经算过一次），命中只花一次
    // `stat`，因此"损坏但存在"的公式文件/检测模型在**读入请求体之前**就是 409
    // `models_corrupt`（评审 P1-2：只 `stat` 的旧预检会让它溜到 worker 里才失败）。
    // 文本队列不受影响。
    if matches!(route, Route::Ocr)
        && class == Some(QueueClass::Formula)
        && !shared.formula_models_ready()
    {
        let _ = respond(request, shared.formula_blocked_body(), &[]);
        return;
    }

    let outcome = dispatch(shared, &mut request, &route, query, &descriptor, admitted);
    match outcome {
        Ok(Dispatch::Respond(body, headers)) => {
            let _ = respond(request, body, &headers);
        }
        // 响应由另一个线程写（M3 的 provider 切换 / M2 的按当前文件重建）：本线程立刻回到
        // accept 循环，否则 `/api/status` 与 `/api/ocr` 会在整个建会话序列期间停摆。
        Ok(Dispatch::EngineWork(work)) => {
            if let Err(failure) = spawn_engine_work(shared, work, request) {
                // 请求没人接手（已有建会话序列在跑 / 线程起不来）：由本线程如实回答。
                let (error, request) = *failure;
                let _ = respond(request, Body::error(&error), &[]);
            }
        }
        // 响应由评估线程写（M4）：同一个理由（批量推理期间服务必须继续可观测、可提交任务）。
        Ok(Dispatch::Evaluate(manifest)) => {
            if let Err(failure) = spawn_evaluation(shared, manifest, request) {
                let (error, request) = *failure;
                let _ = respond(request, Body::error(&error), &[]);
            }
        }
        Err(error) => {
            let body = shared.error_body(&error);
            let _ = respond(request, body, &[]);
        }
    }
}

/// §4.4 第 4 步的 `QueueSlots` 生产实现：把 `Arc<ServeShared>` 的预留入口交给准入层。
///
/// 它只有两个字段（共享状态 + 队列类别），因此"预留"这件事在 http 层不引入任何新的状态：
/// 容量账目始终只有 `ServeShared.jobs` 一个所有者。
struct QueueSlotSource<'a> {
    shared: &'a Arc<ServeShared>,
    class: QueueClass,
}

impl admit::QueueSlots for QueueSlotSource<'_> {
    fn reserve(&self) -> Option<admit::QueueReservation> {
        self.shared.reserve_queue_slot(self.class)
    }
}

/// 端点分发的结论（见 [`dispatch`]）。
enum Dispatch {
    /// 本线程写响应。
    Respond(Body, Vec<(String, String)>),
    /// 交给 [`spawn_engine_work`]：它拥有请求对象（含写响应的责任）。
    ///
    /// `EngineWork::Reload(None)` = 无 body 的 `POST /api/engine/reload`（按磁盘上的当前文件
    /// 重建会话）；`EngineWork::Reload(Some(provider))` = M3 的显式 provider 设置应用；
    /// `EngineWork::ReverifyModels` = `POST /api/models/reverify`（A2）。**三者走同一条线程
    /// 路径**：ONNX Runtime 建会话绝不在 accept 线程上发生（评审 P2-2）。
    EngineWork(EngineWork),
    /// 交给 [`spawn_evaluation`]（M4）：评估是批量动作，同样由独立线程写响应。
    Evaluate(PathBuf),
}

/// 一次"引擎工作"的具体形态（见 [`Dispatch::EngineWork`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EngineWork {
    /// `POST /api/engine/reload`：`None` = 按当前文件重建，`Some` = 应用 provider 设置。
    Reload(Option<ProviderPreference>),
    /// `POST /api/models/reverify`：清缓存 + 冷验证 + 重建引擎（A2）。
    ReverifyModels,
}

/// 端点分发（准入已通过，这里才允许读 body）。
fn dispatch(
    shared: &Arc<ServeShared>,
    request: &mut Request,
    route: &Route,
    query: &str,
    descriptor: &RequestDescriptor<'_>,
    admitted: admit::Admit,
) -> Result<Dispatch, ServeError> {
    match route {
        Route::Page => Ok(Dispatch::Respond(
            Body::html(shared.page().as_bytes().to_vec()),
            vec![(
                "Content-Security-Policy".to_string(),
                page_csp(shared.nonce()),
            )],
        )),
        Route::Status => Ok(Dispatch::Respond(
            json_body(200, shared.status_json())?,
            Vec::new(),
        )),
        Route::Models => Ok(Dispatch::Respond(
            json_body(200, shared.models_json())?,
            Vec::new(),
        )),
        Route::Ocr => {
            descriptor.check_content_type()?;
            let bytes = read_body(request, &admitted)?;
            let max_side = parse_max_side(query_param(query, "max_side"), shared.min_side_len())?;
            let class = shared.routing().class_for(query_param(query, "queue"))?;
            // 第 4 步预留的槽位随请求一路传到这里：入队成功即提交，任何提前返回都会归还容量。
            let value = shared.submit_ocr(bytes, class, max_side, admitted.reservation)?;
            // §4.2：任务提交返回 **202**（异步任务，不存在"同步返回结果"的第二套语义）。
            Ok(Dispatch::Respond(json_body(202, value)?, Vec::new()))
        }
        Route::ModelsDownload => {
            check_json_content_type(descriptor.content_type)?;
            let bytes = read_body(request, &admitted)?;
            let set_id = parse_set_id(&bytes)?;
            let value = shared.submit_download(&set_id)?;
            Ok(Dispatch::Respond(json_body(202, value)?, Vec::new()))
        }
        Route::ModelsReverify => {
            // A2：一次动作完成"重新读盘"。请求体**必须为空**（这是无参数动作，任何 body
            // 都是协议级错误），并且**委派**给引擎工作线程：它要清缓存、冷验证 566 MB
            // 量级的模型并重建会话，绝不能在 accept 线程上跑（§8.2、评审 P2-2）。
            let body = read_body(request, &admitted)?;
            if !body.is_empty() {
                return Err(ServeError::BadRequest);
            }
            Ok(Dispatch::EngineWork(EngineWork::ReverifyModels))
        }
        Route::Evaluate => {
            check_json_content_type(descriptor.content_type)?;
            let bytes = read_body(request, &admitted)?;
            let manifest = evaluate::parse_manifest_body(&bytes)?;
            // **委派**：一次评估最多 `--max-eval-cases` 张图、每张一次完整推理；在 accept
            // 线程上跑会让 `/api/status`、两个 OCR 队列与下载在整个评估期间停摆。
            // 请求对象由 `handle` 交给那个线程（这里只做"要不要委派"的判定）。
            Ok(Dispatch::Evaluate(manifest))
        }
        Route::EngineReload => {
            // 显式创建/重建引擎（§4.2、§7.6）：无 body = M2 的"按当前文件重建"；
            // 带 `{"provider": …}` = M3 的显式 provider 设置应用（含 Rebuilding 序列）。
            let body = read_body(request, &admitted)?;
            let provider = parse_provider_body(body)?;
            // **委派（两种形态都委派）**：建会话要排空在跑的推理、再建（最多两次）会话。
            // 在 accept 线程上执行会让整个服务（包括 `/api/status` 与 `POST /api/ocr`）
            // 在它结束前停摆，而"`Loading`/`Rebuilding` 可见""新请求入队而不是被拒绝"
            // 正是要观察的行为（评审 P2-2：无 body 的那一支以前漏了这一步）。
            Ok(Dispatch::EngineWork(EngineWork::Reload(provider)))
        }
        Route::Job(id) => Ok(Dispatch::Respond(
            json_body(200, shared.job_view(id)?)?,
            Vec::new(),
        )),
        Route::JobResult(id) => Ok(Dispatch::Respond(shared.job_result(id)?, Vec::new())),
        Route::JobCancel(id) => Ok(Dispatch::Respond(
            json_body(200, shared.cancel_job(id)?)?,
            Vec::new(),
        )),
        Route::JobAnnotated(id) => Ok(Dispatch::Respond(shared.annotated_png(id)?, Vec::new())),
        Route::JobExport(id) => {
            let Some(format) = export_format(query_param(query, "format")) else {
                // 未知/缺失的 `format` 是协议级错误（§9.5 只定义了三个取值）。
                return Err(ServeError::BadRequest);
            };
            let body = shared.export(id, format)?;
            // §9.5 第 2 条：导出以**附件**形式返回，并带**独立的导出 CSP**。
            // CSP 只加在 HTML 上：另外两种格式不是可渲染的文档，给它们加 CSP 没有意义。
            let mut headers = vec![(
                "Content-Disposition".to_string(),
                content_disposition(id, format.extension()),
            )];
            if format == ExportFormat::Html {
                headers.push((
                    "Content-Security-Policy".to_string(),
                    EXPORT_CSP.to_string(),
                ));
            }
            Ok(Dispatch::Respond(body, headers))
        }
    }
}

/// 在独立线程里执行一次"引擎工作"序列，并由那个线程写响应（M2 的按当前文件重建、
/// M3 的 provider 切换、评审 P2-2 的无 body reload、A2 的 `POST /api/models/reverify`
/// 都走这一条路径）。
///
/// - 同一时刻只允许一个引擎序列（[`ServeShared::begin_provider_switch`]）：第二个请求
///   立刻得到 503 `busy`，不排队。**A2 的单飞就是这一条**：`/api/models/reverify` 与
///   `/api/engine/reload` 互相排斥，因此不存在"两个清缓存/两次重建"交错；
/// - 线程创建失败 / 已有序列在跑 → 把请求连同错误原样还给调用方
///   （`Err(Box::new((error, request)))`，装箱只为不让 `Result` 的 `Err` 变体过大），
///   由它写出对应的错误响应，绝不留下"没有响应的连接"；
/// - **accept 线程立刻回到循环**（`/api/status` 与 `POST /api/ocr` 在整个序列期间照常可用），
///   而客户端仍然**只在序列结束（或失败）之后**才拿到响应——三种形态在这一点上完全一致。
fn spawn_engine_work(
    shared: &Arc<ServeShared>,
    work: EngineWork,
    request: Request,
) -> Result<(), Box<(ServeError, Request)>> {
    let Some(guard) = shared.begin_provider_switch() else {
        return Err(Box::new((ServeError::Busy, request)));
    };
    let shared = Arc::clone(shared);
    // 请求对象经一条容量 1 的 channel 交给新线程：**不**把它 move 进闭包，
    // 因此线程创建失败时它还在这里，可以由调用方写出错误响应。
    let (tx, rx) = std::sync::mpsc::sync_channel::<Request>(1);
    let spawned = thread::Builder::new()
        .name("serve-engine-work".to_string())
        .spawn(move || {
            // 资格在线程退出时释放（含 panic）。
            let _guard = guard;
            let Ok(request) = rx.recv() else {
                return;
            };
            let result = match work {
                EngineWork::Reload(provider) => shared.reload_engine(provider),
                EngineWork::ReverifyModels => shared.reverify_models(),
            };
            let body = match result {
                Ok(value) => match serde_json::to_vec(&value) {
                    Ok(bytes) => Body::json(200, bytes),
                    Err(_) => Body::error(&ServeError::Internal),
                },
                Err(error) => Body::error(&error),
            };
            let _ = respond(request, body, &[]);
        });
    match spawned {
        Ok(_) => {
            // 容量 1 且接收端已经在 `recv`，因此这次发送不会阻塞。
            let _ = tx.send(request);
            Ok(())
        }
        Err(error) => {
            eprintln!("serve: cannot start the engine-work thread: {error}");
            Err(Box::new((ServeError::Internal, request)))
        }
    }
}

/// 在独立线程里执行一次评估，并由那个线程写响应（M4；见 `evaluate.rs` 的模块文档）。
///
/// 与 [`spawn_provider_switch`] 同一形状：
///
/// - 同一时刻只允许一个评估（[`ServeShared::begin_evaluation`]）：第二个请求立刻得到
///   503 `busy`，**不排队**；
/// - 线程创建失败 / 已有评估在跑 → 把请求连同错误原样还给调用方，由它写出错误响应，
///   绝不留下"没有响应的连接"；
/// - 断言与 `/api/models` 同源同值的错误（模型缺失 409 / 引擎 503）由那个线程产出。
fn spawn_evaluation(
    shared: &Arc<ServeShared>,
    manifest: PathBuf,
    request: Request,
) -> Result<(), Box<(ServeError, Request)>> {
    let Some(guard) = shared.begin_evaluation() else {
        return Err(Box::new((ServeError::Busy, request)));
    };
    let shared = Arc::clone(shared);
    let (tx, rx) = std::sync::mpsc::sync_channel::<Request>(1);
    let spawned = thread::Builder::new()
        .name("serve-evaluate".to_string())
        .spawn(move || {
            // 资格在线程退出时释放（含 panic）。
            let _guard = guard;
            let Ok(request) = rx.recv() else {
                return;
            };
            let body = match evaluate::run(&shared, &manifest) {
                Ok(value) => match serde_json::to_vec(&value) {
                    Ok(bytes) => Body::json(200, bytes),
                    Err(_) => Body::error(&ServeError::Internal),
                },
                // 与 `/api/ocr` **同一份**错误映射（含 409 的模型清单）：
                // `ServeShared::error_body` 是唯一实现。
                Err(error) => shared.error_body(&error),
            };
            let _ = respond(request, body, &[]);
        });
    match spawned {
        Ok(_) => {
            let _ = tx.send(request);
            Ok(())
        }
        Err(error) => {
            eprintln!("serve: cannot start the evaluation thread: {error}");
            Err(Box::new((ServeError::Internal, request)))
        }
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
///
/// 借用 `Admit` 而不是取走它：第 4 步预留的队列槽位必须留到 `submit_ocr` 才提交，
/// 中途任何失败都让它在 `dispatch` 返回时被丢弃（= 归还容量）。
fn read_body(request: &mut Request, admitted: &admit::Admit) -> Result<Vec<u8>, ServeError> {
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

/// `POST /api/engine/reload` 的可选请求体（§4.2、§7.5）。
///
/// - **空 body**（M1/M2 的形状）：`None` → "按磁盘上的当前文件重建会话"；
/// - `{"provider":"cpu|directml|cuda"}`：显式设置应用。
///
/// 与 `parse_set_id` 同一种白名单式校验：对象里**恰好**一个 `provider` 键，任何额外键
/// （包括 `allow_provider_fallback`——它是 CLI 开关，不允许从 API 改）都会被拒绝，
/// 而不是被忽略。
fn parse_provider_body(bytes: Vec<u8>) -> Result<Option<ProviderPreference>, ServeError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| ServeError::BadRequest)?;
    let object = value.as_object().ok_or(ServeError::BadRequest)?;
    if object.len() != 1 {
        return Err(ServeError::BadRequest);
    }
    let raw = object
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .ok_or(ServeError::BadRequest)?;
    // 与 CLI 的 `--provider` 复用**同一个**映射（含单设备场景的 `device_id = 0`），
    // 不在这里另写一套取值。
    let provider = match raw.to_ascii_lowercase().as_str() {
        "cpu" => ProviderChoice::Cpu,
        "directml" => ProviderChoice::Directml,
        "cuda" => ProviderChoice::Cuda,
        _ => return Err(ServeError::BadRequest),
    };
    Ok(Some(provider.preference()))
}

/// `?format=` 的三个取值（§9.5）。
fn export_format(raw: Option<&str>) -> Option<ExportFormat> {
    match raw? {
        "json" => Some(ExportFormat::Json),
        "md" => Some(ExportFormat::Markdown),
        "html" => Some(ExportFormat::Html),
        _ => None,
    }
}

/// 写出 `Content-Disposition: attachment; filename="…"`（§9.5）。
fn content_disposition(job_id: &str, extension: &str) -> String {
    format!("attachment; filename=\"ocr-{job_id}.{extension}\"")
}

/// 路由表：路径 → 路由；方法不匹配 → `MethodNotAllowed`。
fn route_of(method: HttpMethod, path: &str) -> (RouteDecision, Option<Route>) {
    let candidate = match path {
        "/" => Some(Route::Page),
        "/api/status" => Some(Route::Status),
        "/api/models" => Some(Route::Models),
        "/api/models/download" => Some(Route::ModelsDownload),
        "/api/models/reverify" => Some(Route::ModelsReverify),
        "/api/engine/reload" => Some(Route::EngineReload),
        "/api/ocr" => Some(Route::Ocr),
        "/api/evaluate" => Some(Route::Evaluate),
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

/// `/api/jobs/{id}`、`/api/jobs/{id}/result`、`/api/jobs/{id}/cancel`、
/// `/api/jobs/{id}/annotated.png`、`/api/jobs/{id}/export`。
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
        (Some("annotated.png"), None) => Some(Route::JobAnnotated(id.to_string())),
        (Some("export"), None) => Some(Route::JobExport(id.to_string())),
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
        EXPORT_CSP, HttpMethod, Route, RouteDecision, check_json_content_type, content_disposition,
        export_format, page_csp, parse_max_side, parse_provider_body, parse_set_id, query_param,
        route_of, split_url,
    };
    use crate::serve::error::ServeError;
    use crate::serve::export::ExportFormat;
    use rapid_ocr_rs::ProviderPreference;

    #[test]
    fn the_router_matches_the_documented_endpoint_set_and_nothing_else() {
        let cases = [
            (HttpMethod::Get, "/", Route::Page),
            (HttpMethod::Get, "/api/status", Route::Status),
            (HttpMethod::Get, "/api/models", Route::Models),
            (
                HttpMethod::Post,
                "/api/models/download",
                Route::ModelsDownload,
            ),
            (
                HttpMethod::Post,
                "/api/models/reverify",
                Route::ModelsReverify,
            ),
            (HttpMethod::Post, "/api/engine/reload", Route::EngineReload),
            (HttpMethod::Post, "/api/ocr", Route::Ocr),
            (HttpMethod::Post, "/api/evaluate", Route::Evaluate),
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
            // M3 的两个端点现在是真实路由（不再是 404）。
            (
                HttpMethod::Get,
                "/api/jobs/job-1/annotated.png",
                Route::JobAnnotated("job-1".into()),
            ),
            (
                HttpMethod::Get,
                "/api/jobs/job-1/export",
                Route::JobExport("job-1".into()),
            ),
        ];
        for (method, path, expected) in cases {
            let (decision, route) = route_of(method, path);
            assert_eq!(decision, RouteDecision::Matched, "{path}");
            assert_eq!(route.as_ref(), Some(&expected), "{path}");
        }

        // 不存在的路径仍然 404（`?format=` 属于查询串，路由只看路径）。
        for path in [
            "/api/models/download/cancel",
            "/api/jobs/job-1/annotated",
            "/api/jobs/job-1/annotated.png/extra",
            "/api/jobs/bogus/export/data",
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
        let (decision, route) = route_of(HttpMethod::Get, "/api/evaluate");
        assert_eq!(decision, RouteDecision::MethodNotAllowed);
        assert_eq!(route.expect("known path").allow(), "POST");
        let (decision, route) = route_of(HttpMethod::Post, "/api/status");
        assert_eq!(decision, RouteDecision::MethodNotAllowed);
        assert_eq!(route.expect("known path").allow(), "GET");
        for path in ["/api/jobs/job-1/annotated.png", "/api/jobs/job-1/export"] {
            let (decision, route) = route_of(HttpMethod::Post, path);
            assert_eq!(decision, RouteDecision::MethodNotAllowed, "{path}");
            assert_eq!(route.expect("known path").allow(), "GET");
        }

        // HEAD 只对 GET 路由放行。
        assert_eq!(route_of(HttpMethod::Head, "/").0, RouteDecision::Matched);
        assert_eq!(
            route_of(HttpMethod::Head, "/api/ocr").0,
            RouteDecision::MethodNotAllowed
        );
    }

    /// 只有 §9.5 的三个格式被接受；缺失或未知取值都是 400（不是"猜一个默认值"）。
    #[test]
    fn only_the_three_documented_export_formats_are_accepted() {
        assert_eq!(export_format(Some("json")), Some(ExportFormat::Json));
        assert_eq!(export_format(Some("md")), Some(ExportFormat::Markdown));
        assert_eq!(export_format(Some("html")), Some(ExportFormat::Html));
        for bad in [None, Some(""), Some("JSON"), Some("pdf"), Some("mdx")] {
            assert_eq!(export_format(bad), None, "{bad:?}");
        }
        assert_eq!(ExportFormat::Json.extension(), "json");
        assert_eq!(ExportFormat::Markdown.extension(), "md");
        assert_eq!(ExportFormat::Html.extension(), "html");
        assert_eq!(
            ExportFormat::Html.content_type(),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_disposition("job-1", ExportFormat::Html.extension()),
            "attachment; filename=\"ocr-job-1.html\""
        );
    }

    /// `POST /api/engine/reload` 的可选请求体：空 body = M2 的重建，带 provider = M3 的设置应用。
    #[test]
    fn the_reload_body_carries_at_most_a_provider() {
        assert_eq!(parse_provider_body(Vec::new()).expect("no body"), None);
        assert_eq!(
            parse_provider_body(br#"{"provider":"cpu"}"#.to_vec()).expect("cpu"),
            Some(ProviderPreference::Cpu)
        );
        assert_eq!(
            parse_provider_body(br#"{"provider":" directml "}"#.to_vec()).expect("directml"),
            Some(ProviderPreference::DirectMl { device_id: 0 })
        );
        assert_eq!(
            parse_provider_body(br#"{"provider":"CUDA"}"#.to_vec()).expect("cuda"),
            Some(ProviderPreference::Cuda { device_id: 0 })
        );
        for bad in [
            &br#"{"provider":"tpu"}"#[..],
            &br#"{"provider":null}"#[..],
            &br#"{"provider":""}"#[..],
            &br#"{"provider":"cpu","allow_provider_fallback":true}"#[..],
            &br#"{"other":"cpu"}"#[..],
            &br#"{}"#[..],
            &br#"[]"#[..],
            &br#"not json"#[..],
        ] {
            assert!(
                matches!(
                    parse_provider_body(bad.to_vec()),
                    Err(ServeError::BadRequest)
                ),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    /// §9.5 第 2 条：导出 CSP 逐字冻结，且与主页面 CSP 是两条不同的策略。
    #[test]
    fn the_export_csp_is_the_documented_one_and_differs_from_the_page_csp() {
        assert_eq!(
            EXPORT_CSP,
            "default-src 'none'; style-src 'unsafe-inline'; img-src data:; script-src 'none'; \
             sandbox"
        );
        assert!(EXPORT_CSP.contains("script-src 'none'"), "{EXPORT_CSP}");
        assert!(EXPORT_CSP.contains("img-src data:"), "{EXPORT_CSP}");
        assert!(!EXPORT_CSP.contains("nonce"), "{EXPORT_CSP}");
        // 主页面仍然不允许内联样式；`unsafe-inline` 只出现在导出这一条响应上。
        let page = page_csp("abc");
        assert!(!page.contains("unsafe-inline"), "{page}");
        assert_ne!(page, EXPORT_CSP);
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
