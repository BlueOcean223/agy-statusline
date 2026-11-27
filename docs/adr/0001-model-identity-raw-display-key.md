# 计价按 raw+display 匹配，每日聚合的键存为 "raw|display"

实时调用一直用 `"{resp_model} {display}"` 匹配模型规则，而 `usage-history.json` 里的每日聚合只按 raw（缺失时用 display）分键，于是同一个模型在实时路径和历史路径上可能匹配到不同规则（例如 display 为 `Gemini 4 Hydrogen (Orchestrator)` 的调用）。我们决定所有路径都按模型标识（raw 与 display 两部分）匹配，每日聚合（`raw`、`cli_raw`、`by_conv`）的键改为 `"raw|display"` 字符串；不含 `|` 的旧键视为 display 为空，计价结果与旧版一致，因此不做数据迁移。升级当天同一模型可能暂时分成新旧两行，总量不变。

## Considered Options

- **只按 raw 匹配**：持久化数据不变，但依赖 display 区分的规则会失效，状态栏显示的价格也会变。
- **改成结构化数组 `Vec<{raw, display, agg}>`**：类型更清楚，但要写一次性迁移，还要兼容旧格式的反序列化。
- **存小写拼接的 price key**：实现最简单，但之后无法再拆回 raw 和 display。
