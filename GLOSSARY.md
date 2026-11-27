# agy-statusline

Antigravity 插件：统计模型调用的 token 与花费、追踪额度，并在 IDE 内显示状态栏与侧边栏面板。

## 调用与用量

**Call（调用）**:
一次模型请求及其 token 用量（输入、缓存读、缓存写、输出），带发生时间与所属对话。
_Avoid_: request, generation, record

**Response ID（响应 ID）**:
模型提供方给一次调用分配的唯一 ID；同一次调用在 language_server 返回值与对话库中取值相同，用来判断两处记录是否为同一次调用。
_Avoid_: request id, message id

**Source（来源）**:
调用的产生端：桌面端 Antigravity 或 `agy CLI`。
_Avoid_: origin, client

**Local day（本地日）**:
本机时区里的一个日历日，从当天第一个存在的时刻到次日第一个存在的时刻；夏令时切换日为 23 或 25 小时。用 `YYYY-MM-DD` 作键。
_Avoid_: day window, 24h day

**Daily aggregate（每日聚合）**:
某一本地日按模型标识与来源汇总的调用数与 token，是 400 天用量历史的单位。
_Avoid_: day record, bucket

## 计价

**Model identity（模型标识）**:
由后端模型名（raw）与显示名（display）组成的一对；计价按两者一起匹配。只有 raw 的旧数据视为显示名为空。
_Avoid_: model key, bucket key, price key

**Model rule（模型规则）**:
按模型标识子串匹配的一条定价，含显示名、短名、上下文上限与单价；按顺序取第一个匹配的规则。
_Avoid_: price rule, pattern

**Price tier（价格档）**:
一条模型规则在某个截止时间之前生效的单价。
_Avoid_: price period, tiered price

**Price-cut day（降价日）**:
当天内有任一价格档截止的本地日；这一天的花费按调用发生时间精确计价。
_Avoid_: transition day, tier change day

**Remainder（剩余用量）**:
降价日里，每日聚合中多于能找到的精确调用的那部分用量；按当天本地时间中午（墙钟 12:00）的价格计价。
_Avoid_: leftover, residual

**Other（未定价模型）**:
没有匹配到任何模型规则的模型；花费计为 0。
_Avoid_: unknown, unmatched

**Pricer**:
决定一个模型标识对应哪条模型规则、一次调用或一天用量花多少钱的唯一来源，由配置规则与在线价格表合成。
_Avoid_: price table, cost calculator
