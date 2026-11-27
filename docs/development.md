# 开发与架构说明

[简体中文](development.md) | [English](development.en.md) | [返回 README](../README.md)

更详细的方案对比与字段来源图解见 [architecture.html](architecture.html)。

---

## 模块结构

| 组件 | 路径 | 职责 |
|---|---|---|
| **Sidecar 入口** | `src/main.rs`、`src/server.rs`、`src/live/panel.rs` | 由 `sidecars/statusline/sidecar.json` 拉起并托管：运行 live 引擎、CDP 注入器、本地 HTTP 面板服务（写接口与 `GET /api/checkpoint` 均受 UI 令牌保护）、调试重启守卫与更新中心 |
| **实时引擎** | `src/live.rs`、`src/ls.rs`、`src/git.rs`、`src/transcript.rs` | 增量拉取 `language_server` 只读接口、读取 transcript/annotations 与按需提取 `transcript_full.jsonl` 压缩摘要、定期采集工作区 Git 状态 |
| **用量、额度与归因** | `src/usage.rs`、`src/quota.rs`、`src/cli_usage.rs`、`src/miss.rs` | 持久化 400 天每日用量（`usage-history.json`）、8 天每周额度采样（`quota-samples.json`）与 CLI 日期索引（`cli-index.json`）；只读 SQLite 补算桌面与 `agy CLI` 对话库、单日调用/按对话明细、6 类缓存未命中归因分析，以及跨周期分段与提早重置（`↺`）的每周额度状态机 |
| **CDP 渲染注入** | `src/cdp.rs`、`web/bar.js` | 每 3 秒扫描 `DevToolsActivePort`，向主窗口与 Swarm iframe 注入纯渲染脚本 `bar.js` 与 `i18n/translator.js`（v12）；同步全局临时英文快捷键、关闭中文时即时还原英文，并自动重载卡死在 `"plugin unavailable"` 的侧边栏面板 |
| **跨平台重启守卫** | `src/relaunch.rs` | macOS（`nohup` 脱离）与 Windows（两段式 PowerShell 脱离 Job Object）调试重启，触发前核验主进程命令行防误重启 |
| **更新与数据同步** | `src/updater.rs` | 自动识别环境变量与系统代理；支持 GitHub Releases `SHA256SUMS` 校验自更新（含备份回滚与开发目录保护），以及按 commit 同步 `agy-zh-pack` 词典与价格表 |
| **生命周期 Hook** | `src/hook.rs`、`hooks.json` | 响应会话/工具事件，向 sidecar 发送本地 UDP 通知（≤10ms）触发即时刷新 |
| **侧边栏前端** | `web/index.html`、`web/app.js`、`web/styles.css` | 编译期嵌入二进制的 5 标签页前端（Overview / Turns / Usage / Theme / Settings） |

---

## 本地只读数据源

`language_server` 地址与令牌由 Antigravity 通过 `ANTIGRAVITY_LS_ADDRESS` 和 `ANTIGRAVITY_CSRF_TOKEN` 传入 sidecar（CLI 模式下通过 `ps`/`lsof` 自动发现）：

- **`GetAllCascadeTrajectories`**：对话运行状态、步数、父子关系与工作区路径。
- **`GetCascadeTrajectoryGeneratorMetadata`**：按 `generatorMetadataOffset` 增量拉取每次模型调用的 token（含 Claude 缓存写入与思考/回复输出）、首字时间（TTFT）、流式时长、实际模型名与上下文窗口估算。
- **`GetCascadeTrajectorySteps`**：仅对当前屏幕活跃或刚结束的子代理按 `stepOffset` 增量拉取步骤，获取审批等待（`WAITING`）、耗时与近期步骤时间线。
- **`RetrieveUserQuotaSummary`**：5 小时与每周额度桶及重置时间。
- **`GetUserStatus`**：当前登录账号（页面与落盘仅保留打码邮箱和 FNV-1a 哈希键）及订阅档次。

**本地文件与 Git**：
- `brain/<id>/.system_generated/logs/transcript.jsonl` 与 `transcript_full.jsonl`：前者按字节偏移增量读取步骤创建时间、工具摘要与历史子代理提示；后者仅在用户点击压缩摘要时按需读取（经 UI 令牌鉴权，不持久化）。
- `annotations/<id>.pbtxt`：读取对话标题与最近查看时间。
- `conversations/*.db` 与 `~/.gemini/antigravity-cli/conversations/*.db`（及 `conversation_summaries.db`）：后台只读打开（`SQLITE_OPEN_READ_ONLY`），轻量解析 protobuf 行补算历史天数与 `agy CLI` 用量，并在查看历史单日明细时按需查询。
- Git 状态：对当前屏幕对话的工作区执行 `git status --porcelain=v2 --branch` 与 `git diff HEAD --numstat`，带 `GIT_OPTIONAL_LOCKS=0`，每个工作区最多 5 秒执行一次。

**数据目录**（`~/.gemini/antigravity/plugin_data/agy-statusline/`，Windows 为 `%USERPROFILE%\.gemini\antigravity\plugin_data\agy-statusline\`；所有落盘文件使用原子写入，解析失败时自动备份为 `*.bad-YYYYMMDDHHMM.*`）：
- `config.toml`：全部插件配置、外观、高价告警阈值与模型单价规则
- `usage-history.json` / `quota-samples.json` / `cli-index.json`：400 天每日用量汇总（含周期历史与重置事件）、8 天每周额度采样与 CLI 会话库日期索引
- `relaunch-state.json` / `relaunch.log`：防误重启状态与重启脚本日志
- `release-cache.json` / `update-state.json` / `backup/`：版本检查缓存、更新状态与回滚备份
- `remote/`：在线同步的 `zh-CN.json`、`zh-CN.rules.json`、`prices.json` 与同步状态

---

## 本地开发与调试

### 编译并热更新运行中的 Sidecar

将本仓库软链接到插件目录（例如 `ln -s "$(pwd)" ~/.gemini/config/plugins/agy-statusline`）后：

```bash
cargo build --release && cp target/release/agy-statusline bin/.new && mv -f bin/.new bin/agy-statusline
pkill -f 'agy-statusline sidecar'
```

Antigravity 会在 1～2 秒内自动用新二进制重新拉起 sidecar。

### CLI 调试命令

```bash
# 在终端打印最近查看对话的两行预览
cargo run -- status [-c <conversation-id>]

# 打印推给主窗口状态栏的完整 JSON（可脱离 sidecar 独立运行）
cargo run -- live [<conversation-id>...]

# 指定自定义 CDP 端口文件，配合无头 Chrome 测试页面渲染
AGY_DEVTOOLS_PORT_FILE=/path/to/DevToolsActivePort cargo run -- sidecar --port 47811
```

### 本地自检（与 CI 一致）

```bash
cargo fmt --check
for f in web/*.js i18n/translator.js; do node --check "$f"; done
for f in plugin.json hooks.json sidecars/*/sidecar.json i18n/*.json; do python3 -m json.tool "$f" > /dev/null; done
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

---

## CI 与发版流程

- **`.github/workflows/ci.yml`**：推送到 `main` 或提 PR 时执行格式检查、JS 语法检查、JSON 清单检查，并在 macOS、Windows、Linux 三平台运行 `cargo clippy --locked --all-targets -- -D warnings` 与 `cargo test --locked`。
- **`.github/workflows/release.yml`**：推送 `v*` 标签后自动校验版本号一致（标签、`Cargo.toml`、`plugin.json` 三处必须相同，且 `CHANGELOG.md` 包含对应版本变更），构建 macOS 通用二进制（`aarch64` + `x86_64`）与 Windows x64 二进制，打包生成 `SHA256SUMS`，并自动提取 `CHANGELOG.md` 对应章节作为 GitHub Release 正文。

发布新版本：

```bash
# 1. 在 CHANGELOG.md 中补充目标版本（如 v0.4.0）的变更记录
# 2. 将 Cargo.toml、Cargo.lock 与 plugin.json 的 version 改为目标版本并提交
# 3. 打标签并推送，触发 Release 工作流
git tag v0.4.0 && git push origin v0.4.0
```
