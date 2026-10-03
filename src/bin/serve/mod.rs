//! `rapidocr serve` 的服务端核心（M0c：纯逻辑 + 单元测试 + feature 骨架）。
//!
//! # 分层（§2.1）
//!
//! HTTP 依赖只允许出现在二进制侧，库保持零网络/零 UI 依赖：
//!
//! ```text
//! rapidocr (bin)
//!   └── serve 子命令（feature = "serve"，非 default）
//!         ├── http 层：路由 / 准入顺序 / ServeError → 状态码 / 安全头   ← M1
//!         ├── 静态页：include_str! + nonce/token 注入                  ← M1
//!         ├── job 层：双队列 + 固定 worker + 有界结果存储 + tombstone    ← 本模块（纯逻辑）
//!         └── download 层：独立 worker + 单飞 + 加固下载器              ← M0b/M2
//! ```
//!
//! # M0c 的范围
//!
//! 本模块**只**包含可以脱离 HTTP、线程与文件系统单测的纯逻辑：
//!
//! | 模块 | 内容 | 文档 |
//! | --- | --- | --- |
//! | [`limits`] | MiB→字节换算与全部取值校验（唯一实现） | §3、§4.4、§4.6、§6.2 |
//! | [`error`] | `ServeError`、状态码/`code` 映射、JSON 错误体 | §11.1 |
//! | [`state`] | `ServiceState` / `EngineState` 状态机、启动期配置校验 | §7.5、§7.6 |
//! | [`queue`] | 双队列容量与双向公平调度 | §8.2、§8.3 |
//! | [`jobs`] | 有界任务存储、TTL、tombstone、404/410 | §4.3、§4.5 |
//! | [`security`] | 硬编码 loopback、Host/Origin、令牌、注入契约 | §7.1–§7.3、§9 |
//! | [`admit`] | 准入顺序、有界读取、字节账本 | §4.4 |
//! | [`cli`] | `serve` 的选项面与默认值（无行为） | §3 |
//!
//! **不在**本模块内：HTTP 服务器与 `tiny_http`、路由表、`GET /`、前端、下载器的
//! 库侧实现、`ModelSet`/`ModelManifest`（后者由并行的 M0a 在库里落地）。
//!
//! # 与并行 M0a 的接缝
//!
//! M0c 不解析模型清单，只通过 [`state::ModelReadiness`] 接收"模型是否齐备、缺哪些文件"
//! 这一事实；M1 的填充点是库侧的共享逐文件校验函数（`ModelSet` / `ModelSetStatus`）。
//! `ServeError::Download` 的 [`error::DownloadError`] 同理：M0b 的加固下载器落地后
//! 必须与 `model_store` 的错误表示统一，避免两套下载错误类型长期并存。
//!
//! # 关于下面这条 `allow`
//!
//! M0c 只交付纯逻辑与单元测试：HTTP 层（M1）尚未接线，因此本模块的多数项在
//! **非 test 构建**里暂时没有调用点。这条 `allow` 只作用于 `serve` 子树，
//! 且**必须在 M1 接线时删除**——M1 的验收要求 `cargo clippy` 在默认 lint 集下无警告。
#![allow(dead_code)]

mod admit;
mod cli;
mod error;
mod jobs;
mod limits;
mod queue;
mod security;
mod state;

#[cfg(test)]
mod scope_tests {
    use std::path::{Path, PathBuf};

    /// 本模块的**范围边界**也是被测对象：M0c 不允许引入 HTTP 服务器或 socket。
    ///
    /// 两条断言：
    ///
    /// 1. `serve/` 目录源码的**代码部分**（去掉行注释后）不得出现 HTTP 库的路径引用
    ///    或 socket 类型——它们只可能出现在真正的 HTTP 层里，M1 才引入
    ///    （§2.1：`tiny_http` 只能 optional 且不进 default）；
    /// 2. `Cargo.toml` 的 `[dependencies]` 段不得声明 `tiny_http`。
    ///
    /// 注释里提到库名不构成依赖（本模块的文档正需要说明"为什么 M1 才引入它"），
    /// 因此扫描前去掉行注释；被禁的名字按字面量拼出来，避免本文件命中自己。
    #[test]
    fn the_scaffold_does_not_contain_an_http_server_yet() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let directory = root.join("src/bin/serve");
        let mut sources = Vec::new();
        collect_sources(&directory, &mut sources);
        assert!(
            sources.len() >= 8,
            "expected the M0c modules, found {sources:?}"
        );

        let forbidden = [
            ["tiny", "_http::"].concat(),
            ["Tcp", "Listener"].concat(),
            ["Tcp", "Stream"].concat(),
        ];
        for path in &sources {
            let text = std::fs::read_to_string(path).expect("a serve source file must be readable");
            let code = strip_line_comments(&text);
            for needle in &forbidden {
                assert!(
                    !code.contains(needle.as_str()),
                    "M0c must not pull in the HTTP layer yet: {} contains {needle}",
                    path.display()
                );
            }
        }

        let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
            .expect("the crate manifest must be readable");
        let mut in_dependencies = false;
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_dependencies = trimmed == "[dependencies]";
                continue;
            }
            if in_dependencies && !trimmed.starts_with('#') {
                assert!(
                    !trimmed.starts_with(["tiny", "_http"].concat().as_str()),
                    "the default dependency graph must not gain an HTTP server: {trimmed}"
                );
            }
        }
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
        for entry in std::fs::read_dir(directory).expect("the serve directory must exist") {
            let path = entry.expect("readable entry").path();
            if path.is_dir() {
                collect_sources(&path, out);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                out.push(path);
            }
        }
    }
}
