//! `rapidocr serve` 的服务端核心：HTTP 层、静态页、任务层与模型下载（M2）。
//!
//! # 分层（§2.1）
//!
//! HTTP 依赖只允许出现在二进制侧，库保持零网络/零 UI 依赖：
//!
//! ```text
//! rapidocr (bin)
//!   └── serve 子命令（feature = "serve"，非 default）
//!         ├── http 层：路由 / 准入顺序 / ServeError → 状态码 / 安全头     ← http.rs
//!         ├── 静态页：include_str!("web/index.html") + nonce/token 注入    ← run.rs
//!         ├── job 层：双队列 + 固定 worker + 有界结果存储 + tombstone       ← queue/jobs/server
//!         ├── download 层：独立 worker + 可注入执行体 + 文件边界取消        ← download.rs
//!         └── rapid-ocr-rs（库，零 HTTP 依赖）
//!               ModelSet / model_store（加固） / ImageInput → OcrRequest → OcrOutput
//! ```
//!
//! # 模块
//!
//! | 模块 | 内容 | 文档 |
//! | --- | --- | --- |
//! | [`limits`] | MiB→字节换算与全部取值校验（唯一实现） | §3、§4.4、§4.6、§6.2 |
//! | [`error`] | `ServeError`、状态码/`code` 映射、JSON 错误体 | §11.1 |
//! | [`state`] | `ServiceState` / `EngineState` 状态机、启动期配置校验 | §7.5、§7.6 |
//! | [`queue`] | 双队列容量与双向公平调度 | §8.2、§8.3 |
//! | [`jobs`] | 有界任务存储、TTL、tombstone、404/410、下载进度与取消 | §4.3、§4.5、§6.6 |
//! | [`security`] | 硬编码 loopback、Host/Origin、令牌、注入契约 | §7.1–§7.3、§9 |
//! | [`admit`] | 准入顺序、有界读取、字节账本 | §4.4 |
//! | [`cli`] | `serve` 的选项面与默认值（无行为） | §3 |
//! | [`model_plan`] | 模型集的单一来源解析、逐文件状态、引擎路径绑定 | §5.3、§5.4、§7.6 |
//! | [`engine`] | `OcrBackend` + 引擎工厂（测试可替换） | §8.2 |
//! | [`results`] | 有界结果存储与有界序列化 | §4.5、§4.6 |
//! | [`export`] | 标注 PNG、base64、JSON/Markdown/HTML 导出文档与时间账本 | §4.2、§9.5 |
//! | [`download`] | 独立下载 worker + 可注入执行体 + `--allow-download-host` 校验 | §8.1、§6 |
//! | [`evaluate`] | `POST /api/evaluate`：复用库的 `evaluation`（CER/精确匹配） | §4.2、§12 |
//! | [`flowlog`] | 流动日志：请求行 + 任务生命周期行（`--log-level`/`RAPID_OCR_SERVE_LOG`，默认关闭） | §17 |
//! | [`server`] | 运行期核心：共享状态、worker、TTL 清理、端点语义、惰性建引擎 | §4、§7.6、§8 |
//! | [`http`] | `tiny_http` 接线：路由、准入、响应头 | §4.2、§4.4、§7 |
//! | [`run`] | 启动编排、静态页注入、启动日志、`--open` | §3、§7.1、§7.6、§9 |
//! | [`tests`] | 真实绑定端口的端到端测试（HTTP + 双队列 + 安全 + M2 下载） | §12 |
//!
//! # M2 的范围（模型管理）
//!
//! `POST /api/models/download` 现在是**真实**任务：集合严格按 `set_id` 解析（无 `sets[0]`
//! 回落），库的加固下载器（[`rapid_ocr_rs::download_model_set_observed`]）逐文件下载，
//! 进度写进任务存储，取消在**文件边界**生效（§6.6）；`POST /api/engine/reload` 是
//! `EngineStateMachine::{begin_loading, models_still_missing}` 的生产者，下载完成后**不**
//! 自动建引擎，而是在下一次 `POST /api/ocr` 或显式 reload 时惰性创建。
//!
//! # HTTP 依赖的边界（M1 的硬约束）
//!
//! `tiny_http` 是 `optional` 依赖且不进 `default`；它**只**能在 [`http`] 里被引用。
//! 下面的 `dependency_boundary` 测试会扫描整个 `serve/` 子树的代码行并断言这一点，
//! 同时断言 `Cargo.toml` 里它是 `optional = true` 且 `serve` feature 通过 `dep:`
//! 引入——§12 的"依赖隔离"因此有编译期与源码级两条独立证据。

mod admit;
mod cli;
mod download;
mod engine;
mod error;
mod evaluate;
mod export;
mod flowlog;
mod http;
mod jobs;
mod limits;
mod model_plan;
mod queue;
mod results;
mod run;
mod security;
mod server;
mod state;

/// `serve` 子命令的选项面（rapidocr.rs 的子命令表要用）。
pub(crate) use cli::ServeArgs;
/// `serve` 的启动入口与启动期错误（rapidocr.rs 只负责把错误打印出来）。
pub(crate) use run::run as serve_main;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod dependency_boundary {
    use std::path::{Path, PathBuf};

    /// 本子树里**唯一**允许引用 `tiny_http` 的文件。
    const ONLY_HTTP_MODULE: &str = "http.rs";

    /// M1 的依赖边界：库零 HTTP 依赖，`tiny_http` 只出现在 `serve/http.rs`，
    /// 且在 `Cargo.toml` 里是 optional、由 `serve` feature 用 `dep:` 引入。
    ///
    /// M0c 的旧断言（"整个 serve 子树里不得出现 HTTP 库引用"）在那个阶段是对的：
    /// 当时 HTTP 层还没写。M1 把它替换成**更强**的版本：不是"哪里都没有"，而是
    /// "只允许在这一个文件里、且依赖必须 optional 且不进 default"。
    #[test]
    fn the_http_dependency_lives_only_in_the_http_module() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let directory = root.join("src/bin/serve");
        let mut sources = Vec::new();
        collect_sources(&directory, &mut sources);
        assert!(
            sources.len() >= 14,
            "expected the M1 modules, found {sources:?}"
        );

        let needle = ["tiny", "_http"].concat();
        for path in &sources {
            let text = std::fs::read_to_string(path).expect("a serve source file must be readable");
            let code = strip_line_comments(&text);
            if path
                .file_name()
                .is_some_and(|name| name == ONLY_HTTP_MODULE)
            {
                assert!(
                    code.contains(&needle),
                    "the HTTP module must be the one that uses the HTTP library"
                );
                continue;
            }
            assert!(
                !code.contains(&needle),
                "only serve/{ONLY_HTTP_MODULE} may reference the HTTP library: {} does",
                path.display()
            );
        }

        // 库侧（`src/` 下除 `src/bin/`）不得出现同一个引用。
        let mut library_sources = Vec::new();
        collect_sources(&root.join("src"), &mut library_sources);
        for path in &library_sources {
            if path.starts_with(root.join("src/bin")) {
                continue;
            }
            let text = std::fs::read_to_string(path).expect("a library source must be readable");
            assert!(
                !strip_line_comments(&text).contains(&needle),
                "the library must stay HTTP-free: {} references it",
                path.display()
            );
        }
    }

    /// `Cargo.toml` 的依赖隔离：optional + 只能由 `serve` 引入，且 `serve` 不在 default。
    #[test]
    fn the_http_dependency_is_optional_and_outside_the_default_feature() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
            .expect("the crate manifest must be readable");
        let name = ["tiny", "_http"].concat();

        let mut in_dependencies = false;
        let mut dependency = None;
        let mut default_features = None;
        let mut serve_feature = None;
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_dependencies = trimmed == "[dependencies]";
                continue;
            }
            if in_dependencies && !trimmed.starts_with('#') && trimmed.starts_with(&name) {
                dependency = Some(trimmed.to_string());
            }
            if let Some(value) = trimmed.strip_prefix("default =") {
                default_features = Some(value.to_string());
            }
            if let Some(value) = trimmed.strip_prefix("serve =") {
                serve_feature = Some(value.to_string());
            }
        }

        let dependency = dependency.expect("the HTTP library must be a dependency");
        assert!(
            dependency.contains("optional = true"),
            "the HTTP library must be optional: {dependency}"
        );
        let default_features = default_features.expect("a `default` feature list must exist");
        assert!(
            !default_features.contains(&name),
            "the HTTP library must not be in `default`: {default_features}"
        );
        let serve_feature = serve_feature.expect("a `serve` feature must exist");
        assert!(
            serve_feature.contains(&format!("dep:{name}")),
            "only the `serve` feature may enable the HTTP library: {serve_feature}"
        );
    }

    /// 去掉行注释。本 crate 只用 `//` / `///` / `//!` 三种注释，没有块注释。
    fn strip_line_comments(text: &str) -> String {
        text.lines()
            .map(|line| match line.find("//") {
                Some(index) => &line[..index],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn collect_sources(directory: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(directory).expect("the directory must exist") {
            let path = entry.expect("readable entry").path();
            if path.is_dir() {
                collect_sources(&path, out);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                out.push(path);
            }
        }
    }
}
