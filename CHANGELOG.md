# 更新日志 (Changelog)

本项目所有重要变更均记录于此。版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)；每次推送 `v*` 标签发版时，GitHub Actions 会自动提取本文件中对应版本的变更内容作为 GitHub Release 说明。

## [Unreleased]

## [v0.5.2] - 2026-11-27

### 改进与修复 (Fixed & Improved)
- **macOS 与 Windows 重启询问弹窗升级为双端统一 Rust 自绘 Material 3 对话框**：替换旧版 macOS `osascript` 与 Windows `TaskDialogIndirect` 系统弹窗，零外部 GUI 框架——共享同一套 Google Material 3 布局、明暗色板与交互状态机，底层分别由 macOS 原生 Cocoa/CoreGraphics（sRGB + Retina）与 Windows 原生 `WS_EX_LAYERED` + GDI+ `32bppPARGB`（28px 抗锯齿圆角 + Per-Monitor DPI v2，保留 `MessageBoxW` 回退）绘制；精简文案并新增左下「不再询问」复选框（勾选后自动将 `relaunch_mode` 设为 `never`），支持拖拽与 `Enter`/`Esc`/`Space` 快捷键；弹窗出现后 0.7 秒内以及按住不放的 `Enter`/`Space` 会被忽略，避免在 Antigravity 里打字时误触重启或误勾「不再询问」，Windows 上用 Alt+F4 关闭等同「稍后」并保留复选框状态。对话框在独立的 `relaunch-prompt` 子进程中运行，AppKit/CoreGraphics 与 GDI+ 只在该进程里加载，常驻 sidecar 与每次 hook 调用都不加载。Windows 对话框按光标所在显示器的 DPI 确定尺寸，有任务栏与 Alt+Tab 入口（拿不到前台时闪烁任务栏按钮），绘制失败时改用 `MessageBoxW`；读屏软件可读到对话框的文字与按键说明。
- **重启询问弹窗未能给出回答时不再算作「用户拒绝」**：对话框进程启动失败、崩溃或被结束时，`relaunch.log` 记为提示失败，本次会话仍算未询问，5 分钟后重新弹出（每个会话最多 3 次）；`relaunch-state.json` 无法读写时，同一会话也不会在点「稍后」后反复弹出。
- **非调试模式启动时重启询问弹窗更快出现**：调试端口不可达的判定等待从 20 秒缩短为 3 秒，监视线程在 sidecar 启动后的前 60 秒内每 1 秒检查一次（此前固定 5 秒），弹窗由约 20–25 秒提前到约 3–4 秒；是否弹窗仍以 Antigravity 主进程命令行缺少 `--remote-debugging-port` 为准，不会因此误触发重启。
- **修复 CLI 调试命令（`status` / `live`）在 Antigravity 2.35+ 上找不到 `language_server`**：新版 `language_server`（`--standalone --subclient_type hub`）改从 stdin 读取令牌，命令行不再带 `--csrf_token`，导致不经 sidecar 环境变量的发现逻辑总是失败。现在依次尝试：旧版 `--csrf_token` 参数；新版写入的 `~/.gemini/antigravity/daemon/hub_<port>.json` 发现文件（仅当其中 pid 为正在运行的 `language_server` 时采用）；最后读取正在运行的 agy-statusline sidecar 进程环境中的 `ANTIGRAVITY_LS_ADDRESS` / `ANTIGRAVITY_CSRF_TOKEN`。只连接回环地址，探测仅调用 `GetAllCascadeTrajectories`，不会让 `language_server` 加载新对话；sidecar 内的连接方式不变。
- **按响应 ID 识别同一次调用，修复单日明细中实时调用与对话库副本重复计入**：`language_server` 返回的调用记录里没有 `requestId`，此前读取它的去重键始终为空，实时调用只能靠「对话 + 秒 + 输入/输出 token」匹配库中副本，尚未写入 transcript 的调用时间不准时会被算两次。现在实时记录取 `usage.responseId`、对话库取 usage 字段 11（本机 131/131 次调用核对一致），两者相同即为同一次调用；都带响应 ID 且不同时不再按秒级键误合并。桌面端、`agy CLI` 与实时路径共用同一套去重规则和同一个对话库解码器。
- **修复调价当天同一次调用被计两次，各处金额统一按同一套计价规则**：每日聚合此前只按模型 raw 名称分键并匹配规则，而单次调用按「raw + display」匹配，两者落到不同规则时（例如 display 为 `Gemini 4 Hydrogen (Orchestrator)`），调价当天的余量扣减对不上，同一次调用会算两次。现在每日聚合的键改存为 `raw|display`（见 ADR 0001），状态栏、侧边面板、用量页、每周额度周期与缓存未命中分析都通过同一个计价模块匹配规则和计价；调价当天统一按「仍在磁盘上的调用按各自时间计价，其余按当天中午计价」处理，此前每周额度周期在调价当天整日按中午计价。不含 `|` 的旧键视为 display 为空，计价与旧版一致，不做迁移；升级当天同一模型可能暂时分成新旧两行，总量不变——调价当天的余量按每个聚合键扣减（先扣同键的调用，旧键再扣同一 raw 名称的调用），旧键与新键匹配到不同规则时也只计一次，每个键的余量按自己的规则计价。截止时间正好落在某天结束（次日 0 点）时，那一天不再按调价日处理。
- **修复 Windows 开机后不久启动 sidecar 时崩溃**：Windows 上 `Instant` 从开机时刻起计且不能早于开机，此前主循环与 CDP 轮询用 `Instant::now() - 60s` / `- POLL_EVERY` 表示「立即到期」，开机后一分钟内启动 sidecar（例如登录后自动启动）会直接 panic。现在改为 `Option<Instant>`，`None` 表示尚未执行、首轮立即执行，行为不变。
- **修复午夜进入夏令时的时区把全部历史对话算作「今天」**：在 00:00 进入夏令时的地区（如 `America/Santiago`、`America/Asuncion`、`Asia/Beirut`、`Africa/Cairo`），切换当天本地 00:00 不存在，此前「今天起点」退回为 0，所有历史对话都会被当作今天加载并计入今日花费；按日期求当天起点的 `day_start_ms` 在这一天返回空值，部分每日统计会跳过或改用兜底值。现在两者统一取当天第一个存在的本地时刻（例如 01:00）；整天被跳过的日期（如萨摩亚 2011-12-30）取下一天的起点。
- **侧边面板打开未加载的对话时不再卡住状态栏**：此前面板请求在持有引擎锁期间同步读取对话本身、整棵子代理树和 transcript 里提到的历史子代理（language_server RPC 或对话数据库），子代理较多时状态栏刷新会一起等待。现在面板请求与引擎循环共用同一套「规划 → 锁外读取 → 合并」流程，只读取还没加载的对话，每次请求最多读 4 轮、每轮最多读 24 个数据库，其余由引擎循环补齐；面板加载不会淘汰对话，读取失败也不再显示在状态栏的错误里。
- **引擎持锁期间不再读取任何文件，也不再查找 language_server**：未连接时查找 `language_server`（启动 `ps`/`lsof`、探测端口）改为在锁外进行，此前找不到时每轮都在锁内查找、面板请求一直等待；对话 transcript（用于给调用标注时间和标签）的增量读取并入对话读取计划，与 language_server / 数据库读取一起在锁外进行，合并时若已有另一次读取先合并则丢弃、下一轮重读；历史子代理线索只依据每 30 秒一次的 `brain/` 目录扫描判断是否存在，不在锁内逐个检查目录；侧边面板标题只使用每 10 秒刷新一次的 annotation 缓存，新建对话的标题最多晚 10 秒出现；调价当天状态栏「今日」所需的对话库与 `agy CLI` 调用改为在锁外每 5 秒读取一次，状态栏与面板只用读好的副本。面板读取对话库期间若 language_server 刚好加载了该对话，同一请求内会改从 language_server 重读，不再显示一次空数据。
- **对话库中一行无法解析时不再丢掉整个对话的用量**：此前一行异常就会让整个对话库读取失败，首次补算历史时该对话的历史会永久缺失；现在该行按无用量记录处理，读取中途出错时保留已读的行。同一响应 ID 先被无 ID 副本匹配后，后续副本不会再被重复计入；实时调用与对话库副本合并时采用对话库记录的时间，尚未写入 transcript 的调用不会被算到错误的日期或价格档。

## [v0.5.1] - 2026-11-26

### 改进与修复 (Fixed & Improved)
- **子代理改为调用 Antigravity 原生侧边栏标签页打开**：移除原先点击「打开」时调用 `toggleConversation` 切换主窗口到旧版全屏子代理路由的行为（该行为会劫持当前主对话并迁移 sidecar 标签页，在切回侧边栏会话时还易误触宿主的隐形归档按钮）；现在对当前窗格已加载的直属子代理改用 `toggleAuxPane({ tabType: 'subagentView', ... })` 在右侧辅助栏打开原生子代理标签页，对跨会话预览、嵌套或历史仅落盘子代理则降级为面板内「详情」查看（附「← 返回主对话」按钮），并修复切换会话/子代理时顶部下拉框「当前窗格」标题错位问题。
- **Windows 重启询问弹窗升级为原生 `TaskDialogIndirect`**：替换原先通过 `powershell.exe` 调起的 `WScript.Shell.Popup`（无高分屏 DPI 感知且为旧版经典控件样式）；在 MSVC 构建中嵌入 ComCtl32 v6 与 Per-Monitor DPI v2 应用清单（附运行时 SxS 激活上下文与 `MessageBoxW` 回退），按钮改为与 macOS 一致的「立即重启 / 稍后」，并支持系统深色标题栏与主标题/正文/脚注层级。

## [v0.5.0] - 2026-11-26

### 新增 (Added)
- **侧边栏面板原生柔面 UI 重构**：全面重设计右侧辅助面板（`AUX_PANE`）的 5 个标签页，采用基于宿主 CSS 变量的无边框柔面色阶体系（自动适配 Antigravity 明暗主题）；顶部状态栏预览支持一键折叠/展开（切入外观页时自动展开），导航升级为胶囊式标签栏。
- **概览页独立速度卡片与额度预计消耗**：上下文窗口与 Prompt 缓存改用双圆环主卡；费用区新增 Token 构成占比色条；将「速度与延迟」拆分为独立卡片并加入 TPS 渐变趋势折线图；Antigravity 额度卡片内嵌每周额度预计消耗/耗尽时间与容量估算（`cap ≈`）。
- **用量页新增独立日历热力图**：重构用量页层级——顶部聚焦今日 KPI 与今日缓存未命中提示；「花费趋势与每日明细」自带 `7天 / 30天 / 全部` 切换，优化多模型堆叠柱状图比例与辅助参考线；新增独立的**「活跃度热力图」**卡片（展示近 20 周至最长 400 天全量历史，支持按花费 / Token / 调用切换、悬浮查看当日各模型明细、点击直接下钻单日调用，并统计活跃天数、连续天数与单日峰值）。
- **设置页与外观页交互升级**：设置页顶部整合「调试连接状态 + 版本更新」柔面横幅与可展开诊断卡；原生复选框升级为平滑开关，12 项状态栏分段改为带勾选图标的胶囊标签云，补充说明收入可点击的 `?` 抽屉；外观页改为双列柔面配色卡与胶囊分段控制器。

### 改进与修复 (Fixed & Improved)
- **修复 `language_server` 内存持续增长**：此前 sidecar 会向 `language_server` 查询它尚未加载的对话（当天所有对话与子代理、历史子代理补全、面板切换的对话），每次查询都会让其把整段对话载入内存且直到 Antigravity 重启才释放，大量子代理时可涨至数 GB。现在只对 `language_server` 已加载的对话调用接口，其余对话改为只读读取 `conversations/<id>.db`（含用量、上下文、压缩点、子代理归属与工作区），统计结果不变。
- **Agent Team / 大量子代理场景性能优化**：
  - Hook 不再每次都触发全量刷新：轨迹列表 Hook 后最快 1.5 秒刷新一次，额度在 Stop 后最快 30 秒刷新一次，引擎循环最快 250ms 一轮。
  - 移除 `PostToolUse` Hook，避免每次工具调用都同步拉起一次进程。
  - 只实时轮询状态栏能显示的子代理（每个根对话 4 个），步骤列表从末尾附近开始拉取。
  - 仅用于今日花费统计的屏幕外对话最多每 5 秒刷新一次；每轮最多读取 24 个对话库；没有对话库的对话失败后退避 60 秒。
- **长时间运行不再内存增长**：
  - 屏幕外、非今日且 10 分钟未用的对话、子代理步骤、Hook 与 git 缓存会被回收。
  - transcript 补同步只在对话变化后 10 分钟内进行。
  - CLI 调用缓存按 32 天裁剪。
  - 额度采样对所有账号统一按 8 天裁剪。
  - CDP 会话每 60 秒释放 V8 保留的 console 对象，且不再解析 console / 异常事件。
- **面板与用量页加速**：
  - 引擎锁只在规划与合并时持有，语言服务器请求、SQLite、目录扫描与 git 均在锁外执行，面板请求不再被慢调用卡住。
  - 用量页按对话库签名进程级缓存调用记录（同机重复请求由约 41ms 降至约 1ms）。
  - 面板会话标题每 10 秒在锁外刷新一次；「调用」页只下发最近 300 条（附总数）。
  - 面板轮询改为串行，页面隐藏时暂停。
  - 用量历史与采样文件在释放锁后写盘。
- **其他稳定性改进**：CDP WebSocket 握手增加超时；状态栏列对齐改为批量读写以避免反复强制布局；单日明细刷新定时器不再泄漏；今日花费按对话缓存，价格匹配不再逐条分配内存。
- **macOS 调试模式重启修复与加固**：修复 PID 回绕后 `target_app()` 误将 `Antigravity Helper (Renderer).app` 识别为主应用导致重启无响应的问题（过滤嵌套 Helper 子包及 `--type=` / `--import=` 进程并校验主程序路径）；重启脚本改用 `ps` + `awk` 字面匹配 PID 与参数化 `osascript`，并支持 `osascript quit` → `SIGTERM` → `SIGKILL` 逐级退出。
- **进程校验状态同步**：CDP 调试端口连接后同步更新 `relaunch-state.json` 中的 `last_flag`，发起重启时清空旧状态，且已连接时不再显示过期的 `进程校验: noflag`。
- **重启弹窗确认与超时恢复**：将 Settings 页手动重启按钮由内联二次点击改为主题自适应的模态确认弹窗（存在运行中对话时标红提示），并增加 20 秒退出超时自动恢复机制，避免异常时按钮卡死。

## [v0.4.0] - 2026-11-25

### 新增 (Added)
- **400 天用量看板与 `agy CLI` 合并**：历史保留期扩展至 400 天，新增「全部」视图（超过 62 天自动按周合并）；只读扫描 `~/.gemini/antigravity-cli/conversations/*.db`（配合 `conversation_summaries.db` 与 `cli-index.json`），将桌面端与 `agy CLI` 用量合并统计，状态栏花费悬停可查看今日 CLI 占比。
- **6 类缓存未命中归因分析与高价提醒**：自动识别上下文压缩、空闲超时、切换模型等 6 类缓存未命中并量化浪费费用；新增可配置单次高价提醒阈值（`costly_call_usd`，默认 `$0.50`），触发时状态栏显示高亮 `⚡` 徽章并在悬停提示展示归因详情。
- **上下文压缩摘要查看器**：Overview 与 Turns 页支持点击压缩节点按需查看完整压缩摘要（读取本机 `transcript_full.jsonl`，受面板令牌鉴权保护且不落盘复制）。
- **单日明细与筛选增强**：单日下钻支持「按次调用 / 按对话」双视图切换，新增来源（桌面 / CLI）、模型标签、仅看缓存未命中筛选与加载更多分页。
- **每周额度周期状态机**：支持同日多周期分段净变化累计、提早重置检测与 `↺` 标记、按周期起点精算容量（`cap ≈`）、过往周期容量记录以及历史采样回放。
- **中文界面 v12 与 Swarm 同步翻译**：同步 `agy-zh-pack` v2.0.12（3,469 条词典、111 条正则规则）与翻译器 v12；支持 Swarm 面板 iframe 同步翻译，新增全局临时切回英文快捷键（macOS `Ctrl+Option+E` / Windows `Ctrl+Alt+E`，跨窗口与 Swarm 同步，不改配置）。
- **侧边栏卡死自恢复**：CDP 监听自动检测并重载卡死在 `"plugin unavailable"` 的侧边栏面板。

### 改进与修复 (Fixed & Improved)
- **鉴权与数据安全**：`GET /api/checkpoint` 强制校验 `X-Sidecar-Token` 面板令牌（前端改经 `window.sidecar.fetch` 请求）；移除原子写入失败时的截断降级路径，损坏的配置文件或历史 JSON 自动备份为 `*.bad-YYYYMMDDHHMM.*` 而不静默覆盖。
- **并发与用量准确性**：用量页改为短锁快照 + 锁外读取与单请求 `DayCallCache`，避免阻塞状态栏实时刷新；单个损坏的 CLI 会话库不再阻断其余正常库更新；调价日明细不完整时保留持久化日汇总总量；跨午夜首次调用引入前一日对话基准避免漏算缓存未命中；周额度 `capExact` 仅在所有非空边界日均有明细时标记为精算。
- **界面与翻译细节**：压缩摘要面板增加请求序号防乱序覆写；周视图平均线改按图中活跃周实际花费计算；保存其他设置时不再重置临时英文状态；关闭中文前先执行 `restoreAll()` 彻底还原 Swarm 规则译文；修复子代理结束补拉重试时机并将 CDP `/json/list` 超时提升至 4 秒。

## [v0.3.0] - 2026-11-23

### 新增 (Added)
- **每日用量看板与历史补算**：右侧面板新增 `Usage` 标签页（今日 4 格 KPI、7/30 天按模型堆叠柱状图、每日汇总表与单日逐次调用下钻）；首次运行后台只读打开 `conversations/*.db` 补算历史天数；按账号隔离追踪每周额度净变化、容量估算（`cap ≈`）与重置预估（`est`）。
- **分层主题画廊与外观定制**：新增 `Theme` 标签页，提供 8 套明暗双版配色（Cometix / Catppuccin / Tokyo Night / Rosé Pine / Gruvbox / Nord / Dracula / Mono）实时预览卡片，以及外形（`plain`/`pill`/`powerline`）、额度进度条（`thin`/`cells`/`ring`/`none`）、信息密度与 4 种内联图标样式。
- **更新中心与在线数据同步**：`Settings` 新增更新中心，支持 GitHub Releases `SHA256SUMS` 校验一键自更新（含备份回滚与 `.git` 开发目录保护），以及按 commit SHA 同步 `agy-zh-pack` 词典、规则与官方价格表；自动适配环境变量与 macOS/Windows 系统代理。
- **价格体系与子代理步骤时间线**：支持 Claude 5 分钟缓存写入计费、按生效时间分时调价（含 Gemini 4 Carbon/Argon 与 3.8 Flash 调价日）及在线价格覆盖；子代理树形行按需增量拉取步骤，展示审批等待（`WAITING`）、实时耗时与最近 5 步时间线悬停提示。
- **跨平台调试模式重启守卫**：支持 macOS 与 Windows 一键以 `--remote-debugging-port=0` 重启 Antigravity，提供 `不处理 / 弹框询问 / 自动重开` 策略并在触发前核验主进程命令行防误重启。
- **中文界面翻译器 v10**：同步 `agy-zh-pack` 2.34.0 词典与翻译器 v10，关闭中文时即时还原英文无需刷新窗口。

### 改进与修复 (Fixed & Improved)
- 修复历史调用按实际时间戳归属日期、历史子代理补算及跨天对话花费显示不准确的问题。
- 优化状态栏自绘悬停提示（不被重绘打断）与空闲占位防跳行布局。
- 修复 Linux 目标上的 Clippy 警告；重写中英文 README、补充状态栏预览图并将开发文档迁移至 `docs/`。

## [v0.2.0] - 2026-11-20

### 新增 (Added)
- **原生 Rust Sidecar 与输入框状态栏**：直接读取本机 `language_server` 只读接口并配合本地 Hooks 即时唤醒，经 CDP 向主窗口输入框上方注入两行状态栏（模型、Git 仓库状态、上下文占比与压缩徽章、Prompt 缓存命中率、折算花费、TTFT、输出速度、5h/7d 额度与子代理行）。
- **右侧辅助面板（`AUX_PANE`）**：提供顶部实时状态栏预览、Overview、Turns 与自动保存的 Settings（含 12 项分段独立开关）。
- **可选中文界面与调试重启**：集成 `agy-zh-pack` 中文词典，支持从面板触发带调试端口重启。
- **跨平台启动与 CI/CD**：移除 sidecar 与 hook 启动对 POSIX shell 的依赖以兼容 Windows；接入 GitHub Actions 自动化检查以及 macOS Universal / Windows x64 发版打包。

### 改进与修复 (Fixed & Improved)
- 统一由实时引擎驱动右侧辅助面板，移除旧版 transcript 桥接代码。
- 修复 Antigravity 重启后查找未加载对话、按模型列表匹配首页额度组、跟随系统明暗主题、`language_server` 断开后自动退出以及面板轮询同步等问题。

[Unreleased]: https://github.com/BlueOcean223/agy-statusline/compare/v0.5.2...HEAD
[v0.5.2]: https://github.com/BlueOcean223/agy-statusline/compare/v0.5.1...v0.5.2
[v0.5.1]: https://github.com/BlueOcean223/agy-statusline/compare/v0.5.0...v0.5.1
[v0.5.0]: https://github.com/BlueOcean223/agy-statusline/compare/v0.4.0...v0.5.0
[v0.4.0]: https://github.com/BlueOcean223/agy-statusline/compare/v0.3.0...v0.4.0
[v0.3.0]: https://github.com/BlueOcean223/agy-statusline/compare/v0.2.0...v0.3.0
[v0.2.0]: https://github.com/BlueOcean223/agy-statusline/releases/tag/v0.2.0
