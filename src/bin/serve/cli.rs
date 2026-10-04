//! `rapidocr serve` 的命令行面（§3）。
//!
//! M0c 只定义**结构、默认值与校验入口**，不含任何启动行为：M1 才把它接进
//! `rapidocr` 的子命令表并实现"未启用 `serve` feature 时返回可定位错误"。
//!
//! 三条必须在选项面上成立的约束：
//!
//! 1. **没有 `--host`**：监听地址硬编码为 `127.0.0.1`（§7.1）。选项清单由
//!    `cli::tests` 逐项枚举断言，`--host` 无法解析；
//! 2. **没有 `--ocr-workers`**：M1 固定 1 个 OCR worker（引擎 `&mut self`），
//!    不提供设了不生效的选项（§8.2）；
//! 3. **`--provider` 与 `--max-side` 没有 clap 默认值**：§3 冻结的优先级是
//!    **CLI flag > `--config` YAML > 内建默认**，若给它们写死 `default_value`，
//!    YAML 里的值会被静默丢弃、优先级规则不可能实现。它们的"默认"由内建默认
//!    （`cpu` / 库内 `max_side_len`）承担，见 [`ServeArgs::provider_preference`]。

use std::path::PathBuf;

use clap::{Args, ValueEnum};
use rapid_ocr_rs::{EngineConfig, ProviderPreference};

use super::limits::{
    DEFAULT_JOB_TTL_SECS, DEFAULT_MAX_BODY_MB, DEFAULT_MAX_CONSECUTIVE_FORMULA,
    DEFAULT_MAX_CONSECUTIVE_TEXT, DEFAULT_MAX_DOWNLOAD_MB, DEFAULT_MAX_EVAL_CASES,
    DEFAULT_MAX_EXPORT_MB, DEFAULT_MAX_QUEUE_FORMULA, DEFAULT_MAX_QUEUE_TEXT,
    DEFAULT_MAX_RESULT_MB, DEFAULT_MAX_RETAINED, DEFAULT_MAX_RETAINED_MB, DEFAULT_MAX_TOMBSTONES,
    DEFAULT_PORT, RawServeLimits,
};
use super::state::StartupConfigError;

/// `--provider` 的三个取值（§3；CANN 不是 Windows x64 目标，已整体删除）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProviderChoice {
    Cpu,
    Directml,
    Cuda,
}

impl ProviderChoice {
    /// 转成库的 provider 偏好（单设备场景下 `device_id = 0`）。
    pub fn preference(self) -> ProviderPreference {
        match self {
            Self::Cpu => ProviderPreference::Cpu,
            Self::Directml => ProviderPreference::DirectMl { device_id: 0 },
            Self::Cuda => ProviderPreference::Cuda { device_id: 0 },
        }
    }
}

/// `rapidocr serve` 的全部选项（§3 的清单，逐项对应）。
#[derive(Debug, Clone, Args)]
pub struct ServeArgs {
    /// 监听端口；被占用时报可定位错误，**不静默换端口**（§3）。
    #[arg(long, default_value_t = DEFAULT_PORT, value_name = "PORT")]
    pub port: u16,

    /// 模型目录；缺省用 `model_store::default_model_store_dir()`。
    #[arg(long = "model-dir", value_name = "DIR")]
    pub model_dir: Option<PathBuf>,

    /// `EngineConfig` YAML（与 `run`/`evaluate` 一致）。
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// `cpu | directml | cuda`；**不给** clap 默认值，缺省时由 YAML 或内建默认决定。
    #[arg(long, value_name = "PROV")]
    pub provider: Option<ProviderChoice>,

    /// 允许 provider 不可用时回退 CPU（默认不允许，§7.5）。
    #[arg(long = "allow-provider-fallback")]
    pub allow_provider_fallback: bool,

    /// 覆盖 `max_side_len`。
    #[arg(long = "max-side", value_name = "N")]
    pub max_side: Option<usize>,

    /// 允许下载模型（仍需 token，§7.2）。
    #[arg(long = "allow-download")]
    pub allow_download: bool,

    /// 追加一个允许下载的 host（可重复；默认仅编译期白名单，§6.1）。
    #[arg(long = "allow-download-host", value_name = "HOST")]
    pub allow_download_host: Vec<String>,

    /// 启动后打开系统默认浏览器。
    #[arg(long)]
    pub open: bool,

    /// 请求体上限（MiB，默认 32）。
    #[arg(long = "max-body-mb", default_value_t = DEFAULT_MAX_BODY_MB, value_name = "N")]
    pub max_body_mb: u64,

    /// 单个结果序列化上限（MiB，默认 8，§4.6）。
    #[arg(long = "max-result-mb", default_value_t = DEFAULT_MAX_RESULT_MB, value_name = "N")]
    pub max_result_mb: u64,

    /// 单个导出文档上限（含内嵌图片，MiB，默认 32，§9.5）。
    #[arg(long = "max-export-mb", default_value_t = DEFAULT_MAX_EXPORT_MB, value_name = "N")]
    pub max_export_mb: u64,

    /// 单文件下载上限（MiB，默认 1024，必须大于 566 MB 的公式模型，§6.2）。
    #[arg(long = "max-download-mb", default_value_t = DEFAULT_MAX_DOWNLOAD_MB, value_name = "N")]
    pub max_download_mb: u64,

    /// 普通 OCR 队列上限（默认 4）。
    #[arg(long = "max-queue-text", default_value_t = DEFAULT_MAX_QUEUE_TEXT, value_name = "N")]
    pub max_queue_text: usize,

    /// 公式 OCR 队列上限（默认 2）。
    #[arg(long = "max-queue-formula", default_value_t = DEFAULT_MAX_QUEUE_FORMULA, value_name = "N")]
    pub max_queue_formula: usize,

    /// 普通任务连续处理上限（默认 4，保公式不被饿死，§8.3）。
    #[arg(
        long = "max-consecutive-text",
        default_value_t = DEFAULT_MAX_CONSECUTIVE_TEXT,
        value_name = "N"
    )]
    pub max_consecutive_text: usize,

    /// 公式任务连续处理上限（默认 1，保普通 OCR 不被饿死，§8.3）。
    #[arg(
        long = "max-consecutive-formula",
        default_value_t = DEFAULT_MAX_CONSECUTIVE_FORMULA,
        value_name = "N"
    )]
    pub max_consecutive_formula: usize,

    /// 终态任务保留数上限（默认 32）。
    #[arg(long = "max-retained", default_value_t = DEFAULT_MAX_RETAINED, value_name = "N")]
    pub max_retained: usize,

    /// 终态任务占用字节上限（MiB，默认 64）。
    #[arg(long = "max-retained-mb", default_value_t = DEFAULT_MAX_RETAINED_MB, value_name = "N")]
    pub max_retained_mb: u64,

    /// 已淘汰任务 ID 记录上限（默认 256，§4.5）。
    #[arg(long = "max-tombstones", default_value_t = DEFAULT_MAX_TOMBSTONES, value_name = "N")]
    pub max_tombstones: usize,

    /// 终态任务与 tombstone 保留时长（秒，默认 600）。
    #[arg(long = "job-ttl-secs", default_value_t = DEFAULT_JOB_TTL_SECS, value_name = "N")]
    pub job_ttl_secs: u64,

    /// 页面公式检测模型（`pix2text-mfd-1.5.onnx`；给出即启用公式队列，§4.2、§10.8）。
    ///
    /// 公式**识别**模型来自模型集（`formula_recognizer`，默认表可下载）；检测模型在
    /// `FormulaPolicy` 里是可选的、且没有可信的公开下载来源，因此只能显式给出。
    #[arg(long = "formula-detector", value_name = "ONNX")]
    pub formula_detector: Option<PathBuf>,

    /// `POST /api/evaluate` 一张清单最多评估多少张图（默认 32）。
    #[arg(long = "max-eval-cases", default_value_t = DEFAULT_MAX_EVAL_CASES, value_name = "N")]
    pub max_eval_cases: usize,

    /// `POST /api/evaluate` 的沙箱目录（M1 评审 P2-3）。
    ///
    /// 端点接收的是**本机路径**（清单 + 清单里的图片），因此读取范围必须显式配置：
    /// 给出本开关时，清单与它引用的每张图都必须规范化到该目录内（拒绝 `..`、绝对路径
    /// 逃逸与符号链接逃逸）；**不给时 `/api/evaluate` 整体拒绝**并给出可定位理由。
    #[arg(long = "eval-root", value_name = "DIR")]
    pub eval_root: Option<PathBuf>,

    /// 启动时**冷验证**这次运行会用到的每一个模型文件，任缺失/损坏即拒绝启动。
    ///
    /// 与"清缓存"无关（进程内的校验缓存本来就是空的）：它的价值是把验证从**首次使用**
    /// 移到**启动期**，并在文件缺失或损坏时**拒绝启动**，而不是等到第一次 OCR 才发现。
    /// 校验范围是这次运行真的会加载的文件（文本管线 + 配置了检测模型时的公式管线），
    /// 不是默认表里的每一个文件。
    #[arg(long = "reverify-models")]
    pub reverify_models: bool,
}

impl ServeArgs {
    /// CLI 上的原始取值（交给 [`RawServeLimits::validate`] 做换算与校验）。
    pub fn raw_limits(&self) -> RawServeLimits {
        RawServeLimits {
            max_body_mb: self.max_body_mb,
            max_result_mb: self.max_result_mb,
            max_export_mb: self.max_export_mb,
            max_download_mb: self.max_download_mb,
            max_queue_text: self.max_queue_text,
            max_queue_formula: self.max_queue_formula,
            max_consecutive_text: self.max_consecutive_text,
            max_consecutive_formula: self.max_consecutive_formula,
            max_retained: self.max_retained,
            max_retained_mb: self.max_retained_mb,
            max_tombstones: self.max_tombstones,
            job_ttl_secs: self.job_ttl_secs,
            max_eval_cases: self.max_eval_cases,
        }
    }

    /// CLI 给出的 provider 覆盖（`None` = 未给出，保留 YAML 值，§3）。
    pub fn provider_preference(&self) -> Option<ProviderPreference> {
        self.provider.map(ProviderChoice::preference)
    }

    /// CLI 的 `--allow-provider-fallback`（默认 `false`，§7.5）。
    pub fn allow_provider_fallback(&self) -> bool {
        self.allow_provider_fallback
    }

    /// 生效的模型目录：CLI > 库默认。
    pub fn model_dir(&self) -> PathBuf {
        self.model_dir
            .clone()
            .unwrap_or_else(rapid_ocr_rs::default_model_store_dir)
    }

    /// 读取 `EngineConfig`（CLI 的 `--config` 优先，否则库内建默认）。
    ///
    /// 这里**只**负责"取到配置"，覆盖与校验交给
    /// [`super::state::ServeStartup::validate`]（§3 的优先级规则在那里实现）。
    /// 读取/解析失败会带上 `--config` 的路径，因此是**可定位**的启动期错误。
    pub fn engine_config(&self) -> Result<EngineConfig, StartupConfigError> {
        match &self.config {
            Some(path) => EngineConfig::from_yaml_file(path).map_err(|source| {
                StartupConfigError::ConfigFile {
                    path: path.clone(),
                    source,
                }
            }),
            None => Ok(EngineConfig::default()),
        }
    }

    /// 本文档冻结的默认值是否被改动过（M1 的启动日志用它说明"哪个值生效"）。
    pub fn uses_documented_defaults(&self) -> bool {
        self.raw_limits() == RawServeLimits::default() && self.provider.is_none()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use clap::Parser as _;
    use rapid_ocr_rs::ProviderPreference;

    use super::{ProviderChoice, ServeArgs};
    use crate::serve::limits::{
        DEFAULT_ALLOW_DOWNLOAD, DEFAULT_ALLOW_PROVIDER_FALLBACK, DEFAULT_JOB_TTL_SECS,
        DEFAULT_MAX_BODY_MB, DEFAULT_MAX_CONSECUTIVE_FORMULA, DEFAULT_MAX_CONSECUTIVE_TEXT,
        DEFAULT_MAX_DOWNLOAD_MB, DEFAULT_MAX_EVAL_CASES, DEFAULT_MAX_EXPORT_MB,
        DEFAULT_MAX_QUEUE_FORMULA, DEFAULT_MAX_QUEUE_TEXT, DEFAULT_MAX_RESULT_MB,
        DEFAULT_MAX_RETAINED, DEFAULT_MAX_RETAINED_MB, DEFAULT_MAX_TOMBSTONES, DEFAULT_PORT, MIB,
    };

    /// §3 的完整选项名清单（**唯一**的一处枚举）。
    const DOCUMENTED_OPTION_NAMES: [&str; 25] = [
        "port",
        "model-dir",
        "config",
        "provider",
        "allow-provider-fallback",
        "max-side",
        "allow-download",
        "allow-download-host",
        "open",
        "max-body-mb",
        "max-result-mb",
        "max-export-mb",
        "max-download-mb",
        "max-queue-text",
        "max-queue-formula",
        "max-consecutive-text",
        "max-consecutive-formula",
        "max-retained",
        "max-retained-mb",
        "max-tombstones",
        "job-ttl-secs",
        // M4：公式队列的检测模型与评估用例上限（§3、§4.2）。
        "formula-detector",
        "max-eval-cases",
        // 评审 P2-3：评估的沙箱根（不给即拒绝整个端点）。
        "eval-root",
        // A1：启动期冷验证这次运行会用到的模型文件，缺失/损坏即拒绝启动。
        "reverify-models",
    ];

    /// 测试用的最小 `Parser` 包装。
    ///
    /// `ServeArgs` 只实现 `clap::Args`（M1 会把它 flatten 进 `Command::Serve`），
    /// `try_parse_from` 属于 `clap::Parser`，因此单测用一个同名的命令包住它，
    /// 走的是与 M1 完全相同的 clap 解析路径。
    #[derive(clap::Parser)]
    #[command(name = "serve")]
    struct ServeCli {
        #[command(flatten)]
        args: ServeArgs,
    }

    fn try_parse(arguments: &[&str]) -> Result<ServeArgs, clap::Error> {
        let mut argv = vec!["serve"];
        argv.extend_from_slice(arguments);
        ServeCli::try_parse_from(argv).map(|cli| cli.args)
    }

    fn parse(arguments: &[&str]) -> ServeArgs {
        try_parse(arguments).expect("the documented options must parse")
    }

    /// 选项面逐项枚举：既不多也不少，且 `--host` / `--ocr-workers` 不存在。
    #[test]
    fn the_option_surface_is_exactly_the_documented_list() {
        let command = <ServeArgs as clap::Args>::augment_args(clap::Command::new("serve"));
        let names: BTreeSet<String> = command
            .get_arguments()
            .filter_map(|argument| argument.get_long().map(str::to_string))
            // clap 自带的帮助/版本次于 `build()` 才注入；这里显式排除以免误判。
            .filter(|name| name != "help" && name != "version")
            .collect();
        let expected: BTreeSet<String> = DOCUMENTED_OPTION_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        assert_eq!(names, expected, "the serve option surface changed");

        assert!(!names.contains("host"), "§7.1 forbids any address option");
        assert!(
            !names.contains("ocr-workers"),
            "§8.2 forbids a worker-count option in M1"
        );
    }

    /// 不只是"名字不在清单里"：这两个选项必须**无法解析**。
    #[test]
    fn neither_host_nor_ocr_workers_can_be_parsed() {
        assert!(
            try_parse(&["--host", "0.0.0.0"]).is_err(),
            "there must be no way to bind another address"
        );
        assert!(
            try_parse(&["--host"]).is_err(),
            "--host must not even exist as a flag"
        );
        assert!(
            try_parse(&["--ocr-workers", "4"]).is_err(),
            "--ocr-workers must not exist"
        );
        // 其他"看起来能改监听地址"的写法同样不存在。
        for option in ["--address", "--bind", "--listen", "--ip"] {
            assert!(try_parse(&[option, "0.0.0.0"]).is_err(), "{option}");
        }
    }

    /// 每个默认值都必须与文档一致（并逐项对照 §3 的数字）。
    #[test]
    fn every_default_matches_the_document() {
        let args = parse(&[]);
        assert_eq!(args.port, DEFAULT_PORT);
        assert_eq!(args.port, 8760);
        assert_eq!(args.model_dir, None, "default comes from the library");
        assert_eq!(args.config, None);
        assert_eq!(
            args.provider, None,
            "no clap default on purpose: CLI > YAML > builtin (§3)"
        );
        assert_eq!(args.provider_preference(), None);
        assert!(!args.allow_provider_fallback);
        assert_eq!(args.max_side, None);
        assert!(!args.allow_download);
        assert!(args.allow_download_host.is_empty());
        assert!(!args.open);
        // §3 的两个布尔默认值同样是文档冻结的一部分（编译期常量断言）。
        const {
            assert!(!DEFAULT_ALLOW_DOWNLOAD);
            assert!(!DEFAULT_ALLOW_PROVIDER_FALLBACK);
        }

        assert_eq!(args.max_body_mb, DEFAULT_MAX_BODY_MB);
        assert_eq!(args.max_result_mb, DEFAULT_MAX_RESULT_MB);
        assert_eq!(args.max_export_mb, DEFAULT_MAX_EXPORT_MB);
        assert_eq!(args.max_download_mb, DEFAULT_MAX_DOWNLOAD_MB);
        assert_eq!(args.max_queue_text, DEFAULT_MAX_QUEUE_TEXT);
        assert_eq!(args.max_queue_formula, DEFAULT_MAX_QUEUE_FORMULA);
        assert_eq!(args.max_consecutive_text, DEFAULT_MAX_CONSECUTIVE_TEXT);
        assert_eq!(
            args.max_consecutive_formula,
            DEFAULT_MAX_CONSECUTIVE_FORMULA
        );
        assert_eq!(args.max_retained, DEFAULT_MAX_RETAINED);
        assert_eq!(args.max_retained_mb, DEFAULT_MAX_RETAINED_MB);
        assert_eq!(args.max_tombstones, DEFAULT_MAX_TOMBSTONES);
        assert_eq!(args.job_ttl_secs, DEFAULT_JOB_TTL_SECS);
        // M4：公式检测模型默认**不给**（路由默认关闭，§10.8），用例上限是文档默认值。
        assert_eq!(args.formula_detector, None);
        assert_eq!(args.max_eval_cases, DEFAULT_MAX_EVAL_CASES);
        // 评审 P2-3：默认**没有**沙箱 → `/api/evaluate` 整体拒绝。
        assert_eq!(args.eval_root, None);

        // §3 表格里的数字本身（防止常量被改歪还自洽）。
        assert_eq!(args.max_body_mb, 32);
        assert_eq!(args.max_result_mb, 8);
        assert_eq!(args.max_export_mb, 32);
        assert_eq!(args.max_download_mb, 1024);
        assert_eq!(args.max_queue_text, 4);
        assert_eq!(args.max_queue_formula, 2);
        assert_eq!(args.max_consecutive_text, 4);
        assert_eq!(args.max_consecutive_formula, 1);
        assert_eq!(args.max_retained, 32);
        assert_eq!(args.max_retained_mb, 64);
        assert_eq!(args.max_tombstones, 256);
        assert_eq!(args.job_ttl_secs, 600);
        assert_eq!(args.max_eval_cases, 32);

        // 默认值经同一套换算变成字节。
        let limits = args.raw_limits().validate().expect("defaults are valid");
        assert_eq!(limits.max_body_bytes, 32 * MIB);
        assert_eq!(limits.max_download_bytes, 1024 * MIB);
        assert_eq!(limits.job_ttl_ms, 600_000);
        assert!(args.uses_documented_defaults());
    }

    #[test]
    fn every_option_can_be_set_from_the_command_line() {
        let args = parse(&[
            "--port",
            "9000",
            "--model-dir",
            "D:\\models",
            "--config",
            "cfg.yaml",
            "--provider",
            "directml",
            "--allow-provider-fallback",
            "--max-side",
            "1600",
            "--allow-download",
            "--allow-download-host",
            "example.org",
            "--allow-download-host",
            "mirror.example.org",
            "--open",
            "--max-body-mb",
            "16",
            "--max-result-mb",
            "4",
            "--max-export-mb",
            "48",
            "--max-download-mb",
            "2048",
            "--max-queue-text",
            "8",
            "--max-queue-formula",
            "3",
            "--max-consecutive-text",
            "6",
            "--max-consecutive-formula",
            "2",
            "--max-retained",
            "64",
            "--max-retained-mb",
            "128",
            "--max-tombstones",
            "512",
            "--job-ttl-secs",
            "1200",
            "--formula-detector",
            "D:\\models\\pix2text-mfd-1.5.onnx",
            "--max-eval-cases",
            "8",
            "--eval-root",
            "D:\\eval-root",
            "--reverify-models",
        ]);
        assert_eq!(args.port, 9000);
        assert_eq!(
            args.model_dir.as_deref(),
            Some(std::path::Path::new("D:\\models"))
        );
        assert_eq!(
            args.config.as_deref(),
            Some(std::path::Path::new("cfg.yaml"))
        );
        assert_eq!(args.provider, Some(ProviderChoice::Directml));
        assert_eq!(
            args.provider_preference(),
            Some(ProviderPreference::DirectMl { device_id: 0 })
        );
        assert!(args.allow_provider_fallback);
        assert_eq!(args.max_side, Some(1600));
        assert!(args.allow_download);
        assert_eq!(
            args.allow_download_host,
            vec!["example.org".to_string(), "mirror.example.org".to_string()]
        );
        assert!(args.open);
        let limits = args.raw_limits().validate().expect("valid");
        assert_eq!(limits.max_body_bytes, 16 * MIB);
        assert_eq!(limits.max_result_bytes, 4 * MIB);
        assert_eq!(limits.max_export_bytes, 48 * MIB);
        assert_eq!(limits.max_download_bytes, 2048 * MIB);
        assert_eq!(limits.max_queue_text, 8);
        assert_eq!(limits.max_queue_formula, 3);
        assert_eq!(limits.max_consecutive_text, 6);
        assert_eq!(limits.max_consecutive_formula, 2);
        assert_eq!(limits.max_retained, 64);
        assert_eq!(limits.max_retained_bytes, 128 * MIB);
        assert_eq!(limits.max_tombstones, 512);
        assert_eq!(limits.job_ttl_ms, 1_200_000);
        assert_eq!(limits.max_eval_cases, 8);
        assert_eq!(
            args.formula_detector.as_deref(),
            Some(std::path::Path::new("D:\\models\\pix2text-mfd-1.5.onnx"))
        );
        assert_eq!(
            args.eval_root.as_deref(),
            Some(std::path::Path::new("D:\\eval-root"))
        );
        assert!(args.reverify_models);
        assert!(!args.uses_documented_defaults());
    }

    /// A1：`--reverify-models` 是**开关**（默认关闭），不是需要取值的选项。
    #[test]
    fn the_startup_reverification_flag_defaults_to_off_and_takes_no_value() {
        assert!(!parse(&[]).reverify_models);
        assert!(parse(&["--reverify-models"]).reverify_models);
        assert!(
            try_parse(&["--reverify-models=true"]).is_err(),
            "a boolean flag must not accept a value"
        );
        assert!(
            try_parse(&["--reverify-models", "1"]).is_err(),
            "the value after the flag would be an unexpected positional argument"
        );
    }

    #[test]
    fn the_three_provider_names_are_the_documented_ones() {
        assert_eq!(
            parse(&["--provider", "cpu"]).provider,
            Some(ProviderChoice::Cpu)
        );
        assert_eq!(
            parse(&["--provider", "directml"]).provider,
            Some(ProviderChoice::Directml)
        );
        assert_eq!(
            parse(&["--provider", "cuda"]).provider,
            Some(ProviderChoice::Cuda)
        );
        assert_eq!(
            parse(&["--provider", "cpu"]).provider_preference(),
            Some(ProviderPreference::Cpu)
        );
        assert_eq!(
            parse(&["--provider", "cuda"]).provider_preference(),
            Some(ProviderPreference::Cuda { device_id: 0 })
        );
        // 已删除的 CANN 不再是合法取值。
        assert!(try_parse(&["--provider", "cann"]).is_err());
        assert!(try_parse(&["--provider", "auto"]).is_err());
    }

    /// `--max-body-mb=0` 能被解析（clap 不管语义），但必须在校验层被**可定位**地拒绝。
    #[test]
    fn zero_mib_limits_parse_but_fail_validation_with_the_flag_name() {
        let args = parse(&["--max-body-mb", "0"]);
        let error = args
            .raw_limits()
            .validate()
            .expect_err("--max-body-mb=0 must not reach the runtime");
        assert_eq!(error.field(), "--max-body-mb");
        assert!(error.to_string().contains("--max-body-mb=0"), "{error}");

        let args = parse(&["--job-ttl-secs", "0"]);
        assert_eq!(
            args.raw_limits().validate().expect_err("zero TTL").field(),
            "--job-ttl-secs"
        );

        // M4：评估用例上限同样必须 ≥ 1（0 个用例的评估没有意义，是配置错误）。
        let args = parse(&["--max-eval-cases", "0"]);
        assert_eq!(
            args.raw_limits()
                .validate()
                .expect_err("zero eval cases")
                .field(),
            "--max-eval-cases"
        );
    }

    /// 荒谬的大整数要么被 clap 拒绝（超出 u64），要么被乘法溢出检查拒绝；**都不会 wrap**。
    #[test]
    fn absurd_mib_values_are_rejected_and_never_wrap() {
        // 超出 u64 的字面量：clap 在解析期拒绝，并且点名开关。
        let error = try_parse(&["--max-body-mb", "999999999999999999999999999999"])
            .expect_err("an out-of-range integer must be rejected");
        let text = error.to_string();
        assert!(text.contains("--max-body-mb"), "{text}");

        // 能放进 u64，但乘 1 MiB 会溢出：由校验层拒绝。
        let args = parse(&["--max-body-mb", "17592186044416"]);
        let error = args
            .raw_limits()
            .validate()
            .expect_err("2^44 MiB overflows u64 bytes");
        assert_eq!(error.field(), "--max-body-mb");
        assert!(error.reason().contains("overflows"), "{error}");

        // 负数/非数字同样在解析期被拒绝。
        assert!(try_parse(&["--max-body-mb", "-1"]).is_err());
        assert!(try_parse(&["--port", "abc"]).is_err());
        assert!(try_parse(&["--port", "70000"]).is_err());
        assert!(try_parse(&["--max-side", "0x10"]).is_err());
    }

    #[test]
    fn the_model_dir_default_comes_from_the_library() {
        let args = parse(&[]);
        assert_eq!(args.model_dir(), rapid_ocr_rs::default_model_store_dir());
        let explicit = parse(&["--model-dir", "D:\\m"]);
        assert_eq!(explicit.model_dir(), std::path::PathBuf::from("D:\\m"));
    }

    #[test]
    fn a_missing_config_file_is_a_locatable_startup_error() {
        let args = parse(&["--config", "definitely-not-here.yaml"]);
        let error = args
            .engine_config()
            .expect_err("a missing config must fail");
        assert!(
            error.to_string().contains("definitely-not-here.yaml"),
            "{error}"
        );
    }
}
