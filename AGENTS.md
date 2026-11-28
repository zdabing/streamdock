# AGENTS.md

本仓库为 M3U8/HLS 与媒体直链下载工具，采用 **Rust 后端与 CLI（Cargo workspace） + Vue 3 前端（单二进制内嵌静态资源）**。保留 CLI、WebUI、服务端、平台分享链接解析、网页媒体地址提取、RSS 订阅中心与 Emby NFO/海报刮削；不要重新引入播放器、搜索站、内嵌网页浏览器、AriaNg、Electron 或外部 CDN。

## 命令

前端使用 pnpm + Vite，后端与 CLI 使用 Cargo（Rust edition 2024）：
- `pnpm -F streamdock-frontend install` / `pnpm install`：安装前端依赖。
- `pnpm build:frontend`：构建 WebUI 到 `client/`（`crates/server` 通过 `rust-embed` 编译期嵌入 `client/`，编译 Rust 服务/CLI 前需确保 `client/index.html` 存在）。
- `pnpm build`：先执行 `pnpm build:frontend`，再执行 `cargo build --release -p streamdock-cli` 生成单二进制 `streamdock`。
- `pnpm start` / `cargo run --release -p streamdock-cli -- server`：启动 6600 端口服务（支持 `--web-dir` 或 `DS_WEB_DIR` 覆盖内嵌 WebUI）。
- `pnpm dev`：并发启动 `cargo run -p streamdock-cli -- server` 与 Vite 前端开发服务。
- `cargo test --workspace`：运行 core 与 server 全套 Rust 集成测试（本地 Axum 测试服务 + 真实 FFmpeg + SQLite 持久化/迁移验证；FFmpeg 缺失时跳过媒体用例）。
- `node --test test/scriptcat.test.cjs`：验证脚本猫用户脚本。
- `pnpm lint` / `pnpm lint:all`：前端 Biome lint + `cargo fmt --all -- --check`。前端启用 Biome 完整 Vue 支持，不能把仅在模板使用的变量视为未使用。
- `pnpm typecheck`：前端 `vue-tsc` + `cargo check --workspace --all-targets`。

## 架构

- `crates/core`（`streamdock-core`）：
  - `contract.rs`：纯数据前后端契约（与 `packages/frontend/src/types/contract.ts` 一一对应，不引入服务端实现细节）。
  - `http.rs` / `error.rs`：请求头归一化、自动 Referer/User-Agent、代理配置（`custom`/`system`/`disabled` + `no_proxy`）、HTTP 重试/重定向与取消、Cloudflare/短效签名提示。手动请求头优先；空 Referer 禁用自动值。HTTP 状态错误不能吞掉。
  - `m3u8_parse.rs` / `crypto.rs` / `container.rs`：M3U8、EXT-X-MAP、字节范围与 AES-128 解密；落盘前解密，缺失显式 IV 时按媒体序号推导。
  - `m3u8_download.rs` / `file_download.rs` / `download.rs`：统一分流 HLS 与直链下载，有限并发异步下载（不使用 OS 多线程阻塞池），普通文件流式落盘 + Range/ETag/Last-Modified 校验，不匹配时重下，仅完整文件才发布（不经 FFmpeg，不篡改字幕后缀）。
  - `merge.rs`：流式 stdin 接入 FFmpeg、单合并队列、进度与无进展超时及异常/取消清理；合并锁按 Drop/作用域释放。
  - `extractor.rs` / `video_parser/` / `signed_hls_proxy.rs` / `cat_catch.rs`：网页有限深度的 BFS 提取、抖音/微博/皮皮虾/内置签名站点解析、本地短效 HLS 签名代理续签与猫抓接入；不执行远程脚本，跨源请求不转发 Cookie/Authorization。
  - `rss_filter.rs` / `rss_parser.rs` / `rss_service.rs`：RSS/Atom/HTML 列表抓取（32 MiB/50,000 条上限）、剧集键提取与过滤规则、后台定时调度与自动入队。
  - `scraper/`：多数据源（RSS 全量缓存 + WordPress REST API）并发刮削、详情页元数据解析、封面校验/WebP 转码/FFmpeg 2:3 裁切与 Emby NFO 生成。
  - `store.rs`：SQLite WAL 持久化（`<cacheDir>/streamdock.db`），首次启动在单事务内自动迁移旧版 `config.json`、`cache.json`、`scrape-config.json`、`scrape-job.json`、`rss-sources.json` 与 `rss-items/*.json`。
- `crates/server`（`streamdock-server`）：
  - 基于 Axum 0.8 + WebSocket，内嵌 `client/`（`rust-embed`）与 `examples/scriptcat-downloader.user.js`。
  - 统一认证中间件（`DS_SECRET`/`DS_TOKEN`，原始 `Authorization` 或 8 位 MD5 短密码；仅 `/api/file` 接受 `?token=`），队列并发控制、防旧回调覆盖、`Range` 206 下载与后台刮削任务管理。
- `crates/cli`（`streamdock-cli`）：
  - `clap` v4 命令行入口，支持默认下载、`extract`、`server` 与 `scrape` 子命令。
- `packages/frontend/src`：`App.vue`、`AppIcon.vue`、`types/contract.ts` 和普通 CSS；不使用 Pinia、Router、Tailwind 或字体 CDN。修改功能后更新中文文档。

## CodeGraph

仅当根目录存在 `.codegraph/` 时，在代码搜索前优先使用 `codegraph explore`；没有该目录则跳过。
