//! `rapidocr serve` 的启动编排：CLI → 绑定 → 校验 → 模型 → 页面 → 运行时（§3、§7、§9）。
//!
//! # 启动顺序（`docs/05` §7.6 的逐步落地）
//!
//! 1. **绑定**硬编码的 `127.0.0.1`（[`super::security::bind_address`]：签名里没有地址参数）
//!    → 失败即退出（不静默换端口），并断言实际绑定地址属于 loopback；
//! 2. **校验运行配置**（[`ServeStartup::validate`]）：资源上限、provider 名称与对应
//!    feature、引擎配置；任一非法立即退出；
//! 3. **检查模型集**（[`ModelPlan`]）：不齐备 → 服务照常 Ready + `BlockedModelsMissing`；
//!    齐备 → 预加载 `Loading → Ready|Failed`（发生在 [`ServeRuntime::new`] 里）。
//!
//! # 参数优先级（§3）
//!
//! `CLI flag > --config YAML > 内建默认`。本模块负责把"哪个值生效、被覆盖的值是什么"
//! 打印出来（`max_side_len` 与 `provider_preference` 两项是 §3 明确要求的）。
//!
//! # 页面注入（§9）
//!
//! 页面**只有两个字面量**会被替换（`__CSP_NONCE__`、`__SRV_TOKEN__`），替换后立即做三项
//! 断言：占位符无残留、每个 `nonce="…"` 属性与 CSP 头的 nonce **逐字节相同**、
//! token 真的出现在页面里。任一项失败 → **拒绝启动**（绝不发出一个"看起来能跑、
//! 实际被自己的 CSP 拦掉"的页面）。

use std::net::SocketAddr;
use std::process::Command;

use super::cli::ServeArgs;
use super::download;
use super::evaluate;
use super::http::{BoundServer, ServeHandle};
use super::limits::{DEFAULT_ALLOW_DOWNLOAD, DEFAULT_ALLOW_PROVIDER_FALLBACK, DEFAULT_PROVIDER};
use super::model_plan::{ModelPlan, ModelPlanError, ModelSnapshot};
use super::security::{
    NonLoopbackBindError, PlaceholderError, RandomError, ServeToken, TOKEN_PLACEHOLDER,
    assert_no_placeholders_left, generate_nonce, inject, random_source,
};
use super::server::ServeContext;
use super::state::{ServeStartup, StartupConfigError};

/// 内联页面的唯一来源（`Temp/demo3-v2.html` 是评审过的原型，**保持不动**）。
pub(crate) const PAGE_TEMPLATE: &str = include_str!("../web/index.html");

/// 页面在仓库里的路径（错误信息里用它定位）。
pub(crate) const PAGE_PATH: &str = "src/bin/web/index.html";

/// 启动失败：每一条都必须能定位到具体开关/文件。
#[derive(Debug)]
pub(crate) enum ServeStartError {
    /// 绑定失败（端口被占用等）——**不静默换端口**（§3）。
    Bind { address: SocketAddr, detail: String },
    /// 实际绑定地址不是硬编码的 loopback（§7.1 的启动期断言）。
    NonLoopback(NonLoopbackBindError),
    /// 运行配置非法（§7.5/§7.6 第 2 步）。
    Config(StartupConfigError),
    /// 模型清单/单一来源规则失败（§5.3）。
    Models(ModelPlanError),
    /// 静态页注入契约失败（§9）。
    Page(PageError),
    /// `--formula-detector` 指向的文件不存在（M4：配置错误在启动期失败）。
    FormulaDetector { path: std::path::PathBuf },
    /// `--eval-root` 不可用（不存在、不是目录、或无法规范化）——启动期失败，
    /// 绝不"先开着、第一次评估才发现沙箱是空的"。
    EvalRoot {
        path: std::path::PathBuf,
        detail: String,
    },
    /// 操作系统 CSPRNG 不可用（评审 P2-4 的 fail-closed 行为）。
    Random(RandomError),
    /// 运行期无法启动（线程创建失败）。
    Runtime(std::io::Error),
}

impl std::fmt::Display for ServeStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bind { address, detail } => write!(
                f,
                "cannot bind {address}: {detail}; rapidocr serve reports the conflict instead of \
                 silently switching ports (docs/05 §3)"
            ),
            Self::NonLoopback(error) => write!(f, "{error}"),
            Self::Config(error) => write!(f, "startup configuration rejected: {error}"),
            Self::Models(error) => write!(f, "model inventory rejected: {error}"),
            Self::Page(error) => write!(f, "{error}"),
            Self::FormulaDetector { path } => write!(
                f,
                "--formula-detector {} does not exist (or is not a file); the formula queue needs \
                 the page formula detection model, and a wrong path must fail at startup instead \
                 of on the first formula request (docs/05 §4.2)",
                path.display()
            ),
            Self::Runtime(error) => write!(f, "cannot start the serve runtime: {error}"),
            Self::Random(error) => write!(
                f,
                "refusing to start: {error}; the service token and the CSP nonce must come from a \
                 cryptographic RNG (docs/05 §7.2, M1 review P2-4)"
            ),
            Self::EvalRoot { path, detail } => write!(
                f,
                "--eval-root {} is not usable as an evaluation sandbox: {detail}; \
                 POST /api/evaluate must not read local paths outside an explicitly configured \
                 root (docs/05 §4.2, M1 review P2-3)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ServeStartError {}

impl From<NonLoopbackBindError> for ServeStartError {
    fn from(error: NonLoopbackBindError) -> Self {
        Self::NonLoopback(error)
    }
}

impl From<StartupConfigError> for ServeStartError {
    fn from(error: StartupConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<ModelPlanError> for ServeStartError {
    fn from(error: ModelPlanError) -> Self {
        Self::Models(error)
    }
}

impl From<PageError> for ServeStartError {
    fn from(error: PageError) -> Self {
        Self::Page(error)
    }
}

/// 静态页注入契约的失败（§9：替换后必须重新扫描并在残留时**失败退出**）。
#[derive(Debug)]
pub(crate) enum PageError {
    /// 替换后仍然残留占位符。
    Residual(PlaceholderError),
    /// 页面里存在与 CSP 头 nonce 不同的 `nonce="…"` 属性（或一个都没有）。
    NonceMismatch { found: Vec<String> },
    /// token 没有出现在页面里（页面会退化成离线预览模式，等于永远不访问服务）。
    TokenMissing,
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Residual(error) => write!(f, "{PAGE_PATH}: {error}"),
            Self::NonceMismatch { found } => write!(
                f,
                "{PAGE_PATH}: the rendered page has nonce attributes that do not match the CSP \
                 nonce ({found:?}); the page would be blocked by its own CSP"
            ),
            Self::TokenMissing => write!(
                f,
                "{PAGE_PATH}: the rendered page does not contain {TOKEN_PLACEHOLDER}'s value; the \
                 page would silently fall back to offline preview mode"
            ),
        }
    }
}

impl std::error::Error for PageError {}

/// 启动 `serve`（阻塞到 accept loop 结束）。
pub(crate) fn run(args: ServeArgs) -> Result<(), ServeStartError> {
    // §7.6 第 1 步：先绑定，绑定失败就不要做任何别的事。
    let bound = BoundServer::bind(args.port)?;
    let local = super::security::LocalOrigin::from_bound(bound.local_addr())?;
    let model_dir = args.model_dir();
    // 评审 P2-4：令牌与 nonce 的熵**只**来自操作系统 CSPRNG。拿不到就拒绝启动
    // （fail-closed），而不是继续用一个弱熵令牌。
    let token = ServeToken::generate().map_err(ServeStartError::Random)?;
    let nonce = generate_nonce().map_err(ServeStartError::Random)?;
    let page = render_page(&token, &nonce)?;
    println!("serve: token/nonce entropy source: {}", random_source());

    // §7.6 第 2 步：运行配置（上限 / provider / 引擎配置）。
    let raw_engine = args.engine_config()?;
    let yaml_provider = raw_engine.runtime.provider_preference;
    let yaml_max_side = raw_engine.global.max_side_len;
    let startup = ServeStartup::validate(
        args.raw_limits(),
        raw_engine,
        args.provider_preference(),
        args.max_side,
        args.allow_provider_fallback(),
    )?;
    // §6.1 第 3 条：`--allow-download-host` 是**显式**的可信配置扩展，必须自我一致
    // （裸主机名）并且必须打印高风险警告。
    let allow_download_hosts = download::validate_extra_hosts(&args.allow_download_host)
        .map_err(StartupConfigError::Limit)?;

    // §7.6 第 3 步：模型集（单一来源规则）与就绪判定。
    //
    // 快照只算一次（逐文件校验会重新读盘并哈希）：日志、引擎状态机与 `/api/models`
    // 的启动期字段都来自这一份。
    let model_plan = ModelPlan::resolve(&model_dir, &startup.plan.engine)?;
    let snapshot = model_plan.snapshot();
    let engine_config_min_side = startup.plan.engine.global.min_side_len;
    debug_assert!(engine_config_min_side > 0);

    // M4：公式队列的检测模型。优先级与其它开关一致（CLI > 模型集声明）：
    // `--formula-detector` 是显式配置，模型集里的 `formula_detector` role（本地清单可以声明）
    // 是"这个目录自己描述了它"；两者都没有时公式路由不可用（§10.8 默认关闭），
    // 但**普通 OCR 完全不受影响**。
    //
    // 评审 P1-1：解析结果是"路径 + 集合声明的 SHA-256"，两者一起交给 `FormulaPolicy`，
    // 库在加载检测器时校验——与识别模型完全对称，不再存在"检测器从不校验"的缺口。
    if let Some(path) = args.formula_detector.as_ref()
        && !path.is_file()
    {
        // 启动期就校验它真的存在：一个打错的文件名应该在启动时报出来，
        // 而不是等到第一次公式请求（§7.6 第 2 步的"配置错误在启动期失败"）。
        return Err(ServeStartError::FormulaDetector { path: path.clone() });
    }
    let formula_detector = model_plan.resolve_formula_detector(args.formula_detector.as_deref())?;
    let routing =
        super::server::routing_for(formula_detector.as_ref().map(|spec| spec.path.as_path()));

    // M1 评审 P2-3：`--eval-root` 沙箱。缺少它时 `/api/evaluate` **整体关闭**
    // （而不是"接受任意本机路径"）：评估是唯一接受"路径"而不是"上传字节"的端点，
    // 因此必须由启动参数显式开启，并且清单与它引用的每张图都必须落在该目录内。
    let eval_root = match args.eval_root.as_ref() {
        Some(directory) => Some(evaluate::EvalRoot::new(directory).map_err(|detail| {
            ServeStartError::EvalRoot {
                path: directory.clone(),
                detail,
            }
        })?),
        None => None,
    };

    log_startup(
        &args,
        &startup,
        &local,
        &snapshot,
        &allow_download_hosts,
        yaml_provider_is_overridden(&args),
        yaml_max_side,
        yaml_provider,
        &routing,
        formula_detector.as_ref().map(|spec| spec.path.as_path()),
        eval_root.as_ref(),
    );

    let port = local.port();
    let context = ServeContext {
        limits: startup.limits,
        plan: startup.plan,
        model_plan,
        snapshot,
        token,
        local,
        page,
        nonce,
        allow_download: args.allow_download,
        allow_download_hosts,
        // §7.5：运行期切换 provider 必须沿用启动期的同一个开关值。
        allow_provider_fallback: args.allow_provider_fallback(),
        // M4：公式路由由"是否配置了页面公式检测模型"唯一决定（§4.2、§10.8）。
        // 请求侧没有第二个开关：`queue=formula` **就是**公式管线的选择。
        routing,
        formula_detector,
        // M1 评审 P2-3：`/api/evaluate` 的沙箱（`None` = 端点整体关闭）。
        eval_root,
        engine_factory: super::engine::real_engine_factory(),
        downloader: super::download::real_downloader_factory(),
        free_space: super::server::real_free_space(),
    };

    let mut service = ServeHandle::start(bound, context)?;
    // §7.1 的不变量：允许的 Host/Origin 集合必须由**实际绑定端口**算出。
    if port != service.local_addr().port() {
        return Err(ServeStartError::Bind {
            address: service.local_addr(),
            detail: format!(
                "the allow-list was built for port {port} but the listener bound {}",
                service.local_addr().port()
            ),
        });
    }
    log_engine(&service);
    if args.open {
        open_browser(service.shared().local().primary_origin());
    }
    service.accept_loop();
    Ok(())
}

/// 注入 nonce/token；失败即拒绝启动（§9）。
pub(crate) fn render_page(token: &ServeToken, nonce: &str) -> Result<String, PageError> {
    render_page_with(PAGE_TEMPLATE, nonce, token.as_str())
}

/// 注入的唯一实现（测试用病理输入直接验证三个断言）。
pub(crate) fn render_page_with(
    template: &str,
    nonce: &str,
    token: &str,
) -> Result<String, PageError> {
    let page = inject(template, nonce, token);
    assert_no_placeholders_left(&page).map_err(PageError::Residual)?;
    check_nonce_attributes(&page, nonce)?;
    if !page.contains(token) {
        return Err(PageError::TokenMissing);
    }
    Ok(page)
}

/// 页面里每个 `nonce="…"` **属性**必须与 CSP 头的 nonce **逐字节相同**（§9.5）。
///
/// HTML 注释会被剔除：页面顶部的契约注释里**逐字**写了 `nonce="…"` 这个形状（它是在
/// 描述这条规则本身，不是属性），把它当成属性会让正确页面被拒。
fn check_nonce_attributes(page: &str, nonce: &str) -> Result<(), PageError> {
    let markup = strip_html_comments(page);
    let mut found = Vec::new();
    let mut rest = markup.as_str();
    while let Some(index) = rest.find("nonce=\"") {
        rest = &rest[index + "nonce=\"".len()..];
        let Some(end) = rest.find('"') else {
            break;
        };
        found.push(rest[..end].to_string());
        rest = &rest[end + 1..];
    }
    if found.is_empty() || found.iter().any(|value| value != nonce) {
        return Err(PageError::NonceMismatch { found });
    }
    Ok(())
}

/// 去掉 `<!-- … -->`（含未闭合的尾部）。
fn strip_html_comments(page: &str) -> String {
    let mut out = String::with_capacity(page.len());
    let mut rest = page;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + "-->".len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// CLI 是否覆盖了 YAML 的 `max_side_len`。
fn args_max_side_is_overridden(args: &ServeArgs) -> bool {
    args.max_side.is_some()
}

/// 启动日志：实际监听地址、允许的 Host/Origin 集合、生效的取值与被覆盖的取值（§3、§7.1）。
#[allow(clippy::too_many_arguments)]
fn log_startup(
    args: &ServeArgs,
    startup: &ServeStartup,
    local: &super::security::LocalOrigin,
    snapshot: &ModelSnapshot,
    allow_download_hosts: &[String],
    provider_overridden: bool,
    yaml_max_side: usize,
    yaml_provider: rapid_ocr_rs::ProviderPreference,
    routing: &super::server::OcrRouting,
    formula_detector: Option<&std::path::Path>,
    eval_root: Option<&evaluate::EvalRoot>,
) {
    println!(
        "serve: listening on {} (IPv4 loopback only; there is deliberately no --host option)",
        local.primary_origin()
    );
    println!(
        "serve: allowed Host: {} | allowed Origin: {}",
        local.allowed_hosts().join(", "),
        local.allowed_origins().join(", ")
    );
    println!(
        "serve: model dir {} (source: {}, {})",
        snapshot.model_dir().display(),
        snapshot.source_label(),
        snapshot.summary()
    );
    println!(
        "serve: formula queue {} (formula detector: {}; ordinary OCR is independent of it)",
        if routing.formula {
            "enabled by --formula-detector"
        } else {
            "disabled (no formula detector configured)"
        },
        formula_detector
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
    let formula_blocking = snapshot.formula_blocking_names();
    println!(
        "serve: formula model set {}",
        if formula_blocking.is_empty() {
            "complete".to_string()
        } else {
            format!(
                "incomplete: {} (the formula queue returns 409 until it is present; text OCR is \
                 unaffected)",
                formula_blocking.join(", ")
            )
        }
    );
    println!(
        "serve: evaluation {}",
        match eval_root {
            Some(root) => format!(
                "enabled with --eval-root {} (the manifest and every image it references must \
                 resolve inside that directory)",
                root.root().display()
            ),
            None => "disabled (no --eval-root): POST /api/evaluate refuses with a locating error, \
                     because evaluation reads local paths instead of uploaded bytes"
                .to_string(),
        }
    );
    println!(
        "serve: provider requested={} (documented default: {DEFAULT_PROVIDER}; {}), \
         allow_provider_fallback={} (documented default: {DEFAULT_ALLOW_PROVIDER_FALLBACK}), \
         fail_if_provider_unavailable={}",
        startup.plan.requested_label(),
        if provider_overridden {
            format!(
                "--provider overrides --config {}",
                rapid_ocr_rs::format_provider_preference(yaml_provider)
            )
        } else {
            "from --config / built-in default".to_string()
        },
        args.allow_provider_fallback(),
        startup.plan.fail_if_provider_unavailable
    );
    println!(
        "serve: max_side_len={} ({})",
        startup.plan.engine.global.max_side_len,
        if args_max_side_is_overridden(args) {
            format!("--max-side overrides --config {yaml_max_side}")
        } else {
            format!("from --config / built-in default ({yaml_max_side})")
        }
    );
    println!(
        "serve: limits max_body={} B max_result={} B max_queue_text={} max_queue_formula={} \
         max_retained={} job_ttl={} ms ({})",
        startup.limits.max_body_bytes,
        startup.limits.max_result_bytes,
        startup.limits.max_queue_text,
        startup.limits.max_queue_formula,
        startup.limits.max_retained,
        startup.limits.job_ttl_ms,
        if args.uses_documented_defaults() {
            "all documented defaults"
        } else {
            "some values overridden on the command line"
        }
    );
    println!(
        "serve: downloads {} (documented default: {}), token required on every /api/* request \
         (the token is printed only into the page)",
        if args.allow_download {
            "enabled via --allow-download"
        } else {
            "disabled (no --allow-download)"
        },
        if DEFAULT_ALLOW_DOWNLOAD {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "serve: trusted download hosts {}",
        effective_hosts(allow_download_hosts).join(", ")
    );
    if !allow_download_hosts.is_empty() {
        // §6.1 第 3 条要求的**高风险警告**：白名单必须来自可信配置，而不是资源描述自身
        // （OWASP SSRF Prevention）。把理由与后果都写出来，而不是一句"已启用"。
        eprintln!(
            "serve: WARNING --allow-download-host extends the trusted download allow-list beyond \
             the compiled-in set [{}]: [{}]. The compiled-in constant is what makes a local \
             manifest.json unable to point downloads at arbitrary hosts: a manifest is a \
             *resource description*, and docs/05 §6.1 item 3 (OWASP SSRF prevention) requires the \
             allow-list to come from trusted configuration instead. Every host you add here is \
             accepted for model downloads; add only hosts you control or trust, and remove the \
             flag when you no longer need it.",
            rapid_ocr_rs::ALLOWED_DOWNLOAD_HOSTS.join(", "),
            allow_download_hosts.join(", ")
        );
    }
}

/// 编译期白名单 ∪ `--allow-download-host`（启动日志用；库常量本身不变）。
fn effective_hosts(extra: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = rapid_ocr_rs::ALLOWED_DOWNLOAD_HOSTS
        .iter()
        .map(|host| (*host).to_string())
        .collect();
    hosts.extend(extra.iter().cloned());
    hosts
}

/// 引擎状态的一行摘要（§7.5/§7.6：`Failed` 的原因必须可见）。
fn log_engine(handle: &ServeHandle) {
    let shared = handle.shared();
    println!(
        "serve: service state = {:?}, engine state = {}",
        shared.service_state(),
        shared.engine_state_name()
    );
    let provider = shared.provider_status();
    println!(
        "serve: engine provider requested={} selected_ep={} fallback_to_cpu={}",
        provider.requested,
        provider.selected_ep.as_deref().unwrap_or("<unknown>"),
        match provider.fallback_to_cpu {
            Some(value) => value.to_string(),
            None => "<unknown>".to_string(),
        }
    );
}

/// 用系统默认浏览器打开页面（**不新增依赖**，§2.2）。
fn open_browser(url: &str) {
    if url.is_empty() {
        return;
    }
    match Command::new("cmd").args(["/C", "start", "", url]).spawn() {
        Ok(_) => println!("serve: opened {url}"),
        Err(error) => eprintln!("serve: cannot open a browser ({error}); open {url} manually"),
    }
}

/// `--provider` 是否覆盖了 YAML（日志用）。
fn yaml_provider_is_overridden(args: &ServeArgs) -> bool {
    args.provider.is_some()
}

#[cfg(test)]
mod tests {
    use super::{
        PAGE_TEMPLATE, PageError, check_nonce_attributes, render_page_with, strip_html_comments,
    };

    /// 真实页面：三个断言全过，且每个 `nonce=` 属性与 CSP nonce 逐字节相同。
    ///
    /// 页面里的 `__CSP_NONCE__` 出现 4 次（1 次在文件顶部的契约注释里 + style×1 +
    /// script×2），其中只有 **3** 个是真正的 `nonce="…"` 属性——这正是 §9 说的
    /// "按 nonce 属性计数，不要用全文出现次数"。
    #[test]
    fn the_real_page_injects_cleanly_and_matches_the_csp_nonce() {
        let page = render_page_with(PAGE_TEMPLATE, "n0nce", "t0ken").expect("the page injects");
        assert!(!page.contains("__CSP_NONCE__"));
        assert!(!page.contains("__SRV_TOKEN__"));
        assert_eq!(page.matches("nonce=\"n0nce\"").count(), 3);
        assert_eq!(page.matches("n0nce").count(), 4);
        assert_eq!(page.matches("t0ken").count(), 3);
    }

    /// 残留占位符 → **拒绝启动**。这里构造的是注入本身的病理输入：token 的值里含有
    /// 另一个占位符字面量，于是"先换 nonce、后换 token"的顺序会把占位符重新塞回页面。
    /// 断言的是"替换后一定重新扫描"这条硬要求（§9）。
    #[test]
    fn a_residual_placeholder_refuses_to_start() {
        let error = render_page_with("__SRV_TOKEN__", "n", "__CSP_NONCE__")
            .expect_err("a residual placeholder must refuse to start");
        assert!(matches!(error, PageError::Residual(_)), "{error:?}");
        assert!(error.to_string().contains("__CSP_NONCE__"), "{error}");
    }

    /// nonce 与页面属性不一致（例如旧页面写死了别的 nonce）→ 拒绝启动。
    #[test]
    fn a_foreign_nonce_refuses_to_start() {
        let error = render_page_with(
            r#"<style nonce="deadbeef"></style><script nonce="deadbeef">window.__RROCR__={token:"__SRV_TOKEN__"};</script>"#,
            "n0nce",
            "t0ken",
        )
        .expect_err("a foreign nonce must refuse to start");
        match error {
            PageError::NonceMismatch { found } => {
                assert_eq!(found, vec!["deadbeef".to_string(), "deadbeef".to_string()]);
            }
            other => panic!("expected a nonce mismatch, got {other:?}"),
        }
    }

    /// 页面里根本没有 nonce 属性 / 没有 token → 也要拒绝（否则页面要么被 CSP 拦掉、
    /// 要么静默退化成离线预览）。
    #[test]
    fn a_page_without_a_nonce_or_a_token_refuses_to_start() {
        assert!(matches!(
            render_page_with("<p>__SRV_TOKEN__</p>", "n0nce", "t0ken"),
            Err(PageError::NonceMismatch { .. })
        ));
        assert!(matches!(
            render_page_with("<p nonce=\"__CSP_NONCE__\">x</p>", "n0nce", "t0ken"),
            Err(PageError::TokenMissing)
        ));
    }

    #[test]
    fn nonce_attributes_are_compared_byte_for_byte() {
        assert!(check_nonce_attributes(r#"<a nonce="abc">"#, "abc").is_ok());
        assert!(check_nonce_attributes(r#"<a nonce="abc"><b nonce="ABC">"#, "abc").is_err());
        assert!(check_nonce_attributes("<a>", "abc").is_err());
        // HTML 注释里的同形文字不算属性（页面顶部的契约注释就是这种情况）。
        assert!(check_nonce_attributes(r#"<!-- nonce="…" --><a nonce="abc">"#, "abc").is_ok());
    }

    #[test]
    fn html_comments_are_removed_including_an_unterminated_tail() {
        assert_eq!(strip_html_comments("a<!-- b -->c"), "ac");
        assert_eq!(strip_html_comments("a<!-- b -->c<!-- d -->e"), "ace");
        assert_eq!(strip_html_comments("a<!-- b"), "a");
        assert_eq!(strip_html_comments("plain"), "plain");
    }
}
