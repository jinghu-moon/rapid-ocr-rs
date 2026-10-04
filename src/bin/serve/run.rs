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
use super::flowlog::{FlowSink, LogLevel};
use super::http::{BoundServer, ServeHandle};
use super::limits::{DEFAULT_ALLOW_DOWNLOAD, DEFAULT_ALLOW_PROVIDER_FALLBACK, DEFAULT_PROVIDER};
use super::model_plan::{ModelPlan, ModelPlanError, ModelSnapshot, Pipeline, PlanReverification};
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
    /// `--reverify-models`：启动期冷验证发现这次运行会用到的模型文件缺失或损坏。
    ///
    /// **拒绝启动**（而不是"照常启动、第一次用时才失败"）：这是本开关的全部价值——
    /// 把验证从首次使用移到启动期，并让错误在服务开始接受请求之前就出现。
    ModelsUnusable {
        /// 启动期冷验证的逐文件结论（含每份文件的实际状态与摘要）。
        report: Box<super::model_plan::PlanReverification>,
    },
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
            Self::ModelsUnusable { report } => {
                write!(
                    f,
                    "refusing to start: --reverify-models could not verify every model file this \
                     run will load. Offending file(s) by pipeline:\n  {}",
                    report.blocking_summary()
                )?;
                // 逐文件结论全部列出（包括"这次算出来的摘要"），便于定位到底是哪一份、
                // 期望什么、实际是什么——而不是一句"模型不可用"。
                for file in report.files() {
                    write!(f, "\n  serve: {}", file.describe())?;
                }
                write!(
                    f,
                    "\n  (docs/05 §3: --reverify-models verifies this run's whole plan — the text \
                     pipeline and, when formula routing is enabled, the formula pipeline — at \
                     startup and refuses to serve from an unusable model set)"
                )
            }
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
    // M5：流动日志级别（`--log-level` > `RAPID_OCR_SERVE_LOG` > `off`）。取值非法就拒绝启动
    // —— 一个写错的开关名不应该表现为"日志开关看起来没生效"。
    let log_level = args.log_level()?;
    println!(
        "serve: flow logging: --log-level={} (RAPID_OCR_SERVE_LOG={}); {}",
        log_level.name(),
        std::env::var("RAPID_OCR_SERVE_LOG").unwrap_or_else(|_| "<unset>".to_string()),
        match log_level {
            LogLevel::Off =>
                "one line per HTTP request and per job lifecycle is off (pass \
                              --log-level flow to turn it on)",
            LogLevel::Flow =>
                "one line per HTTP request and per job lifecycle is on: \
                               request id, method/path/status/bytes/duration, and the job's \
                               admission/running/terminal lines share that request id",
        }
    );

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

    // §7.6 第 3 步：模型集（单一来源规则）、**这次运行的模型计划**与就绪判定。
    //
    // 计划在这里一次算完（含公式检测模型的解析），因此 A1/A2/`/api/models` 的 `pipelines`
    // 块、两条管线的准入与两条加载路径读的都是同一份清单。
    //
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
    let model_plan = ModelPlan::resolve(
        &model_dir,
        &startup.plan.engine,
        args.formula_detector.as_deref(),
    )?;
    // 快照只算一次（逐文件校验会重新读盘并哈希）：日志、引擎状态机与 `/api/models`
    // 的启动期字段都来自这一份。
    let snapshot = model_plan.snapshot();
    let engine_config_min_side = startup.plan.engine.global.min_side_len;
    debug_assert!(engine_config_min_side > 0);

    // 公式路由由**运行计划**唯一决定（检测模型在场 ⇔ 公式管线属于这次运行）。
    let routing = super::server::OcrRouting::from_plan(&model_plan);

    // A1：`--reverify-models` —— 启动期**冷验证**这次运行会真正加载的每一个模型文件。
    if args.reverify_models {
        reverify_gate(&model_plan)?;
    }
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
        model_plan
            .formula_detector()
            .map(|spec| spec.path.as_path()),
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
        // M1 评审 P2-3：`/api/evaluate` 的沙箱（`None` = 端点整体关闭）。
        eval_root,
        // M5：流动日志出口。`off`（默认）时是 `FlowSink::Off`，所有记录函数立即返回。
        flow: FlowSink::for_level(log_level),
        engine_factory: super::engine::real_engine_factory(),
        downloader: super::download::real_downloader_factory(),
        free_space: super::server::real_free_space(),
        // 生产路径没有测试钩子。
        post_verify: None,
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

/// A1 的**唯一**实现：启动期冷验证这次运行会真正加载的模型文件，任一缺失/损坏即拒绝启动。
///
/// # 它为什么不是"清缓存"
///
/// 进程内的校验缓存本来就没有历史（新进程 = 空缓存）。这个开关的价值是把验证从
/// **首次使用**移到**启动期**，并在文件不可用时让服务**根本不起来**，
/// 而不是"先跑起来、等第一次 OCR 才发现 566 MB 的模型是坏的"。
///
/// # 校验范围
///
/// [`ModelPlan::plan_files`]（**这次运行的整份计划**：文本管线的
/// detector/recognizer/dictionary，`use_cls` 时再加 classifier，**加上**公式路由启用时的
/// formula_recognizer 与 formula_detector）——**不是**默认表里的每一个文件：
/// 把本轮不会打开的模型也哈希一遍既是纯浪费，也会让"启动失败"指向一个无关的文件。
/// 公式路由关闭时公式模型**不在计划里**，因此一个损坏的 566 MB 识别模型不会拦下普通 OCR，
/// 也不会被读一遍。
///
/// # 逐文件一行日志 + fail-fast
///
/// 每个文件都打印一行（状态、这一轮是否真的算了摘要、算出的摘要），并在**计划里有任何文件
/// 不可用**时返回 [`ServeStartError::ModelsUnusable`]（错误按管线分组点名那些文件）。
/// 检查清单与引擎/`/api/models`/准入用的是**同一份计划**，因此"报告的一份、加载的另一份"
/// 不可能发生（这里用一次自检把这一点钉住）。
pub(crate) fn reverify_gate(model_plan: &ModelPlan) -> Result<PlanReverification, ServeStartError> {
    let started = std::time::Instant::now();
    let report = model_plan.reverify();
    let elapsed_ms = started.elapsed().as_millis();
    for file in report.files() {
        println!(
            "serve: --reverify-models {} | {} | digest computed by this call: {}",
            file.describe(),
            file.cause.as_str(),
            file.sha256.is_some()
        );
    }
    if !report.content_changed().is_empty() {
        // 局部摘要抓住的"同体积同 mtime 的内容替换"：如实说出来，
        // 因为这类替换在旧身份下会静默沿用旧摘要（docs/05 §4.2.1 的盲区已收窄）。
        println!(
            "serve: --reverify-models content changed for: {} (the stat identity matched but the \
             first/last 64 KiB did not)",
            report.content_changed().join(", ")
        );
    }
    println!(
        "serve: --reverify-models verified {} file(s) in {elapsed_ms} ms ({} full digest(s) \
         computed by this call, {} missing/corrupt in this run's plan)",
        report.files().len(),
        report.digests_computed(),
        report.blocking().len()
    );
    if report.blocking().is_empty() {
        return Ok(report);
    }
    // 拒绝启动之前先自检一次"两份清单是否真的同源"：`report` 是刚刚冷验证出来的结论，
    // 而引擎/`/api/models`/`POST /api/ocr` 的 409 用的是 `ModelPlan::snapshot()` 那一条。
    // 两者读的是**同一份计划**，因此必须在**同一个磁盘状态**上给出同一个清单；
    // 不一致就是"报告的一份、加载的另一份"，必须立刻暴露。
    //
    // 注意：`snapshot` 必须在这里**重新取**（调用方的快照是在这次冷验证之前算的），
    // 否则一次位于两者之间的文件改动会让这个断言误报。
    // 按管线比较、顺序敏感：两条路径都继承计划的顺序，因此顺序也必须一致。
    let fresh = model_plan.snapshot();
    for pipeline in Pipeline::ALL {
        let expected: Vec<String> = match pipeline {
            Pipeline::Text => fresh.blocking_names(),
            Pipeline::Formula => fresh.formula_blocking_names(),
        };
        let observed: Vec<String> = report
            .blocking_in(pipeline)
            .iter()
            .map(|file| file.name.clone())
            .collect();
        if observed != expected {
            eprintln!(
                "serve: WARNING --reverify-models found {observed:?} in the {} pipeline but the \
                 engine's readiness snapshot reports {expected:?}; both come from the same plan, \
                 so this is a bug",
                pipeline.as_str()
            );
        }
    }
    Err(ServeStartError::ModelsUnusable {
        report: Box::new(report),
    })
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
            "enabled by a formula detector"
        } else {
            "disabled (no formula detector configured)"
        },
        formula_detector
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<none>".to_string())
    );
    let formula_blocking = snapshot.formula_blocking_names();
    println!(
        "serve: formula pipeline {}",
        if !snapshot.formula_in_plan() {
            "not part of this run's model plan (no formula detector is configured)".to_string()
        } else if formula_blocking.is_empty() {
            "complete (formula recognizer + detector are in this run's plan and verified)"
                .to_string()
        } else {
            format!(
                "incomplete: {} (the formula queue returns 409 until it is present; text OCR is \
                 unaffected)",
                formula_blocking.join(", ")
            )
        }
    );
    println!(
        "serve: startup model verification {}",
        if args.reverify_models {
            "cold (--reverify-models: every file this run uses was re-hashed at startup, and a \
             missing or corrupt file refuses to start)"
        } else {
            "on first use (the identity-keyed cache verifies each file the first time it is asked \
             for; pass --reverify-models to move that to startup and fail fast, or use \
             POST /api/models/reverify to force a fresh check while running)"
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
    use std::path::{Path, PathBuf};

    use rapid_ocr_rs::{EngineConfig, sha256_file};

    use super::{
        PAGE_TEMPLATE, PageError, ServeStartError, check_nonce_attributes, render_page_with,
        reverify_gate, strip_html_comments,
    };
    use crate::serve::model_plan::{ModelPlan, Pipeline};

    /// 一份"看起来像 ONNX"的夹具内容（protobuf 序言：字段 1 = `ir_version` = 7）。
    ///
    /// 没有声明摘要的文件按文档化规则判定，因此夹具必须过这一关。
    fn plausible_onnx_bytes(tag: &str) -> Vec<u8> {
        let mut bytes = vec![0x08, 0x07];
        bytes.extend_from_slice(tag.as_bytes());
        bytes
    }

    /// A1 的夹具：与 serve 测试同一形状的本地清单模型目录（det/rec/dict + 公式识别），
    /// `with_detector` 时再加一个**集合声明的**公式检测模型（`mfd.onnx`）。
    ///
    /// 所有文件都真的写到磁盘上，清单里的 `sha256` 是**真实**摘要——因此"健康"与"损坏"
    /// 的区别只有一个字节的来源，而不是靠状态字段。
    fn fixture_dir(name: &str, with_detector: bool) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/m1-review-evidence")
            .join(format!("reverify-{name}"));
        std::fs::create_dir_all(&dir).expect("create the fixture dir");
        let mut files: Vec<(&str, &str)> = vec![
            ("det.onnx", "detector"),
            ("rec.onnx", "recognizer"),
            ("dict.txt", "dictionary"),
            ("fx.onnx", "formula_recognizer"),
        ];
        if with_detector {
            files.push(("mfd.onnx", "formula_detector"));
        }
        let mut manifest = String::from(
            "{\"schema_version\":1,\"id\":\"reverify-set\",\"family\":\"PP-OCR\",\
             \"version\":\"v-test\",\"files\":[",
        );
        for (index, (file, role)) in files.iter().enumerate() {
            let path = dir.join(file);
            let body = if *file == "mfd.onnx" {
                plausible_onnx_bytes("reverify detector")
            } else {
                format!("reverify fixture {file}").into_bytes()
            };
            std::fs::write(&path, &body).expect("write the fixture");
            let sha = sha256_file(&path).expect("hash the fixture");
            if index > 0 {
                manifest.push(',');
            }
            manifest.push_str(&format!(
                "{{\"name\":\"{file}\",\"role\":\"{role}\",\"sha256\":\"{sha}\"}}"
            ));
        }
        manifest.push_str("]}");
        std::fs::write(dir.join("manifest.json"), manifest).expect("write the manifest");
        dir
    }

    /// 解析一份运行计划（`--formula-detector` 的值由调用方给出）。
    fn plan_for(dir: &Path, detector: Option<&Path>) -> ModelPlan {
        ModelPlan::resolve(dir, &EngineConfig::default(), detector).expect("model plan")
    }

    /// A1：健康的模型集 → 冷验证通过，**每个文件都在这一次真的算了一次摘要**
    /// （`force_verify_file` 不查缓存，因此这个数字不可能是"命中来的"）。
    ///
    /// 这一轮带**集合声明的公式检测模型**，因此计划 = 5 个文件（文本三个 + 公式识别 +
    /// 公式检测），三者都在 `files[]` 里并且都被真的读了一遍。
    #[test]
    fn the_startup_gate_verifies_every_file_the_run_uses_with_a_fresh_digest() {
        let dir = fixture_dir("healthy", true);
        let plan = plan_for(&dir, None);
        let report = reverify_gate(&plan).expect("healthy models must pass");

        assert_eq!(
            report.digests_computed(),
            report.files().len(),
            "one computed digest per file the run uses"
        );
        assert_eq!(
            report.files().len(),
            5,
            "text three + formula recognizer + formula detector: {:?}",
            report.files().iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        assert!(report.blocking().is_empty());
        for file in report.files() {
            assert!(
                file.sha256.is_some(),
                "{} must carry the digest this call computed",
                file.name
            );
            assert_eq!(
                file.state.as_str(),
                "present",
                "{} must be present: {:?}",
                file.name,
                file.state
            );
            assert_eq!(
                file.cause.as_str(),
                "first_sight",
                "a cold verification never reports a cache hit ({}): {:?}",
                file.name,
                file.cause
            );
        }
        // 目录里那个**不在**本轮使用范围里的文件也必须不在清单里（默认表的其它模型）。
        let names: Vec<&str> = report.files().iter().map(|f| f.name.as_str()).collect();
        assert!(!names.contains(&"manifest.json"), "{names:?}");
        assert_eq!(
            report.files_in(Pipeline::Formula).len(),
            2,
            "the formula pipeline is part of this run's plan"
        );
    }

    /// 需求 2（第二半）：公式**关闭**时，566 MB 的公式识别模型**不在计划里**、
    /// 因此 A1 一个字节都不读它（`digests_computed == 3`），而且它的损坏不影响启动。
    #[test]
    fn a_corrupt_formula_recognizer_is_out_of_scope_when_formula_is_disabled() {
        let dir = fixture_dir("formula-disabled", false);
        let plan = plan_for(&dir, None);
        // 公式识别模型的内容与集合声明的摘要不匹配（存在但"损坏"）。
        std::fs::write(dir.join("fx.onnx"), b"corrupted formula recognizer bytes")
            .expect("corrupt the formula recognizer");

        let report = reverify_gate(&plan).expect("formula is not part of this run's plan");
        assert_eq!(
            report.digests_computed(),
            3,
            "only the text pipeline's three files may be hashed: {:?}",
            report.files().iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        assert!(
            report.files_in(Pipeline::Formula).is_empty(),
            "the 566 MB formula recognizer must not be in the plan at all"
        );
        let names: Vec<&str> = report.files().iter().map(|f| f.name.as_str()).collect();
        assert!(!names.contains(&"fx.onnx"), "{names:?}");
    }

    /// 需求 1：公式检测模型被损坏时 `--reverify-models` **拒绝启动**，错误里点名
    /// 检测模型**和它所属的管线**，并且这一次真的为它算了一个摘要。
    #[test]
    fn the_startup_gate_fails_on_a_corrupt_formula_detector_and_names_it() {
        let dir = fixture_dir("corrupt-detector", true);
        // 检测模型来自集合声明，因此启动期就解析出它 → 公式管线属于这次运行。
        let plan = plan_for(&dir, None);
        std::fs::write(dir.join("mfd.onnx"), b"corrupted detector bytes")
            .expect("corrupt the detector");

        let error =
            reverify_gate(&plan).expect_err("a corrupt formula detector must refuse to start");
        let text = error.to_string();
        assert!(text.contains("mfd.onnx"), "{text}");
        assert!(text.contains("corrupt"), "{text}");
        assert!(
            text.contains("formula pipeline"),
            "the error must say which pipeline the file belongs to: {text}"
        );
        assert!(text.contains("--reverify-models"), "{text}");
        match error {
            ServeStartError::ModelsUnusable { report } => {
                let blocking: Vec<&str> = report
                    .blocking()
                    .iter()
                    .map(|file| file.name.as_str())
                    .collect();
                assert_eq!(
                    blocking,
                    vec!["mfd.onnx"],
                    "only the corrupt detector blocks"
                );
                let detector = report
                    .files()
                    .iter()
                    .find(|file| file.name == "mfd.onnx")
                    .expect("the detector is in this run's plan");
                assert_eq!(detector.pipeline, Pipeline::Formula);
                assert!(
                    detector.sha256.is_some(),
                    "this call must have computed its digest"
                );
                assert!(report.blocking_in(Pipeline::Text).is_empty());
            }
            other => panic!("expected ModelsUnusable, got {other:?}"),
        }
    }

    /// 需求 2（第一半）：公式**启用**时，损坏的公式识别模型同样让启动失败并点名它
    /// （`--reverify-models` 的契约是"验证这次运行的整份计划"）。
    #[test]
    fn the_startup_gate_fails_on_a_corrupt_formula_recognizer_when_formula_is_enabled() {
        let dir = fixture_dir("corrupt-formula-required", true);
        let plan = plan_for(&dir, None);
        std::fs::write(dir.join("fx.onnx"), b"corrupted formula recognizer bytes")
            .expect("corrupt the formula recognizer");

        let error = reverify_gate(&plan).expect_err("the formula model is part of the plan");
        let text = error.to_string();
        assert!(text.contains("fx.onnx"), "{text}");
        assert!(text.contains("formula pipeline"), "{text}");
        assert!(
            !text.contains("text pipeline:"),
            "the text pipeline is not the offender here: {text}"
        );
    }

    /// 需求 4：集合之外的 `--formula-detector`（**没有声明摘要**）也在计划里、被冷验证，
    /// 状态按文档化规则（存在 + 可读 + 像 ONNX）如实报告：算出的摘要必须写出来，
    /// 而 `declared_sha256` 为空。
    #[test]
    fn the_startup_gate_covers_an_external_detector_without_a_declared_digest() {
        let dir = fixture_dir("external-detector", false);
        // 模型目录**之外**的检测模型：不属于任何集合 → 没有可信摘要。
        let outside = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/m1-review-evidence/reverify-external-detector.onnx");
        std::fs::write(&outside, plausible_onnx_bytes("external detector")).expect("detector");
        let plan = plan_for(&dir, Some(&outside));

        let report = reverify_gate(&plan).expect("a plausible external detector passes");
        assert_eq!(
            report.files().len(),
            5,
            "text three + the formula recognizer + the external detector: {:?}",
            report.files().iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        assert_eq!(report.digests_computed(), 5);
        let detector = report
            .files()
            .iter()
            .find(|file| file.name == "reverify-external-detector.onnx")
            .expect("the external detector must be in this run's plan");
        assert_eq!(detector.pipeline, Pipeline::Formula);
        assert_eq!(detector.declared_sha256, None);
        assert!(
            detector.sha256.is_some(),
            "the digest of a file without a declared value is still reported: {}",
            detector.describe()
        );

        // 文档化的存在/可读/像 ONNX 规则：内容不是 ONNX → 拒绝启动并点名它。
        std::fs::write(&outside, b"not a model at all").expect("corrupt the detector");
        let error = reverify_gate(&plan).expect_err("a file that is not an ONNX model blocks");
        let text = error.to_string();
        assert!(text.contains("reverify-external-detector.onnx"), "{text}");
        assert!(text.contains("not a plausible ONNX model"), "{text}");
        assert!(text.contains("formula pipeline"), "{text}");
        std::fs::remove_file(&outside).ok();
    }

    /// A1：损坏的**文本**模型文件 → **拒绝启动**，错误按管线分组并点名那个文件。
    #[test]
    fn the_startup_gate_refuses_to_start_and_names_the_corrupt_file() {
        let dir = fixture_dir("corrupt", false);
        let plan = plan_for(&dir, None);
        // 文件仍然在（体积是否相同都无所谓）：内容与清单声明的摘要不匹配。
        std::fs::write(dir.join("rec.onnx"), b"corrupted recognizer bytes")
            .expect("corrupt the recognizer");

        let error = reverify_gate(&plan).expect_err("a corrupt plan model must refuse to start");
        let text = error.to_string();
        assert!(text.contains("rec.onnx"), "{text}");
        assert!(text.contains("corrupt"), "{text}");
        assert!(text.contains("--reverify-models"), "{text}");
        assert!(
            text.contains("text pipeline"),
            "the error must say which pipeline the file belongs to: {text}"
        );
        match error {
            ServeStartError::ModelsUnusable { report } => {
                let blocking: Vec<&str> = report
                    .blocking()
                    .iter()
                    .map(|file| file.name.as_str())
                    .collect();
                assert_eq!(blocking, vec!["rec.onnx"], "only the corrupt file blocks");
            }
            other => panic!("expected ModelsUnusable, got {other:?}"),
        }
    }

    /// A1：缺失的模型文件同样拒绝启动（不是只有"损坏"才拦）。
    #[test]
    fn the_startup_gate_refuses_a_missing_file_too() {
        let dir = fixture_dir("missing", false);
        let plan = plan_for(&dir, None);
        std::fs::remove_file(dir.join("dict.txt")).expect("remove the dictionary");

        let error = reverify_gate(&plan).expect_err("a missing plan model must refuse to start");
        let text = error.to_string();
        assert!(text.contains("dict.txt"), "{text}");
        assert!(text.contains("missing"), "{text}");
        assert!(text.contains("text pipeline"), "{text}");
    }

    // A1 的端到端形态见 `serve::tests`（那里有真实的进程边界与模型夹具）。

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
