//! 仅本机（loopback）安全模型：监听地址、Host/Origin 校验、令牌与静态页注入（§7.1–§7.3、§9.5）。
//!
//! # 硬编码的监听地址（§7.1）
//!
//! 监听地址是**编译期常量** [`LOOPBACK_BIND_ADDRESS`]：[`bind_address`] 只接受端口，
//! 本模块**不读**任何环境变量、也不存在接受地址参数的函数。
//! 上一版的 `--host` 与 Host/Origin 校验自相矛盾（绑定 `192.168.x.x` 后正常浏览器请求
//! 会被自己的校验拒掉），因此整条路径被删除，而不是加一个警告了事。
//!
//! # 为什么允许集合里没有 `[::1]`（§7.1、§12 的 IPv6 行）
//!
//! 服务**只监听 IPv4**，没有监听就不放行：`Host: [::1]:8760` 与
//! `Origin: http://[::1]:8760` 都必须被拒绝（`[::1]` 是"另一个 socket 上的另一个服务"
//! 的地址，放行等于承认一个并不存在的监听端点）。
//!
//! 允许集合由**实际绑定端口**计算（[`LocalOrigin::from_bound`]）：端口是配置项，
//! 集合必须跟着它走，否则换个端口浏览器请求就会被自己的校验拒掉。
//! 端口为 80 时浏览器不再发送 `:80`，因此额外放行不带端口的写法。
//!
//! # 令牌
//!
//! 每进程由**操作系统 CSPRNG** 生成（[`ServeToken::generate`] → `BCryptGenRandom`，
//! 见 [`RANDOM_SOURCE`]），比较使用常量时间实现（[`ServeToken::matches`]）。威胁模型：
//! 本机浏览器里的**恶意页面**可能向 loopback 端口发请求，但拿不到注入到我们页面里的
//! token（受同源策略与 §9.5 的 nonce CSP 保护）。因此 token 是"防跨站触发"的共享密钥，
//! 不承担会话/密码学身份语义（§0.2 明确不引入 cookie）——但它是**不可预测的密钥**，
//! 因此熵必须是密码学安全的：早期实现用时间 + PID + 计数器 + 栈地址派生（并自认不是
//! 密码学 RNG），那对一个可被本机其它进程观察的密钥是不够的。
//!
//! **fail-closed**：CSPRNG 不可用（`BCryptGenRandom` 返回非 0）时 [`ServeToken::generate`]
//! 与 [`generate_nonce`] 返回 [`RandomError`]，`run.rs` 把它变成拒绝启动。不存在
//! "退回弱熵"的分支：一个拿不到密码学随机数的服务不该发出一个看起来正常的令牌。

use std::{
    fmt,
    fmt::Write as _,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use super::error::ServeError;

/// 硬编码的监听地址（IPv4 loopback）。**唯一**定义处。
pub const LOOPBACK_BIND_ADDRESS: &str = "127.0.0.1";

/// 硬编码的 IPv4 loopback 地址。
pub const LOOPBACK_IPV4: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// 由端口得到实际绑定地址。
///
/// 签名里**没有**地址参数：不提供任何可绑定其他地址的入口（§7.1）。
pub fn bind_address(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(LOOPBACK_IPV4), port)
}

/// 实际绑定地址不是硬编码的 loopback 时返回的错误（启动期断言用，§7.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonLoopbackBindError {
    bound: SocketAddr,
}

impl fmt::Display for NonLoopbackBindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "refusing to serve on {}: rapidocr serve only listens on {LOOPBACK_BIND_ADDRESS}; \
             there is deliberately no --host option (docs/05 §7.1)",
            self.bound
        )
    }
}

impl std::error::Error for NonLoopbackBindError {}

/// 启动期断言：实际绑定地址必须**恰好**是硬编码的 loopback（§7.1）。
///
/// 这里不接受整个 `127.0.0.0/8`：允许的 Host 集合只有 `127.0.0.1` 与 `localhost`，
/// 若放行 `127.0.0.2` 就会重现"绑定地址与 Host 校验互相矛盾"的老问题。
pub fn assert_loopback(bound: SocketAddr) -> Result<SocketAddr, NonLoopbackBindError> {
    if bound.ip() == IpAddr::V4(LOOPBACK_IPV4) {
        return Ok(bound);
    }
    Err(NonLoopbackBindError { bound })
}

/// Host / Origin 允许集合（由实际端口计算）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalOrigin {
    port: u16,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
}

impl LocalOrigin {
    /// 由实际端口计算允许集合。IPv4 only：`127.0.0.1` 与 `localhost`。
    pub fn for_port(port: u16) -> Self {
        let mut allowed_hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
        let mut allowed_origins = vec![
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
        ];
        if port == 80 {
            // 80 是 http 的默认端口，浏览器不会再发送 `:80`。
            allowed_hosts.push("127.0.0.1".to_string());
            allowed_hosts.push("localhost".to_string());
            allowed_origins.push("http://127.0.0.1".to_string());
            allowed_origins.push("http://localhost".to_string());
        }
        Self {
            port,
            allowed_hosts,
            allowed_origins,
        }
    }

    /// 从**实际绑定地址**计算（先断言 loopback）：§7.1 要求允许集合含实际端口。
    pub fn from_bound(bound: SocketAddr) -> Result<Self, NonLoopbackBindError> {
        Ok(Self::for_port(assert_loopback(bound)?.port()))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn allowed_hosts(&self) -> &[String] {
        &self.allowed_hosts
    }

    pub fn allowed_origins(&self) -> &[String] {
        &self.allowed_origins
    }

    /// 启动日志用的规范 origin（也是 `--open` 要打开的地址）。
    pub fn primary_origin(&self) -> &str {
        &self.allowed_origins[0]
    }

    /// `Host` 校验：不在允许集合 → 421 `bad_host`（防 DNS rebinding，§7.2）。
    ///
    /// 比较前统一 `trim` + 小写：主机名大小写不敏感，但**不**做任何"补端口""去方括号"
    /// 之类的宽容处理——`[::1]:8760` 必须原样落到拒绝分支。
    pub fn check_host(&self, host: Option<&str>) -> Result<(), ServeError> {
        let Some(host) = host else {
            return Err(ServeError::BadHost);
        };
        let candidate = host.trim().to_ascii_lowercase();
        if self
            .allowed_hosts
            .iter()
            .any(|allowed| allowed == &candidate)
        {
            return Ok(());
        }
        Err(ServeError::BadHost)
    }

    /// `Origin` 校验：**所有** `POST/PUT/DELETE` 都必须带 `Origin` 且等于当前服务 origin；
    /// 缺失、`null`、不匹配 → 403 `bad_origin`（§7.2）。
    ///
    /// 读请求不校验（浏览器不会给同源的 GET 加 `Origin`）。
    pub fn check_origin(
        &self,
        origin: Option<&str>,
        is_state_changing: bool,
    ) -> Result<(), ServeError> {
        if !is_state_changing {
            return Ok(());
        }
        let Some(origin) = origin else {
            return Err(ServeError::BadOrigin);
        };
        let candidate = origin.trim().to_ascii_lowercase();
        if self
            .allowed_origins
            .iter()
            .any(|allowed| allowed == &candidate)
        {
            return Ok(());
        }
        Err(ServeError::BadOrigin)
    }
}

/// 每进程随机令牌（§7.2：客户端用 `X-RapidOCR-Token`；所有 `/api/*` 都需要）。
#[derive(Clone, PartialEq, Eq)]
pub struct ServeToken(String);

impl ServeToken {
    /// 生成本进程的令牌（每次都不同）。
    ///
    /// 熵来自操作系统 CSPRNG；**不可用时不返回一个弱令牌，而是返回错误**，
    /// 由 `run.rs` 转成启动失败（fail-closed，见 [`RandomError`]）。
    pub fn generate() -> Result<Self, RandomError> {
        Ok(Self(random_hex(TOKEN_BYTES)?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 常量时间比较。
    ///
    /// 实现要点：循环次数只取决于**期望值**的长度，越界位置用 0 填充，
    /// 长度差与逐字节差异一起累加到最后一次比较，因此没有按字节提前返回的分支。
    /// （Rust 没有保证"不泄漏时间"的类型，这里是工程上的最优努力，足以消除
    /// "逐字节比较 + 提前返回"这种可被计时区分的最常见写法。）
    pub fn matches(&self, candidate: &str) -> bool {
        let expected = self.0.as_bytes();
        let given = candidate.as_bytes();
        let mut diff = (expected.len() ^ given.len()) as u64;
        for (index, byte) in expected.iter().enumerate() {
            let other = given.get(index).copied().unwrap_or(0);
            diff |= u64::from(byte ^ other);
        }
        diff == 0
    }
}

/// `Debug` 不打印令牌本身：日志里不得出现可直接复用的密钥。
impl fmt::Debug for ServeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ServeToken(<redacted {} hex chars>)", self.0.len())
    }
}

/// 令牌的十六进制长度（256 bit）。
const TOKEN_BYTES: usize = 32;
/// nonce 的十六进制长度（128 bit）。
const NONCE_BYTES: usize = 16;

/// 生成 CSP nonce（§9.5：主页面 CSP 用 nonce，不用 `unsafe-inline`）。
///
/// 与令牌用**同一个** CSPRNG（[`random_hex`]）：nonce 是 CSP 的唯一随机量，
/// 弱 nonce 会让注入脚本成为可能。失败同样向上传播（调用方拒绝启动）。
pub fn generate_nonce() -> Result<String, RandomError> {
    random_hex(NONCE_BYTES)
}

/// 熵源不可用（fail-closed：服务拒绝启动，绝不用弱熵继续）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RandomError {
    /// `BCryptGenRandom` 返回非 0 状态码。
    Bcrypt { status: i32 },
    /// 调用方请求了 0 字节（编程错误，不是运行期状态）。
    Empty,
}

impl fmt::Display for RandomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bcrypt { status } => write!(
                f,
                "BCryptGenRandom (bcrypt.dll) failed with NTSTATUS {status:#x}; rapidocr serve \
                 refuses to start instead of falling back to weak entropy"
            ),
            Self::Empty => write!(f, "cannot generate an empty random string"),
        }
    }
}

impl std::error::Error for RandomError {}

/// 熵源的**名字**（启动日志与测试都读它，因此"用什么产生密钥"永远不是猜测）。
pub const RANDOM_SOURCE: &str =
    "windows:BCryptGenRandom(bcrypt.dll, BCRYPT_USE_SYSTEM_PREFERRED_RNG)";

/// [`RANDOM_SOURCE`] 的函数形式（报告/日志用）。
pub fn random_source() -> &'static str {
    RANDOM_SOURCE
}

/// `BCryptGenRandom` 的 `BCRYPT_USE_SYSTEM_PREFERRED_RNG` 标志。
///
/// 用它时 `hAlgorithm` 必须为 `NULL`：内核提供的系统首选 CSPRNG
/// （`\Device\KsecDD`，即 AES 计数器模式 DRBG），这是文档规定的用法。
const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

// 与 `runtime/memory.rs` 同一风格的手写 Win32 绑定：本 crate 不为一个函数引入
// `windows-sys`（§2.2）。`bcrypt.dll` 是 Windows 自带组件，随系统提供。
#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptGenRandom(
        algorithm: *mut core::ffi::c_void,
        buffer: *mut u8,
        length: u32,
        flags: u32,
    ) -> i32;
}

/// 用操作系统 CSPRNG 填满缓冲区。**任何失败都是错误**，没有后备熵源。
fn fill_random(buffer: &mut [u8]) -> Result<(), RandomError> {
    if buffer.is_empty() {
        return Err(RandomError::Empty);
    }
    let length = u32::try_from(buffer.len()).map_err(|_| RandomError::Empty)?;
    // SAFETY: `buffer` is a valid, exclusively borrowed slice of exactly `length` bytes;
    // `BCRYPT_USE_SYSTEM_PREFERRED_RNG` requires `algorithm` to be null and ignores it.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buffer.as_mut_ptr(),
            length,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(RandomError::Bcrypt { status });
    }
    Ok(())
}

/// 密码学随机的十六进制串（长度 = `byte_len * 2` 个十六进制字符）。
///
/// 熵**只**来自操作系统 CSPRNG（[`fill_random`]）：时间、PID、计数器与栈地址都不再参与
/// ——它们不是密码学安全的，而令牌是一个必须不可预测的共享密钥。失败即返回错误，
/// 由调用方**拒绝启动**（fail-closed）。
pub fn random_hex(byte_len: usize) -> Result<String, RandomError> {
    random_hex_with(byte_len, &fill_random)
}

/// [`random_hex`] 的唯一实现，熵的注入点。
///
/// `fill` 只出现在这里，因此"失败会怎样"是可测的：测试传入一个必然失败的填充器，
/// 断言错误真的被传播出去（而不是静默地退化到某个弱熵分支）。
fn random_hex_with(
    byte_len: usize,
    fill: &dyn Fn(&mut [u8]) -> Result<(), RandomError>,
) -> Result<String, RandomError> {
    if byte_len == 0 {
        return Err(RandomError::Empty);
    }
    let mut bytes = vec![0_u8; byte_len];
    fill(&mut bytes)?;

    let mut out = String::with_capacity(byte_len * 2);
    for byte in &bytes {
        write!(out, "{byte:02x}").expect("writing into a String cannot fail");
    }
    Ok(out)
}

/// 全部响应都必须带的三个响应头（§7.3）。
pub const SECURITY_HEADERS: [(&str, &str); 3] = [
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    ("Cache-Control", "no-store"),
];

/// 静态页里的 CSP nonce 占位符（§9 第 3 条：占位符统一为这两个字面量）。
pub const CSP_NONCE_PLACEHOLDER: &str = "__CSP_NONCE__";

/// 静态页里的令牌占位符。
pub const TOKEN_PLACEHOLDER: &str = "__SRV_TOKEN__";

/// 注入契约：只替换这两个字面量，不做任何其他改写。
///
/// 调用方必须在启动时 `inject` 后立刻 [`assert_no_placeholders_left`]，
/// 残留即启动失败（§9：替换后必须重新扫描并在残留时失败退出）。
pub fn inject(html: &str, nonce: &str, token: &str) -> String {
    debug_assert!(
        !nonce.is_empty() && !token.is_empty(),
        "an empty nonce/token would pass the residual scan while shipping an unusable page"
    );
    html.replace(CSP_NONCE_PLACEHOLDER, nonce)
        .replace(TOKEN_PLACEHOLDER, token)
}

/// 替换后仍然残留占位符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceholderError {
    pub placeholder: &'static str,
}

impl fmt::Display for PlaceholderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the static page still contains the placeholder `{}` after injection",
            self.placeholder
        )
    }
}

impl std::error::Error for PlaceholderError {}

/// 严格检查：两个占位符字面量任何一个残留都必须报错（§9）。
pub fn assert_no_placeholders_left(html: &str) -> Result<(), PlaceholderError> {
    for placeholder in [CSP_NONCE_PLACEHOLDER, TOKEN_PLACEHOLDER] {
        if html.contains(placeholder) {
            return Err(PlaceholderError { placeholder });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    use super::{
        CSP_NONCE_PLACEHOLDER, LOOPBACK_BIND_ADDRESS, LocalOrigin, RANDOM_SOURCE, RandomError,
        SECURITY_HEADERS, ServeToken, TOKEN_PLACEHOLDER, assert_loopback,
        assert_no_placeholders_left, bind_address, fill_random, generate_nonce, inject, random_hex,
        random_hex_with, random_source,
    };

    fn policy() -> LocalOrigin {
        LocalOrigin::for_port(8760)
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn the_bind_address_is_hardcoded_loopback_ipv4() {
        assert_eq!(LOOPBACK_BIND_ADDRESS, "127.0.0.1");
        let bound = bind_address(8760);
        assert_eq!(bound.ip(), IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(bound.port(), 8760);
        assert_eq!(bound.to_string(), "127.0.0.1:8760");
        // 端口是唯一的可变输入：任何端口都落在同一个 IP 上。
        for port in [1_u16, 80, 8760, 65_535] {
            assert_eq!(bind_address(port).ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        }
    }

    #[test]
    fn the_startup_assertion_accepts_only_127_0_0_1() {
        let accepted = assert_loopback(bind_address(8760)).expect("hardcoded loopback");
        assert_eq!(accepted.to_string(), "127.0.0.1:8760");

        let lan = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 8760);
        let error = assert_loopback(lan).expect_err("a LAN address must be refused");
        assert!(error.to_string().contains("192.168.1.5:8760"), "{error}");
        assert!(error.to_string().contains("--host"), "{error}");

        let any = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8760);
        assert!(assert_loopback(any).is_err(), "0.0.0.0 must be refused");

        // 服务不监听 IPv6：`[::1]` 也必须被拒。
        let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8760);
        assert!(assert_loopback(v6).is_err(), "[::1] must be refused");

        // 127.0.0.0/8 里的其他地址同样拒绝：允许的 Host 集合里只有 127.0.0.1。
        let other_loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 8760);
        assert!(assert_loopback(other_loopback).is_err());
    }

    #[test]
    fn allowed_hosts_are_ipv4_only_and_bound_to_the_actual_port() {
        let policy = policy();
        assert_eq!(
            policy.allowed_hosts(),
            strings(&["127.0.0.1:8760", "localhost:8760"]).as_slice()
        );
        assert_eq!(policy.primary_origin(), "http://127.0.0.1:8760");
        assert_eq!(
            LocalOrigin::from_bound(bind_address(9000))
                .expect("loopback")
                .allowed_hosts(),
            strings(&["127.0.0.1:9000", "localhost:9000"]).as_slice()
        );

        for accepted in [
            "127.0.0.1:8760",
            "localhost:8760",
            "LOCALHOST:8760",
            " 127.0.0.1:8760 ",
        ] {
            assert!(
                policy.check_host(Some(accepted)).is_ok(),
                "accepted: {accepted}"
            );
        }

        let rejected = [
            "[::1]:8760",          // 没有监听 IPv6
            "127.0.0.1:8761",      // 端口不匹配
            "localhost:8761",      // 端口不匹配
            "127.0.0.1",           // 非 80 端口时浏览器一定带端口
            "localhost",           // 同上
            "127.0.0.1:87600",     // 非法端口
            "127.0.0.10:8760",     // 不是允许的主机
            "evil.example:8760",   // DNS rebinding
            "127.0.0.1:8760.evil", // 前缀伪装
            "xn--127-0-0-1:8760",  // 同形异义伪装
            "",
        ];
        for host in rejected {
            let error = policy
                .check_host(Some(host))
                .expect_err(&format!("{host} must be rejected"));
            assert_eq!(error.status_code(), 421, "host: {host}");
            assert_eq!(error.code(), "bad_host", "host: {host}");
        }
        let error = policy
            .check_host(None)
            .expect_err("a missing Host must be rejected");
        assert_eq!(error.status_code(), 421);
    }

    #[test]
    fn port_80_also_accepts_the_bare_host_because_browsers_omit_the_default_port() {
        let policy = LocalOrigin::for_port(80);
        assert!(policy.check_host(Some("127.0.0.1")).is_ok());
        assert!(policy.check_host(Some("localhost")).is_ok());
        assert!(policy.check_host(Some("127.0.0.1:80")).is_ok());
        assert!(policy.check_host(Some("127.0.0.1:81")).is_err());
        assert!(policy.check_origin(Some("http://127.0.0.1"), true).is_ok());
        assert_eq!(policy.primary_origin(), "http://127.0.0.1:80");
    }

    #[test]
    fn origin_is_required_and_must_match_exactly_on_state_changing_methods() {
        let policy = policy();

        assert!(
            policy
                .check_origin(Some("http://127.0.0.1:8760"), true)
                .is_ok()
        );
        assert!(
            policy
                .check_origin(Some("http://localhost:8760"), true)
                .is_ok()
        );
        assert!(
            policy
                .check_origin(Some("HTTP://127.0.0.1:8760"), true)
                .is_ok(),
            "the scheme and host are case-insensitive"
        );

        let rejected = [
            "null",                       // `Origin: null`（sandboxed iframe / file://）
            "http://127.0.0.1:8761",      // 端口不匹配
            "http://127.0.0.1",           // 少了端口
            "https://127.0.0.1:8760",     // 不是本服务的 scheme
            "http://evil.example:8760",   // 其他 origin
            "http://127.0.0.1:8760/",     // 带路径（Origin 不带路径）
            "http://127.0.0.1:8760.evil", // 前缀伪装
            "http://[::1]:8760",          // 没有监听 IPv6
            "",
        ];
        for origin in rejected {
            let error = policy
                .check_origin(Some(origin), true)
                .expect_err(&format!("{origin} must be rejected"));
            assert_eq!(error.status_code(), 403, "origin: {origin}");
            assert_eq!(error.code(), "bad_origin", "origin: {origin}");
        }

        let error = policy
            .check_origin(None, true)
            .expect_err("a missing Origin must be rejected on POST");
        assert_eq!(error.status_code(), 403);
        assert_eq!(error.code(), "bad_origin");
    }

    #[test]
    fn read_requests_do_not_require_an_origin() {
        let policy = policy();
        assert!(
            policy.check_origin(None, false).is_ok(),
            "GET/HEAD must not be blocked on a missing Origin"
        );
        assert!(policy.check_origin(Some("null"), false).is_ok());
        assert!(
            policy
                .check_origin(Some("http://evil.example"), false)
                .is_ok(),
            "reads are guarded by the token and the Host check, not by Origin"
        );
        assert!(
            policy
                .check_origin(Some("http://127.0.0.1:8760"), false)
                .is_ok()
        );
    }

    /// 令牌与 nonce 都来自操作系统 CSPRNG：生成必须成功、必须每次都不同、必须是十六进制，
    /// 并且**报告熵源**（评审 P2-4 要求"来源可见"，而不是一句"随机"）。
    #[test]
    fn the_token_is_random_per_run_and_compared_exactly() {
        let token = ServeToken::generate().expect("BCryptGenRandom must be available");
        let other = ServeToken::generate().expect("BCryptGenRandom must be available");
        assert_eq!(token.as_str().len(), 64, "256 bit of hex");
        assert_ne!(
            token.as_str(),
            other.as_str(),
            "each run gets its own token"
        );
        assert!(
            token.as_str().chars().all(|c| c.is_ascii_hexdigit()),
            "the token must be hex: {}",
            token.as_str()
        );
        assert_eq!(random_source(), RANDOM_SOURCE);
        assert!(RANDOM_SOURCE.contains("BCryptGenRandom"), "{RANDOM_SOURCE}");

        assert!(token.matches(token.as_str()));
        assert!(!token.matches(other.as_str()));
        assert!(!token.matches(""), "an empty candidate must never match");
        assert!(
            !token.matches(&token.as_str()[..63]),
            "a prefix must not match"
        );
        assert!(
            !token.matches(&format!("{}0", token.as_str())),
            "a longer value must not match"
        );

        // 逐字节差异都会被捕获（含首字节与末字节）。
        let mut first_flipped = token.as_str().to_string();
        let first = if first_flipped.starts_with('0') {
            '1'
        } else {
            '0'
        };
        first_flipped.replace_range(0..1, &first.to_string());
        assert!(!token.matches(&first_flipped));
        let mut last_flipped = token.as_str().to_string();
        let last = if last_flipped.ends_with('0') {
            '1'
        } else {
            '0'
        };
        last_flipped.replace_range(63..64, &last.to_string());
        assert!(!token.matches(&last_flipped));
    }

    /// **fail-closed 分支**：熵源失败时错误必须被传播出去，而不是静默退化到弱熵。
    ///
    /// 注入点是 [`random_hex_with`] 的填充器（生产路径用的是 [`fill_random`]，它只会在
    /// `BCryptGenRandom` 失败时返回错误——在一台健康的 Windows 上无法让它失败，
    /// 因此这里注入失败来**真的执行**那条分支，而不是只留一句注释）。
    #[test]
    fn a_failing_entropy_source_is_propagated_and_never_falls_back() {
        let failing = |_buffer: &mut [u8]| {
            Err(RandomError::Bcrypt {
                status: 0xC000_0001u32 as i32,
            })
        };
        let error = random_hex_with(32, &failing).expect_err("a failing RNG must fail generation");
        assert_eq!(
            error,
            RandomError::Bcrypt {
                status: 0xC000_0001u32 as i32
            }
        );
        let text = error.to_string();
        assert!(text.contains("BCryptGenRandom"), "{text}");
        assert!(text.contains("refuses to start"), "{text}");

        // 0 字节同样是错误（不会返回一个"看起来成功"的空串）。
        assert_eq!(random_hex_with(0, &failing), Err(RandomError::Empty));
        assert_eq!(random_hex(0), Err(RandomError::Empty));

        // 生产路径的填充器在健康主机上必须成功，且写入的确实是随机字节。
        let mut buffer = [0_u8; 32];
        fill_random(&mut buffer).expect("the OS CSPRNG must be available");
        assert!(buffer.iter().any(|byte| *byte != 0));
    }

    #[test]
    fn the_token_is_redacted_in_debug_output() {
        let token = ServeToken::generate().expect("the OS CSPRNG must be available");
        let text = format!("{token:?}");
        assert!(
            !text.contains(token.as_str()),
            "debug output leaked the token"
        );
        assert!(text.contains("redacted"), "{text}");
    }

    #[test]
    fn nonces_are_random_and_hex() {
        let first = generate_nonce().expect("the OS CSPRNG must be available");
        let second = generate_nonce().expect("the OS CSPRNG must be available");
        assert_eq!(first.len(), 32, "128 bit of hex");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first}");
        assert_ne!(first, second);
    }

    #[test]
    fn the_security_headers_are_the_documented_set() {
        assert_eq!(
            SECURITY_HEADERS,
            [
                ("X-Content-Type-Options", "nosniff"),
                ("Referrer-Policy", "no-referrer"),
                ("Cache-Control", "no-store"),
            ]
        );
    }

    #[test]
    fn the_placeholder_literals_are_exactly_the_two_documented_strings() {
        assert_eq!(CSP_NONCE_PLACEHOLDER, "__CSP_NONCE__");
        assert_eq!(TOKEN_PLACEHOLDER, "__SRV_TOKEN__");
        assert_ne!(CSP_NONCE_PLACEHOLDER, TOKEN_PLACEHOLDER);
    }

    #[test]
    fn inject_replaces_both_placeholders_and_leaves_nothing_behind() {
        let page = "<script nonce=\"__CSP_NONCE__\">let t='__SRV_TOKEN__';</script>\
                    <style nonce=\"__CSP_NONCE__\"></style>";
        let nonce = generate_nonce().expect("the OS CSPRNG must be available");
        let token = ServeToken::generate().expect("the OS CSPRNG must be available");
        let injected = inject(page, &nonce, token.as_str());

        assert_eq!(
            injected.matches(&nonce).count(),
            2,
            "the page has 4 nonce slots, 2 here"
        );
        assert_eq!(injected.matches(token.as_str()).count(), 1);
        assert!(!injected.contains(CSP_NONCE_PLACEHOLDER));
        assert!(!injected.contains(TOKEN_PLACEHOLDER));
        assert_no_placeholders_left(&injected).expect("no placeholder may survive injection");
    }

    #[test]
    fn assert_no_placeholders_left_names_the_leftover_placeholder() {
        let error = assert_no_placeholders_left("<p>__CSP_NONCE__</p>")
            .expect_err("a left nonce placeholder must fail the startup scan");
        assert_eq!(error.placeholder, CSP_NONCE_PLACEHOLDER);
        assert!(error.to_string().contains("__CSP_NONCE__"), "{error}");

        let error = assert_no_placeholders_left("<p>__SRV_TOKEN__</p>")
            .expect_err("a left token placeholder must fail the startup scan");
        assert_eq!(error.placeholder, TOKEN_PLACEHOLDER);

        assert!(
            assert_no_placeholders_left("<p>__csp_nonce__</p>").is_ok(),
            "case matters"
        );
        assert!(assert_no_placeholders_left("<p>plain page</p>").is_ok());
    }
}
