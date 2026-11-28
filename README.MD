# StreamDock

支持电脑与手机访问的自托管流媒体批量下载工作台，集成了 HLS/直链下载、全量 RSS 订阅中心与 Emby NFO/海报刮削整理。

服务端与 CLI 基于 **Rust**（单一原生可执行文件 + **SQLite WAL** 持久化 + 内嵌 Vue WebUI），Docker 镜像仅包含 Alpine、FFmpeg、tini 与 `streamdock` 二进制文件，无 Node.js 运行时或浏览器内核开销。

- M3U8 / HLS 下载、AES-128 解密和 FFmpeg 无损合并。
- MP4 等视频、MP3 / M4A / AAC / WAV / FLAC / OGG / Opus 等音频、VTT / SRT 等字幕直链下载。
- 网页媒体提取和部分平台分享链接解析（抖音、微博、皮皮虾及内置签名站点）。
- 批量下载、暂停续传、失败重试和任务管理（任务/配置/订阅/刮削记录持久化到 `<cacheDir>/streamdock.db`）。
- 从旧版 Node/JSON 缓存平滑升级：首次启动检测到 `<cacheDir>` 下的 `config.json`、`cache.json`、`scrape-config.json`、`scrape-job.json`、`rss-sources.json` 与 `rss-items/*.json` 时，自动在事务内导入 SQLite。
- 脚本猫（`v1.2+`）发送媒体链接、清晰度信息、页面片名和详情页地址。
- 提供内置 WebUI（`rust-embed` 单二进制内嵌，也支持 `--web-dir` / `DS_WEB_DIR` 运行时覆盖）与 CLI。

## Docker 运行

在仓库根目录创建 `.env`，设置访问密码：

```dotenv
DS_SECRET=你的访问密码
```

```sh
docker compose -f docker/docker-compose.yml up --build -d
```

打开 `http://服务器IP:6600`。下载文件保存在 `docker/downloads`，SQLite 数据库（`streamdock.db`）和分片缓存保存在 `docker/cache`。已完成文件可通过页面或 `GET /api/file`（支持断点续传 `Range`）保存到电脑或手机。

## 内置 RSS 订阅中心（全量不限条数 + 自动下载与 NFO 刮削）

服务端内置无 500 条数量限制的 RSS / Atom 与公开 HTML 列表订阅中心，并与下载任务和 Emby 刮削引擎原生打通：

- **全量索引与服务端分页**：支持单源拉取最多 **50,000 条**（最高 32 MiB XML）条目并存入 SQLite 索引表，提供关键词实时搜索、状态筛选与正/倒序分页浏览。
- **收集模式与自动下载**：新建订阅源默认启用 **收集模式**（全部历史入库为“新条目”供手工筛选批量下载）；开启 **自动下载** 后，首轮历史自动标为“历史跳过”，仅对后续新增且命中包含/排除/正则/体积/智能剧集去重规则的条目按每轮上限自动入队，并支持失败指数退避定时轮询。
- **直链与详情页提取**：既支持 `.m3u8` / `.mp4` 等标准 `<enclosure>` 直链，也支持自动解析视频详情页抽取 `.m3u8` 并携带来源 `Referer` 下载。
- **下载完成精确生成 NFO**：订阅源入队时自动绑定精确详情页链接，视频下载合并完成后直接调用刮削引擎写入 Emby/Jellyfin 标准 `.nfo` 与 FFmpeg 2:3 竖版 `poster`（支持原地刮削或按 `演员/日期 片名/` 目录归档）；若刮削失败不影响视频已完成状态，可在任务列表一键「重刮 NFO」。

## 媒体刮削（Emby NFO 与海报裁切）

内置基于多数据源优先级串联（支持 `RSS 全量索引` 与 `WordPress REST API 检索`）的本地视频刮削与整理能力：

- **可排序的多数据源瀑布流匹配**：支持在页面自由添加/删减、启用/停用、上下调整 **元数据数据源** 搜索优先级，并可从已有 RSS 订阅源一键导入：
  - 默认顺序 **#1 RSS 全量索引 → #2 WordPress 兜底检索**：RSS 仅拉取一次即可在内存中毫秒级匹配上万视频；高置信命中后自动跳过后续 WordPress 站点检索，仅将未匹配或低置信视频分词（去编号、下划线分段、提取番号）兜底检索 WordPress（`/wp-json/wp/v2/posts`），兼顾极速与高命中率。
  - **健壮封面校验与 WebP 自动转码**：下载封面时自动校验 `image/*` 响应头与图片文件头（跳过失效图床返回的 `text/html` 并自动切换页面正文图等候选封面），遇到 `.webp` 封面在 FFmpeg 可用时自动转为标准 `.jpg` 后裁切 2:3 竖版 `poster`。
- **WebUI「媒体刮削」与服务端后台执行**：支持同时添加并去重扫描**多个本地目录**（最高共 20,000 个视频）并按文件名模糊匹配预览前 3–5 个候选结果；**扫描目录、数据源列表与所有刮削策略自动保存至 SQLite**，刷新或换设备打开均会自动恢复。点击「开始刮削所选」后提交给服务端后台队列独立运行（可调 `1–10` 并发），**关闭或刷新浏览器都不会中断**；任务状态持久化保存，点击「**暂停刮削**」（甚至重启容器）后无需重新扫描即可直接点「**继续剩余**」或「**重试失败**」，并支持实时多端 WebSocket 进度、平均速率与阶段耗时诊断同步。
- **Docker 避免跨卷拷贝提示**：按目录归档时，若源目录与输出目录分别映射了两条不同的容器挂载卷（即使在 NAS 同一块物理硬盘上），Linux `rename()` 仍会触发 `EXDEV` 并退化为整文件拷贝。建议在 Docker 中将共同父目录合并为单条挂载（例如宿主机 `/volume4/w1:/media`，扫描 `/media/tg`，输出 `/media/emby`），或直接使用「原地刮削」，移动/硬链接即可毫秒完成。
- **诊断与日志**：界面内置瓶颈识别与一键「复制诊断日志」；服务端与 Docker 日志会输出 `[刮削扫描]`、`[刮削开始]`、`[刮削 i/N]`、`[刮削已停止]` 与 `[刮削汇总]`。
- **两种整理模式**：默认 **原地刮削**（不动原文件，旁路生成 `<文件名>.nfo` 与封面）；也可切换 **按目录归档**（`移动 / 硬链接 / 复制` 到 `演员/日期 片名/` 独立目录并附带同名外挂字幕）。
- **命令行**：

```sh
streamdock scrape ./downloads ./extra --rss https://example.com/rss.xml --wp https://wp.example.com --dry-run
streamdock scrape ./downloads --mode organize --output ./emby --transfer hardlink
```

## 本地构建与运行

需要 Rust 工具链（`1.85+` / edition 2024）、Node.js 22+（仅构建前端需要）、pnpm 与 FFmpeg。

```sh
pnpm install
pnpm build
pnpm start
```

构建产物为单个原生二进制 `target/release/streamdock`（Windows 为 `target\release\streamdock.exe`），已内嵌 `client/` 与脚本猫脚本；也可单独使用外部前端目录覆盖：`streamdock server --web-dir ./client`。

访问 `http://localhost:6600`。默认下载目录为 `./downloads`。若启用了 `DS_SECRET`，登录弹窗默认勾选“在此浏览器记住登录状态”，同一浏览器下次打开免重复输入，也可在「系统设置 → 外观与主题」退出登录。

## 使用

WebUI 同时适配电脑端与手机端（移动端提供底部导航栏与抽屉弹窗）。点击「新建下载」，粘贴媒体链接即可，多条链接每行一个。播放页使用「网页提取」；需要登录或校验来源的资源，可填写 Cookie、Referer 等请求头。

使用脚本猫时，先在页面点击「脚本猫脚本 / 安装脚本猫脚本」。默认在未检测到媒体的普通网页及 WebUI 本页均会自动隐藏右下角悬浮按钮（不遮挡浏览），一旦页面嗅探到视频或字幕资源即自动弹出「下载 · N」（可上下拖动；也可从脚本猫扩展菜单「打开媒体发送面板」随时打开，或在「连接设置」中关闭自动隐藏）。配置下载器地址和 `DS_SECRET`，播放视频、开启字幕即可发送到队列。

脚本会优先从页面结构化数据（JSON-LD）、OpenGraph 或标题元素提取真实片名（支持 SPA 切集自动更新，也可点「重抓 / 清空」调整），自动解析 M3U8 主列表分支、分辨率（如 `1080p` / `720p`）、码率、时长与加密标记，并提供「探测清晰度」及「全部 / 视频 / 字幕」筛选；每条资源支持手机端单条一键「发送」，或勾选后「发送所选」，发送成功与失败均有顶部浮层及底部状态栏提示。

支持识别文件名在下载参数中的无后缀 MP4；「重新扫描」会检查已请求的资源，也可展开「手动添加媒体地址」粘贴直链。

脚本猫按媒体地址读取获授权的 Cookie（Master 跨源分支不会转发原域凭证或 Cookie），不能完整替代扩展级网络抓包。目前不支持脚本猫持续续签，短时效链接可能过期。平台解析和网页提取能否成功取决于来源站点；字幕单独保存。

## 命令行

```sh
streamdock "https://example.com/video.m3u8" -f video
streamdock "https://example.com/video.mp4"
streamdock "https://example.com/zh.vtt"
streamdock extract "https://example.com/watch" -d 1
streamdock server -P 6600 --cache-dir ./cache
streamdock --help
```

开发：`pnpm dev`。检查与测试：`cargo test --workspace`、`pnpm test:scriptcat`、`pnpm lint`、`pnpm typecheck`。

MIT 许可证，见 [LICENSE](LICENSE)。
