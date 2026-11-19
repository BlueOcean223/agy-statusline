# AGY CometixLine (`agy-statusline`)

纯 Rust 单二进制实现的 Google Antigravity 2.0 桌面端原生右侧辅助面板（`AUX_PANE`）与遥测状态栏插件，设计理念源自 [CCometixLine](https://github.com/Haleclipse/CCometixLine)。

## 架构特点

- **100% 纯 Rust 单体二进制 (`bin/agy-ccline`)**：零 Python、零 Node.js、零外部悬浮窗口。前端静态资源通过 `include_str!` 直接编译进单个 ~1.5MB 二进制文件。
- **双入口合一**：
  - `agy-ccline sidecar`：由 `sidecars/statusline/sidecar.json` 自动拉起，直接监听 `ANTIGRAVITY_SIDECAR_WEB_PORT` 提供原生右侧辅助栏视图。
  - `agy-ccline hook`：由 `hooks.json` 在模型调用前后触发（冷启动 `< 1ms`），记录毫秒级精确推理耗时用于计算 TPS。
  - `agy-ccline status`：在终端直接输出双行真彩 CCometixLine 状态条。
- **原生宿主融合视觉**：严格遵循 Antigravity 内置面板（如 `swarm_board`）的无边框极简规范，100% 继承宿主的 `--background` / `--foreground` / `--muted-foreground` / `Google Sans` 主题变量，顶部内嵌无外框双行 CCometixLine 状态条（Catppuccin Latte / Mocha 自动随深浅色切换）。
- **TOML 配置**：配置位于 `~/.gemini/antigravity/plugin_data/agy-statusline/config.toml`。

## 构建与更新

```bash
cd /Users/gongshuisanye/code/agy-statusline
cargo build --release
cp target/release/agy-ccline bin/agy-ccline
```
