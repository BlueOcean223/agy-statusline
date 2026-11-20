# AGY Statusline (`agy-statusline`)

Google Antigravity 2.x 桌面端的状态栏插件：在聊天输入框上方显示两行状态栏，右侧辅助面板（`AUX_PANE`）放详细数据和设置。外观参考 [agy-zh-pack](https://github.com/ncdeng/agy-zh-pack)，可选的中文界面直接用它的词典和翻译脚本（MIT，见 `i18n/LICENSE`）。

```
✦ Gemini 4 Neon (XHigh) | ⑂ main* · ↑1 · +525 −353 | ◔ 43% · 222k / 512k | ⛁ 212k · 97% hit | $ $21.8 · $43.2 today | ● running
⧗ 4.0s | ϟ 857 tok/s | 5h ▮▮▮▯▯▯▯▯▯▯ 33% · 19:18 | 7d ▮▮▯▯▯▯▯▯▯▯ 18% · Wed 09:31 · ↓1%
  ▣ research · API Researcher | ✦ Gemini 4 Neon (XHigh) | ◔ … | ⧗ 4.9s | ϟ 427 tok/s | ● running
```

## 安装

从 [Releases](https://github.com/BlueOcean223/agy-statusline/releases) 下载对应平台的包，解压出的 `agy-statusline/` 文件夹放进 Antigravity 的插件目录 `~/.gemini/config/plugins/`，Antigravity 会自动加载。

- macOS：`agy-statusline-v<版本>-macos-universal.tar.gz`（Apple 芯片和 Intel 通用）。二进制没有签名，用浏览器下载的话先去掉隔离标记，否则系统会拦下它：`xattr -dr com.apple.quarantine ~/.gemini/config/plugins/agy-statusline`
- Windows：`agy-statusline-v<版本>-windows-x64.zip`，放到 `%USERPROFILE%\.gemini\config\plugins\`。Windows 包还没有在真实的 Antigravity 上验证过。

`SHA256SUMS` 里是各个包的校验和。

## 结构

方案对比和字段来源见 [docs/architecture.html](docs/architecture.html)。简单说：

| 部分 | 做什么 |
|---|---|
| `agy-statusline sidecar` | 由 `sidecars/statusline/sidecar.json` 拉起、Antigravity 负责重启。里面三件事：live 引擎调 language_server 只读 API 取数；CDP 画师把渲染脚本注入主窗口并推数据；HTTP 服务提供右侧面板 |
| `agy-statusline hook` | `hooks.json` 里的各事件调用。只发一个 UDP 包告诉 sidecar“哪个对话发生了什么”，不等回应（≤10ms） |
| `web/bar.js` | 注入页面的纯渲染脚本：找输入框、回报对话 ID、画状态栏。没有任何取数逻辑 |
| `web/index.html` 等 | 右侧面板（Overview / Turns / Settings），数据来自 `/api/panel`，顶部状态条也由 `bar.js` 绘制 |

数据来源（都是本机 language_server 的只读接口，地址和 token 由 Antigravity 通过 `ANTIGRAVITY_LS_ADDRESS` / `ANTIGRAVITY_CSRF_TOKEN` 交给 sidecar）：

- `GetAllCascadeTrajectories`：对话状态、步数、父子关系、工作区和分支
- `GetCascadeTrajectoryGeneratorMetadata`：每次模型调用的 token、首字时间、生成时长、实际模型、上下文估算（按 offset 增量拉）
- `RetrieveUserQuotaSummary`：5 小时和每周额度

Git 段在 sidecar 里对对话所在工作区（`workspaces[0].workspaceFolderAbsoluteUri`）跑 `git status --porcelain=v2 --branch` 和 `git diff HEAD --numstat`，每个工作区最多 5 秒一次，带 `GIT_OPTIONAL_LOCKS=0`，不抢你自己 git 命令要的锁。

另外读两类本地文件：`brain/<id>/.system_generated/logs/transcript.jsonl`（按字节偏移增量读，给每次调用补开始时间和“这一轮做了什么”的标签）和 `annotations/<id>.pbtxt`（对话标题）。

不修改 Antigravity 发出的任何请求。

## 启用输入框上方的状态栏

Antigravity 2.33.0 起，只有带调试端口参数启动才能注入。最简单的是在右侧面板 Settings 里点“以调试模式重启 Antigravity”（点两次确认，会中断正在运行的对话）；它会退出 Antigravity，等进程结束后用下面的命令重新打开。也可以自己退出后运行：

```bash
open -a Antigravity --args --remote-debugging-port=0
```

`0` 表示随机端口，端口号写在 `~/Library/Application Support/Antigravity/DevToolsActivePort`，sidecar 每 3 秒检查一次，连上后自动注入，不需要刷新窗口。不带参数启动时，右侧面板照常工作，Settings 里会显示 “no debugging port”。

注意：开了调试端口后，本机任何进程都可以通过它控制 Antigravity 窗口。

## 设置

右侧面板的 Settings 页（写入 `~/.gemini/antigravity/plugin_data/agy-statusline/config.toml`）：

- 面板顶部的状态条就是输入框状态栏的预览，跟着下面的设置立刻变化；设置改动 0.4 秒后自动保存
- 输入框状态栏：总开关，以及模型、Git、上下文、缓存命中、对话花费、今日花费、首字延迟、输出速度、5 小时额度、7 天额度、运行状态、子代理行各自的开关
- 中文界面：把 Antigravity 的菜单、按钮等界面文字换成中文，不动对话内容和代码。打开时立刻生效，窗口刷新后仍然生效；关闭后已经翻译的文字要刷新窗口（或重启）才恢复英文。同样需要调试端口
- 调试端口状态和“以调试模式重启 Antigravity”按钮（目前只支持 macOS）
- 上下文上限覆盖、缓存 TTL、面板合计是否含子代理
- 模型单价在 config.toml 的 `[[models]]` 里改，按顺序匹配 `responseModel + 显示名`

费用 = token × 单价，是公开 API 标准价的折算，不是订阅实际扣费。

## 开发

```bash
cargo build --release && cp target/release/agy-statusline bin/.new && mv -f bin/.new bin/agy-statusline
```

替换后结束旧的 sidecar 进程（`pkill -f 'agy-statusline sidecar'`），Antigravity 会用新二进制重新拉起。

- `agy-statusline live [对话ID…]`：打印推给页面的 JSON，不传 ID 时取最近修改的主对话。可在 sidecar 外单独运行（通过 `ps`/`lsof` 找 language_server）。
- `AGY_DEVTOOLS_PORT_FILE=<path> agy-statusline sidecar --port 47811`：让 CDP 连到别的浏览器，用来在无头 Chrome 上测渲染。
- `agy-statusline status [-c 对话ID]`：终端里的两行预览，默认取最近看过的对话。

### CI 和发版

- `.github/workflows/ci.yml`：推到 main 或开 PR 时，检查格式（`cargo fmt --check`）、JS 语法和 JSON 清单，并在 macOS、Windows、Linux 上跑 `cargo clippy -D warnings` 和 `cargo test`。
- `.github/workflows/release.yml`：推 `v*` 标签时构建 macOS 通用二进制和 Windows 二进制，打成插件包发到 GitHub Release。标签必须和 `Cargo.toml`、`plugin.json` 里的版本一致，否则直接失败。

发一个新版本：

```bash
# 先把 Cargo.toml 和 plugin.json 的 version 改成 0.3.0 并提交
git tag v0.3.0 && git push origin v0.3.0
```

## 已知限制

- 压缩次数只在一个对话上验证过（上下文估算从 481k 降到 22k）；那份数据里没有 `checkpointIndex`。
- 重启按钮的“退出后重新打开”没有在真实窗口里完整走过一遍；确认流程和脚本语法已检查。
- 子代理标签页（`subagent-tab-conversation`）的选择器沿用 agy-zh-pack，macOS 上还没在真实窗口里验证；主对话和首页已验证。

## 许可

MIT，见 [LICENSE](LICENSE)。`i18n/` 下的词典和翻译脚本来自 agy-zh-pack，按它自己的 MIT 许可（`i18n/LICENSE`）。`web/preload.js` 是 Antigravity 自带的 sidecar 前端桥接脚本，不在本项目许可范围内。
