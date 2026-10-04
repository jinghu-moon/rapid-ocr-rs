//! 准入顺序与有界读取（§4.4、§4.6、§12 的"准入参数边界"与"请求体"两行）。
//!
//! # 为什么顺序本身是安全要求（§4.4）
//!
//! "拒绝前读入大 body"会让一个恶意页面用很小的成本消耗本机内存与带宽。
//! 因此判定顺序被冻结成：
//!
//! 1. 方法与路径匹配 → 否则 404/405（由 M1 的路由层给出结论）；
//! 2. token 校验 → 401；
//! 3. `Host` → 421；`Origin`（仅 `POST/PUT/DELETE`）→ 403；
//! 4. **队列容量预检 → 满则 503（此时尚未读取请求体）**；
//! 5. `Content-Length` 预检 → 413；
//! 6. 有界流式读取：总字节上限 + 读取超时（chunked 同样受这两条约束）；
//! 7. 仅在以上全部通过后才解码与建任务。
//!
//! [`admit`] 覆盖第 1–5 步并返回 [`Admit`]（"允许读取"），第 6 步是
//! [`read_body`]，第 7 步的入口是 [`RequestDescriptor::check_content_type`]。
//!
//! 第 1 步的结论由 M1 的路由层传入（M0c **不**实现路由表）：`ServeError` 里没有
//! 404/405 变体，§11.1 也没定义对应 `code`，因此本模块不伪造一个，
//! 而是把路由结论原样回传（[`AdmissionError::Route`]）。

use std::fmt;

use super::error::ServeError;
use super::security::LocalOrigin;

/// HTTP 方法（只需要区分"是否会改变状态"，以及 405 时的展示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Head,
    Post,
    Put,
    Delete,
    /// 其他方法（`OPTIONS`、`TRACE` 等）：按"不改变状态"处理，由路由层决定 405。
    Other,
}

impl HttpMethod {
    /// 大小写不敏感解析（HTTP 方法名是大小写敏感的，但对本机工具按不敏感处理更宽容）。
    pub fn parse(raw: &str) -> Self {
        match raw.to_ascii_uppercase().as_str() {
            "GET" => Self::Get,
            "HEAD" => Self::Head,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "DELETE" => Self::Delete,
            _ => Self::Other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Other => "OTHER",
        }
    }

    /// §7.2：`POST/PUT/DELETE` 必须带 `Origin`。
    pub fn is_state_changing(self) -> bool {
        matches!(self, Self::Post | Self::Put | Self::Delete)
    }
}

/// §4.4 第 1 步的结论（由 M1 的路由层给出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteDecision {
    /// 方法与路径都匹配。
    Matched,
    /// 没有任何路由匹配该路径。
    NotFound,
    /// 路径匹配但方法不允许。
    MethodNotAllowed,
}

impl RouteDecision {
    pub fn name(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::NotFound => "not_found",
            Self::MethodNotAllowed => "method_not_allowed",
        }
    }
}

/// 一次请求的判定输入（纯数据，不含任何 HTTP 库类型）。
#[derive(Debug, Clone, Copy)]
pub struct RequestDescriptor<'a> {
    pub method: HttpMethod,
    pub path: &'a str,
    /// 路由层的结论（第 1 步）。
    pub route: RouteDecision,
    /// token 是否通过（比较由 [`super::security::ServeToken::matches`] 完成）。
    pub has_token: bool,
    pub host: Option<&'a str>,
    pub origin: Option<&'a str>,
    /// 第 4 步的队列准入（**判定即预留**；此时**没有**读取请求体）。
    pub queue: QueueAdmission<'a>,
    /// `Content-Length`（chunked 请求没有）。
    pub content_length: Option<u64>,
    /// 判定时已经在 socket/缓冲区里的 body 字节数（tiny_http 可能已预读一部分 chunked 体）。
    pub body_so_far: u64,
    pub content_type: Option<&'a str>,
    /// 是否为 `Transfer-Encoding: chunked`。
    pub is_chunked: bool,
}

/// §4.4 第 4 步的队列准入。
///
/// **判定与预留必须是同一次加锁**（评审 P2-1 的根因）：M1 的实现先读一个 `queue_full`
/// 布尔值（第一个临界区），通过之后才在 `submit_ocr` 里入队（第二个临界区）。两个并发
/// 请求因此可以都通过预检、都读入大 body，然后其中一个才拿到 503——"拒绝时没有读 body"
/// 这条保证在并发下不成立。
///
/// 这里把"检查"与"占位"合成一个动作：[`QueueSlots::reserve`] 在服务端的**同一把锁**下
/// 判定并占位，返回的 [`QueueReservation`] 要么被提交成真正的队列条目，要么在请求
/// 失败/中止时释放。
#[derive(Clone, Copy)]
pub enum QueueAdmission<'a> {
    /// 该路由不进 OCR 队列（例如 `POST /api/engine/reload`）。
    NotQueued,
    /// 由服务端裁决：第 4 步调用 [`QueueSlots::reserve`]。
    Reserve(&'a dyn QueueSlots),
}

impl fmt::Debug for QueueAdmission<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotQueued => f.write_str("NotQueued"),
            Self::Reserve(_) => f.write_str("Reserve(<server queue>)"),
        }
    }
}

impl QueueAdmission<'_> {
    /// 第 4 步：预留一个槽位；队列已满（含其它请求**已经预留但尚未提交**的槽位）时 `None`。
    fn reserve(&self) -> Option<QueueReservation> {
        match self {
            Self::NotQueued => None,
            Self::Reserve(slots) => slots.reserve(),
        }
    }

    /// 该路由是否需要队列容量（拒绝时用 503 而不是放行）。
    fn is_queued(&self) -> bool {
        matches!(self, Self::Reserve(_))
    }
}

/// 队列槽位的来源（生产实现：[`super::server::ServeShared::reserve_queue_slot`]）。
pub trait QueueSlots {
    /// 原子地预留一个槽位；没有容量时返回 `None`。
    fn reserve(&self) -> Option<QueueReservation>;
}

/// 一个**已经预留、尚未提交**的队列槽位。
///
/// - 提交：`ServeShared::submit_ocr` 入队成功之后释放它（入队在先、释放在后，
///   因此"多算一格"只会让并发请求被保守拒绝，绝不超卖容量）；
/// - 释放：`Drop` 无条件归还槽位——请求被后续步骤拒绝、读 body 失败、连接中止、
///   panic 展开都会走到这里，容量不会泄漏。
pub struct QueueReservation {
    /// `None` = 已经被提交（或已经释放）。`Drop` 只在它还持有动作时归还。
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl QueueReservation {
    /// 由一个释放动作构造（**唯一**的生产构造点在 `ServeShared::reserve_queue_slot`）。
    pub fn new(release: impl FnOnce() + Send + 'static) -> Self {
        Self {
            release: Some(Box::new(release)),
        }
    }
}

impl Drop for QueueReservation {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

impl fmt::Debug for QueueReservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "QueueReservation({})",
            if self.release.is_some() {
                "pending"
            } else {
                "released"
            }
        )
    }
}

impl RequestDescriptor<'_> {
    /// 第 7 步：准入全部通过后才谈解码，这里先校验媒体类型。
    pub fn check_content_type(&self) -> Result<(), ServeError> {
        check_content_type(self.content_type)
    }
}

/// 准入失败。
///
/// 只派生 `Debug`：`ServeError::Ocr` 里保留的库错误既不可 `Clone` 也不可比较
/// （`RapidOcrError` 没有实现它们），而准入判定不需要"相等"语义——
/// 状态码与 `code` 才是被断言的对象。
#[derive(Debug)]
pub enum AdmissionError {
    /// 第 1 步未命中：状态码由 M1 的路由层决定（404/405），M0c 不伪造 `code`。
    Route {
        path: String,
        method: HttpMethod,
        decision: RouteDecision,
    },
    /// 第 2–5 步的失败：**就是** §11.1 的错误。
    Rejected(ServeError),
}

impl AdmissionError {
    /// 只有 [`AdmissionError::Rejected`] 才对应 `ServeError` 的状态码。
    pub fn rejected(&self) -> Option<&ServeError> {
        match self {
            Self::Rejected(error) => Some(error),
            Self::Route { .. } => None,
        }
    }
}

impl From<ServeError> for AdmissionError {
    fn from(error: ServeError) -> Self {
        Self::Rejected(error)
    }
}

/// 准入通过：允许读取请求体。
///
/// `reservation` 是第 4 步**已经预留**的队列槽位（只有需要队列的路由才有）。
/// 它随本值一路传到 `submit_ocr`：提交 = 槽位变成真正的队列条目，丢弃 = 归还容量。
#[derive(Debug)]
pub struct Admit {
    /// 允许读取的最大字节数。
    pub max_body: u64,
    /// 声明的 body 长度；chunked 或没有 `Content-Length` 时为 `None`。
    pub expected_body: Option<u64>,
    /// 第 4 步预留的槽位（非队列路由为 `None`）。
    pub reservation: Option<QueueReservation>,
}

/// 按 §4.4 的顺序判定一次请求。
pub fn admit(
    descriptor: &RequestDescriptor<'_>,
    policy: &LocalOrigin,
    max_body: u64,
) -> Result<Admit, AdmissionError> {
    // 1. 方法与路径。
    if descriptor.route != RouteDecision::Matched {
        return Err(AdmissionError::Route {
            path: descriptor.path.to_string(),
            method: descriptor.method,
            decision: descriptor.route,
        });
    }

    // 2. token。
    if !descriptor.has_token {
        return Err(AdmissionError::Rejected(ServeError::Unauthorized));
    }

    // 3. Host 与 Origin（Origin 只对会改变状态的方法强制）。
    policy.check_host(descriptor.host)?;
    policy.check_origin(descriptor.origin, descriptor.method.is_state_changing())?;

    // 4. 队列容量——**在读 body 之前**，而且是**判定即预留**：
    //    满则直接回 Busy 让连接关闭；通过则凭据随 `Admit` 一路走到入队。
    //    第 5 步失败时这个凭据在 `admit` 返回前被丢弃 → 容量归还（不会泄漏）。
    let reservation = match descriptor.queue.is_queued() {
        true => match descriptor.queue.reserve() {
            Some(reservation) => Some(reservation),
            None => return Err(AdmissionError::Rejected(ServeError::Busy)),
        },
        false => None,
    };

    // 5. Content-Length 预检（chunked 没有 Content-Length，走第 6 步的流式上限）。
    let expected_body = if descriptor.is_chunked {
        None
    } else {
        descriptor.content_length
    };
    if descriptor.body_so_far > max_body {
        // 判定时缓冲区里已经超过上限（chunked 体尤其可能）：同样先拒绝，不再继续读。
        return Err(AdmissionError::Rejected(ServeError::PayloadTooLarge));
    }
    if let Some(length) = expected_body {
        if length > max_body {
            return Err(AdmissionError::Rejected(ServeError::PayloadTooLarge));
        }
        if descriptor.body_so_far > length {
            // 声明长度小于实际已到达的字节数：请求自相矛盾。
            return Err(AdmissionError::Rejected(ServeError::BadRequest));
        }
    }

    Ok(Admit {
        max_body,
        expected_body,
        reservation,
    })
}

/// 有界读取的累计账本（§4.4 第 6 步的"总字节上限"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyBudget {
    max_body: u64,
    received: u64,
}

impl BodyBudget {
    pub fn new(max_body: u64) -> Self {
        Self {
            max_body,
            received: 0,
        }
    }

    pub fn max_body(&self) -> u64 {
        self.max_body
    }

    pub fn received(&self) -> u64 {
        self.received
    }

    pub fn remaining(&self) -> u64 {
        self.max_body.saturating_sub(self.received)
    }

    /// 记录一块数据。总长度超过上限立即返回 413，**不写入、不截断**（截断会掩盖超限）。
    pub fn accept(&mut self, chunk_len: u64) -> Result<(), ServeError> {
        if chunk_len > self.remaining() {
            return Err(ServeError::PayloadTooLarge);
        }
        // `received <= max_body` 是上面的不变量，因此这里不会溢出。
        self.received += chunk_len;
        Ok(())
    }

    /// 收尾校验：实际长度必须与声明的 `Content-Length` 一致（§12 的"与实际不符"）。
    pub fn finish(&self, expected: Option<u64>) -> Result<(), ServeError> {
        if let Some(expected) = expected
            && self.received != expected
        {
            return Err(ServeError::BadRequest);
        }
        Ok(())
    }
}

/// 一次按块读取的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkOutcome {
    Data(Vec<u8>),
    End,
    /// 连接/读取失败（错误文本由 M1 记入日志；`ServeError::BadRequest` 不携带载荷）。
    Failed(String),
    /// 在读取超时内没有拿到数据。
    TimedOut,
}

/// 请求体来源。
///
/// M1 用 `tiny_http` 的 reader 实现它；超时判断由实现方按**注入的时间**给出，
/// 因此限流、超时、长度校验三件事都能脱离 HTTP 库单测（§4.4 第 6 步）。
pub trait BodySource {
    /// 读取是否已经超时。
    fn read_timed_out(&self) -> bool;
    /// 取下一块数据。
    fn next_chunk(&mut self) -> ChunkOutcome;
}

/// 有界流式读取：总字节上限 + 读取超时（§4.4 第 6 步）。
///
/// 返回完整 body；任何超限都不返回部分数据。
pub fn read_body(
    source: &mut dyn BodySource,
    expected: Option<u64>,
    max_body: u64,
) -> Result<Vec<u8>, ServeError> {
    let mut budget = BodyBudget::new(max_body);
    let reserve = expected.unwrap_or(0).min(max_body);
    let mut body = Vec::with_capacity(usize::try_from(reserve).unwrap_or(0));
    loop {
        if source.read_timed_out() {
            return Err(ServeError::RequestTimeout);
        }
        match source.next_chunk() {
            ChunkOutcome::End => break,
            ChunkOutcome::TimedOut => return Err(ServeError::RequestTimeout),
            ChunkOutcome::Failed(_) => return Err(ServeError::BadRequest),
            ChunkOutcome::Data(chunk) => {
                if let Err(error) = budget.accept(chunk.len() as u64) {
                    // 记录账本口径（§4.4 第 6 步）：运维要能看出是"哪一侧"超限，
                    // 而不是只看到一句 413。
                    eprintln!(
                        "serve: request body rejected after {} of at most {} byte(s)",
                        budget.received(),
                        budget.max_body()
                    );
                    return Err(error);
                }
                body.extend_from_slice(&chunk);
            }
        }
    }
    budget.finish(expected)?;
    Ok(body)
}

/// §4.4 第 7 步的媒体类型校验：body 是**原始字节**（`application/octet-stream`）或图片。
///
/// 缺省（没有 `Content-Type`）按原始体处理：本机工具不强制客户端声明媒体类型。
/// `multipart/form-data` 与 `application/x-www-form-urlencoded` 一律拒绝——
/// 本服务**不解析** multipart（§2.2 的"避免 multipart 解析依赖"）。
pub fn check_content_type(content_type: Option<&str>) -> Result<(), ServeError> {
    let Some(raw) = content_type else {
        return Ok(());
    };
    let essence = raw
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let accepted = essence == "application/octet-stream" || essence.starts_with("image/");
    if accepted && !essence.is_empty() {
        return Ok(());
    }
    Err(ServeError::BadRequest)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        AdmissionError, BodyBudget, BodySource, ChunkOutcome, HttpMethod, QueueAdmission,
        QueueReservation, QueueSlots, RequestDescriptor, RouteDecision, admit, check_content_type,
        read_body,
    };
    use crate::serve::error::ServeError;
    use crate::serve::security::LocalOrigin;

    const MAX_BODY: u64 = 1024;

    fn policy() -> LocalOrigin {
        LocalOrigin::for_port(8760)
    }

    /// 一个**总是满**的队列：第 4 步必须在这里拒绝，且**不读 body**。
    ///
    /// 预留次数是这台测试替身的唯一状态：它同时证明"检查与占位是同一个动作"
    /// （`admit` 一返回错误，就没有任何凭据留在外面）。
    struct FullQueue {
        attempts: AtomicUsize,
    }

    impl FullQueue {
        fn new() -> Self {
            Self {
                attempts: AtomicUsize::new(0),
            }
        }
    }

    impl QueueSlots for FullQueue {
        fn reserve(&self) -> Option<QueueReservation> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            None
        }
    }

    /// 一个**有空位**的队列：预留必须被交到 `Admit.reservation` 上，
    /// 且释放动作必须真的执行（计数是它的唯一状态）。
    struct FreeQueue {
        released: std::sync::Arc<AtomicUsize>,
    }

    impl FreeQueue {
        fn new() -> Self {
            Self {
                released: std::sync::Arc::new(AtomicUsize::new(0)),
            }
        }

        fn released(&self) -> usize {
            self.released.load(Ordering::SeqCst)
        }
    }

    impl QueueSlots for FreeQueue {
        fn reserve(&self) -> Option<QueueReservation> {
            let released = std::sync::Arc::clone(&self.released);
            Some(QueueReservation::new(move || {
                released.fetch_add(1, Ordering::SeqCst);
            }))
        }
    }

    /// 一份"全部合法"的请求：每个用例只破坏其中一项。
    fn ok_descriptor() -> RequestDescriptor<'static> {
        RequestDescriptor {
            method: HttpMethod::Post,
            path: "/api/ocr",
            route: RouteDecision::Matched,
            has_token: true,
            host: Some("127.0.0.1:8760"),
            origin: Some("http://127.0.0.1:8760"),
            queue: QueueAdmission::NotQueued,
            content_length: Some(128),
            body_so_far: 0,
            content_type: Some("application/octet-stream"),
            is_chunked: false,
        }
    }

    #[test]
    fn a_fully_valid_request_is_admitted() {
        let admitted = admit(&ok_descriptor(), &policy(), MAX_BODY).expect("admitted");
        assert_eq!(admitted.max_body, MAX_BODY);
        assert_eq!(admitted.expected_body, Some(128));
        assert!(
            admitted.reservation.is_none(),
            "a route that does not use a queue must not hold a slot"
        );
    }

    /// 评审 P2-1：`admit` 在**第 4 步**就预留槽位，凭据随 `Admit` 交给调用方；
    /// 凭据被丢弃即归还容量（这里是"准入之后就没有再提交"的最短路径）。
    #[test]
    fn a_queue_admission_reserves_a_slot_and_releases_it_on_drop() {
        let queue = FreeQueue::new();
        let mut descriptor = ok_descriptor();
        descriptor.queue = QueueAdmission::Reserve(&queue);
        let admitted = admit(&descriptor, &policy(), MAX_BODY).expect("admitted");
        assert!(admitted.reservation.is_some(), "a slot must be reserved");
        assert_eq!(queue.released(), 0);
        drop(admitted);
        assert_eq!(
            queue.released(),
            1,
            "dropping an uncommitted reservation must return the slot"
        );
    }

    /// §4.4 的顺序表：逐个破坏前面的条件，断言"最先失败的那一步"赢。
    #[test]
    fn the_admission_order_is_enforced_step_by_step() {
        let full = FullQueue::new();
        let full_queue = QueueAdmission::Reserve(&full);

        // 1. 路由未命中：即使 token/Host/Origin/队列/长度全都成问题，也先回路由结论。
        let mut descriptor = ok_descriptor();
        descriptor.route = RouteDecision::NotFound;
        descriptor.has_token = false;
        descriptor.host = Some("evil.example:8760");
        descriptor.origin = Some("null");
        descriptor.queue = full_queue;
        descriptor.content_length = Some(u64::MAX);
        match admit(&descriptor, &policy(), MAX_BODY).expect_err("route wins") {
            AdmissionError::Route { decision, path, .. } => {
                assert_eq!(decision, RouteDecision::NotFound);
                assert_eq!(path, "/api/ocr");
            }
            other => panic!("expected a route rejection, got {other:?}"),
        }

        // 405 与 404 都是路由层的结论，同样在 token 之前。
        descriptor.route = RouteDecision::MethodNotAllowed;
        assert!(matches!(
            admit(&descriptor, &policy(), MAX_BODY).expect_err("route wins"),
            AdmissionError::Route {
                decision: RouteDecision::MethodNotAllowed,
                ..
            }
        ));

        // 2. token 先于 Host。
        let mut descriptor = ok_descriptor();
        descriptor.has_token = false;
        descriptor.host = Some("evil.example:8760");
        descriptor.origin = Some("null");
        descriptor.queue = full_queue;
        descriptor.content_length = Some(u64::MAX);
        let error = admit(&descriptor, &policy(), MAX_BODY).expect_err("token wins");
        assert_eq!(error.rejected().expect("a ServeError").status_code(), 401);

        // 3. Host 先于 Origin 与队列容量。
        let mut descriptor = ok_descriptor();
        descriptor.host = Some("evil.example:8760");
        descriptor.origin = Some("null");
        descriptor.queue = full_queue;
        descriptor.content_length = Some(u64::MAX);
        let error = admit(&descriptor, &policy(), MAX_BODY).expect_err("host wins");
        assert_eq!(error.rejected().expect("a ServeError").status_code(), 421);

        // 3b. Origin 先于队列容量与长度。
        let mut descriptor = ok_descriptor();
        descriptor.origin = None;
        descriptor.queue = full_queue;
        descriptor.content_length = Some(u64::MAX);
        let error = admit(&descriptor, &policy(), MAX_BODY).expect_err("origin wins");
        assert_eq!(error.rejected().expect("a ServeError").status_code(), 403);

        // 4. 队列容量先于 Content-Length 预检：**500 MB 的 Content-Length 也不会被读**。
        let mut descriptor = ok_descriptor();
        descriptor.queue = full_queue;
        descriptor.content_length = Some(500 * 1024 * 1024);
        let error = admit(&descriptor, &policy(), MAX_BODY).expect_err("queue wins");
        let error = error.rejected().expect("a ServeError");
        assert_eq!(error.status_code(), 503);
        assert_eq!(error.code(), "busy");
        assert_eq!(
            full.attempts.load(Ordering::SeqCst),
            1,
            "the queue step must run exactly once for this descriptor"
        );

        // 第 5 步失败时，第 4 步已经预留的槽位必须被归还（否则容量会泄漏）。
        let queue = FreeQueue::new();
        let mut descriptor = ok_descriptor();
        descriptor.queue = QueueAdmission::Reserve(&queue);
        descriptor.content_length = Some(MAX_BODY + 1);
        let error = admit(&descriptor, &policy(), MAX_BODY).expect_err("length wins");
        assert_eq!(error.rejected().expect("a ServeError").status_code(), 413);
        assert_eq!(
            queue.released(),
            1,
            "a rejected request must return its reserved slot"
        );

        // 5. 最后才是 Content-Length 预检。
        let mut descriptor = ok_descriptor();
        descriptor.content_length = Some(MAX_BODY + 1);
        let error = admit(&descriptor, &policy(), MAX_BODY).expect_err("length wins");
        let error = error.rejected().expect("a ServeError");
        assert_eq!(error.status_code(), 413);
        assert_eq!(error.code(), "payload_too_large");
    }

    #[test]
    fn content_length_boundaries_are_inclusive() {
        // 恰好等于上限 → 放行。
        let mut descriptor = ok_descriptor();
        descriptor.content_length = Some(MAX_BODY);
        assert_eq!(
            admit(&descriptor, &policy(), MAX_BODY)
                .expect("exactly the limit is allowed")
                .expected_body,
            Some(MAX_BODY)
        );

        // 上限 + 1 → 413。
        descriptor.content_length = Some(MAX_BODY + 1);
        assert!(matches!(
            admit(&descriptor, &policy(), MAX_BODY).expect_err("one byte over"),
            AdmissionError::Rejected(ServeError::PayloadTooLarge)
        ));

        // 0 字节 body 合法（例如 `POST /api/engine/reload`）。
        descriptor.content_length = Some(0);
        assert!(admit(&descriptor, &policy(), MAX_BODY).is_ok());

        // 没有 Content-Length 且不是 chunked → 交给流式上限。
        descriptor.content_length = None;
        let admitted = admit(&descriptor, &policy(), MAX_BODY).expect("admitted");
        assert_eq!(admitted.expected_body, None);
    }

    /// chunked 没有 `Content-Length`：预检跳过，但流式上限仍然适用。
    #[test]
    fn chunked_requests_skip_the_length_pre_check_but_keep_the_stream_cap() {
        let mut descriptor = ok_descriptor();
        descriptor.is_chunked = true;
        descriptor.content_length = Some(u64::MAX); // 谎报的长度不参与判定
        let admitted = admit(&descriptor, &policy(), MAX_BODY).expect("admitted");
        assert_eq!(admitted.expected_body, None);

        let mut source = ScriptedSource::new(vec![
            ChunkOutcome::Data(vec![0; 512]),
            ChunkOutcome::Data(vec![0; 512]),
            ChunkOutcome::Data(vec![0; 1]),
        ]);
        let error = read_body(&mut source, admitted.expected_body, admitted.max_body)
            .expect_err("the stream cap must apply to chunked bodies");
        assert_eq!(error.status_code(), 413);
    }

    #[test]
    fn a_body_already_buffered_beyond_the_limit_is_rejected_before_reading() {
        let mut descriptor = ok_descriptor();
        descriptor.body_so_far = MAX_BODY + 1;
        descriptor.content_length = None;
        assert!(matches!(
            admit(&descriptor, &policy(), MAX_BODY).expect_err("already over"),
            AdmissionError::Rejected(ServeError::PayloadTooLarge)
        ));

        // 声明长度小于已到达字节数 → 请求自相矛盾。
        let mut descriptor = ok_descriptor();
        descriptor.content_length = Some(10);
        descriptor.body_so_far = 11;
        assert!(matches!(
            admit(&descriptor, &policy(), MAX_BODY).expect_err("inconsistent"),
            AdmissionError::Rejected(ServeError::BadRequest)
        ));
    }

    #[test]
    fn read_requests_do_not_need_an_origin_but_still_need_host_and_token() {
        let mut descriptor = ok_descriptor();
        descriptor.method = HttpMethod::Get;
        descriptor.origin = None;
        assert!(admit(&descriptor, &policy(), MAX_BODY).is_ok());

        descriptor.host = None;
        assert_eq!(
            admit(&descriptor, &policy(), MAX_BODY)
                .expect_err("host still required")
                .rejected()
                .expect("ServeError")
                .status_code(),
            421
        );
    }

    #[test]
    fn http_methods_are_classified_for_origin_enforcement() {
        assert_eq!(HttpMethod::parse("POST"), HttpMethod::Post);
        assert_eq!(HttpMethod::parse("post"), HttpMethod::Post);
        assert_eq!(HttpMethod::parse("GET"), HttpMethod::Get);
        assert_eq!(HttpMethod::parse("HEAD"), HttpMethod::Head);
        assert_eq!(HttpMethod::parse("PUT"), HttpMethod::Put);
        assert_eq!(HttpMethod::parse("DELETE"), HttpMethod::Delete);
        assert_eq!(HttpMethod::parse("TRACE"), HttpMethod::Other);
        assert_eq!(HttpMethod::parse(""), HttpMethod::Other);

        for method in [HttpMethod::Post, HttpMethod::Put, HttpMethod::Delete] {
            assert!(method.is_state_changing(), "{method:?}");
        }
        for method in [HttpMethod::Get, HttpMethod::Head, HttpMethod::Other] {
            assert!(!method.is_state_changing(), "{method:?}");
        }
        assert_eq!(HttpMethod::Post.name(), "POST");
    }

    // -----------------------------------------------------------------------
    // 有界读取
    // -----------------------------------------------------------------------

    struct ScriptedSource {
        chunks: VecDeque<ChunkOutcome>,
        timed_out: bool,
        polls: usize,
    }

    impl ScriptedSource {
        fn new(chunks: Vec<ChunkOutcome>) -> Self {
            Self {
                chunks: chunks.into(),
                timed_out: false,
                polls: 0,
            }
        }
    }

    impl BodySource for ScriptedSource {
        fn read_timed_out(&self) -> bool {
            self.timed_out
        }

        fn next_chunk(&mut self) -> ChunkOutcome {
            self.polls += 1;
            self.chunks.pop_front().unwrap_or(ChunkOutcome::End)
        }
    }

    #[test]
    fn body_budget_tracks_the_remaining_allowance_without_overflow() {
        let mut budget = BodyBudget::new(MAX_BODY);
        assert_eq!(budget.remaining(), MAX_BODY);
        budget.accept(MAX_BODY - 1).expect("fits");
        assert_eq!(budget.received(), MAX_BODY - 1);
        assert_eq!(budget.remaining(), 1);
        budget.accept(1).expect("exactly the limit");
        assert_eq!(budget.remaining(), 0);
        assert!(matches!(
            budget.accept(1).expect_err("one byte over"),
            ServeError::PayloadTooLarge
        ));
        assert_eq!(
            budget.received(),
            MAX_BODY,
            "a rejected chunk must not be counted"
        );

        // u64::MAX 上限下也不能溢出。
        let mut huge = BodyBudget::new(u64::MAX);
        huge.accept(u64::MAX - 1).expect("fits");
        assert!(matches!(
            huge.accept(u64::MAX).expect_err("overflowing sum"),
            ServeError::PayloadTooLarge
        ));
        assert_eq!(huge.received(), u64::MAX - 1);
    }

    #[test]
    fn body_budget_finish_detects_a_content_length_mismatch() {
        let mut budget = BodyBudget::new(MAX_BODY);
        budget.accept(100).expect("fits");
        budget.finish(Some(100)).expect("matching length");
        assert!(matches!(
            budget.finish(Some(101)).expect_err("short body"),
            ServeError::BadRequest
        ));
        assert!(matches!(
            budget.finish(Some(99)).expect_err("long body"),
            ServeError::BadRequest
        ));
        budget.finish(None).expect("no declared length to check");
    }

    #[test]
    fn read_body_concatenates_chunks_and_checks_the_declared_length() {
        let mut source = ScriptedSource::new(vec![
            ChunkOutcome::Data(vec![1, 2, 3]),
            ChunkOutcome::Data(vec![]),
            ChunkOutcome::Data(vec![4, 5]),
        ]);
        let body = read_body(&mut source, Some(5), MAX_BODY).expect("read");
        assert_eq!(body, vec![1, 2, 3, 4, 5]);

        // 实际长度少于声明 → 400。
        let mut source = ScriptedSource::new(vec![ChunkOutcome::Data(vec![1, 2, 3])]);
        let error = read_body(&mut source, Some(5), MAX_BODY).expect_err("short body");
        assert_eq!(error.status_code(), 400);

        // 实际长度多于声明：多出来的字节已经越过声明长度 → 400（不能悄悄接受）。
        let mut source = ScriptedSource::new(vec![ChunkOutcome::Data(vec![1, 2, 3, 4, 5, 6])]);
        let error = read_body(&mut source, Some(5), MAX_BODY).expect_err("long body");
        assert_eq!(error.status_code(), 400);
    }

    #[test]
    fn read_body_rejects_a_stream_that_exceeds_the_cap() {
        let mut source = ScriptedSource::new(vec![
            ChunkOutcome::Data(vec![0; MAX_BODY as usize]),
            ChunkOutcome::Data(vec![0]),
        ]);
        let error = read_body(&mut source, None, MAX_BODY).expect_err("over the cap");
        assert_eq!(error.status_code(), 413);
        assert_eq!(error.code(), "payload_too_large");

        // 恰好等于上限仍然通过。
        let mut source = ScriptedSource::new(vec![ChunkOutcome::Data(vec![0; MAX_BODY as usize])]);
        let body = read_body(&mut source, None, MAX_BODY).expect("exactly the cap");
        assert_eq!(body.len(), MAX_BODY as usize);

        // 单个超大 chunk 也不能绕过上限。
        let mut source =
            ScriptedSource::new(vec![ChunkOutcome::Data(vec![0; MAX_BODY as usize + 1])]);
        assert!(read_body(&mut source, None, MAX_BODY).is_err());
    }

    #[test]
    fn read_body_reports_a_read_timeout() {
        let mut source = ScriptedSource {
            chunks: VecDeque::new(),
            timed_out: true,
            polls: 0,
        };
        let error = read_body(&mut source, None, MAX_BODY).expect_err("timed out before any chunk");
        assert_eq!(error.status_code(), 408);
        assert_eq!(error.code(), "request_timeout");
        assert_eq!(
            source.polls, 0,
            "a timed-out read must not touch the source"
        );

        // 读到一半超时：仍然是 408，而不是"部分 body 成功"。
        struct TimeoutAfterOne {
            first: Option<Vec<u8>>,
        }
        impl BodySource for TimeoutAfterOne {
            fn read_timed_out(&self) -> bool {
                self.first.is_none()
            }

            fn next_chunk(&mut self) -> ChunkOutcome {
                match self.first.take() {
                    Some(chunk) => ChunkOutcome::Data(chunk),
                    None => ChunkOutcome::TimedOut,
                }
            }
        }
        let mut source = TimeoutAfterOne {
            first: Some(vec![1, 2, 3]),
        };
        let error = read_body(&mut source, None, MAX_BODY).expect_err("stalled mid-body");
        assert_eq!(error.status_code(), 408);

        // 显式 TimedOut 也同样映射到 408。
        let mut source = ScriptedSource::new(vec![ChunkOutcome::TimedOut]);
        assert_eq!(
            read_body(&mut source, None, MAX_BODY)
                .expect_err("TimedOut")
                .status_code(),
            408
        );
    }

    #[test]
    fn read_body_maps_a_broken_stream_to_bad_request() {
        let mut source = ScriptedSource::new(vec![
            ChunkOutcome::Data(vec![1, 2]),
            ChunkOutcome::Failed("connection reset".to_string()),
        ]);
        let error = read_body(&mut source, None, MAX_BODY).expect_err("broken stream");
        assert_eq!(error.status_code(), 400);
        assert_eq!(error.code(), "bad_request");
    }

    #[test]
    fn read_body_accepts_an_empty_body() {
        let mut source = ScriptedSource::new(vec![ChunkOutcome::End]);
        let body = read_body(&mut source, Some(0), MAX_BODY).expect("an empty body is valid");
        assert!(body.is_empty());
    }

    // -----------------------------------------------------------------------
    // 第 7 步：媒体类型
    // -----------------------------------------------------------------------

    #[test]
    fn only_raw_bodies_and_images_are_accepted_for_decoding() {
        for accepted in [
            None,
            Some("application/octet-stream"),
            Some("APPLICATION/OCTET-STREAM"),
            Some("image/png"),
            Some("image/jpeg; charset=binary"),
        ] {
            assert!(
                check_content_type(accepted).is_ok(),
                "should be accepted: {accepted:?}"
            );
        }

        for rejected in [
            Some("application/x-www-form-urlencoded"),
            Some("multipart/form-data; boundary=----x"),
            Some("text/plain"),
            Some("application/json"),
            Some(""),
            Some("   "),
        ] {
            let error = check_content_type(rejected)
                .expect_err(&format!("should be rejected: {rejected:?}"));
            assert_eq!(error.status_code(), 400, "content type: {rejected:?}");
            assert_eq!(error.code(), "bad_request");
        }

        // 描述符自己也提供同一个入口，避免调用方重新拼判定。
        let mut descriptor = ok_descriptor();
        assert!(descriptor.check_content_type().is_ok());
        descriptor.content_type = Some("multipart/form-data");
        assert!(descriptor.check_content_type().is_err());
    }
}
