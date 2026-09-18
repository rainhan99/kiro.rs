# 多上游网关：路由与计费

给一个对外的模型别名配置多条上游路线，按路记账。**没有 `gateway.json` 时这一切都不存在**：
所有请求原样走既有 Kiro 路径，行为与引入这个特性之前完全一致。

- 配置位置：缓存目录下的 `gateway.json`（与 `traces.db` 等并列）
- 账本：同目录下的 `billing.db`
- 管理界面：Admin → 设置 → **多上游网关**；某个 Key 的额度在 Admin → 客户端 Key → 钱包图标

---

## 一、最小可用配置：`opus5` 走主备两组

目标：客户端照常请求 `opus5`；优先走 Kiro 主号组，主号不行时落到一个按人民币计费的
直连 Anthropic 上游。

```jsonc
{
  "defaultRoutingMode": "sticky",
  "affinityTtlSecs": 900,
  "maxAttempts": 3,
  "requestTimeoutSecs": 120,

  "upstreams": [
    {
      "id": "kiro-main", "name": "Kiro 主号组", "kind": "kiro",
      "enabled": true, "weight": 10,
      "kiroGroup": "main"          // 只有 kiro 类型能设；地址与凭据都来自既有凭据池
    },
    {
      "id": "kiro-backup", "name": "Kiro 备号组", "kind": "kiro",
      "enabled": true, "weight": 5,
      "kiroGroup": "backup"
    },
    {
      "id": "anthropic-direct", "name": "Anthropic 直连", "kind": "anthropic",
      "enabled": true, "weight": 10,
      "baseUrl": "https://api.anthropic.com",
      "apiKey": "sk-ant-..."       // 读接口永不回显；保存时省略即沿用原值
    }
  ],

  "models": [{
    "id": "opus5",
    "bindings": [
      { "id": "main",   "upstreamId": "kiro-main",   "upstreamModel": "claude-sonnet-4",
        "enabled": true, "priorityTier": 0, "weight": 10,
        "contextWindow": 200000, "maxOutputTokens": 8000,
        "supportsTools": true, "supportsImages": true, "supportsReasoning": false,
        "billingUnit": "kiroCredit" },

      { "id": "backup", "upstreamId": "kiro-backup", "upstreamModel": "claude-sonnet-4",
        "enabled": true, "priorityTier": 0, "weight": 5,
        "contextWindow": 200000, "maxOutputTokens": 8000,
        "supportsTools": true, "supportsImages": true, "supportsReasoning": false,
        "billingUnit": "kiroCredit" },

      { "id": "paid",   "upstreamId": "anthropic-direct", "upstreamModel": "claude-sonnet-4-20250514",
        "enabled": true, "priorityTier": 1, "weight": 10,
        "contextWindow": 200000, "maxOutputTokens": 8000,
        "supportsTools": true, "supportsImages": true, "supportsReasoning": false,
        "billingUnit": "CNY",
        "costPrices": { "currency": "CNY", "input": "21.6", "output": "108",
                        "cacheRead": "2.16", "cacheWrite": "27" },
        "sellPrices": { "currency": "CNY", "input": "30",   "output": "150",
                        "cacheRead": "3",    "cacheWrite": "37.5" } }
    ]
  }]
}
```

**两个权重是干什么的。** `priorityTier` 是**层**：先挑层号最小、且当前可用的那一层，选不出来
才降到下一层。`weight` 只在**同一层内**起作用。上面三条绑定里，`main` 与 `backup` 同在第 0 层，
按 10:5 分流；只有这一层全部不可用，才轮到第 1 层的 `paid`。

**模式的继承。** 别名可以用 `routingMode` 覆盖全局的 `defaultRoutingMode`；不写就跟随全局。

- `sticky`（默认）：同一会话尽量落在同一条路上。
- `weighted_random`：同层内按权重抽样。这个模式会**同时关掉 Kiro 自身的会话粘性**，
  否则第一次选中的凭据会接管整个会话，"随机"只在第一次生效。

---

## 二、账户与额度

额度挂在 **(Key, 币种)** 上，不是挂在 Key 上。一个积分账户已耗尽、但人民币账户有余额的 Key，
走按钱计费的那条路完全应该放行；反过来，一个**只有**积分账户的 Key 够不到按钱计费的备路——
它在那个币种上根本没有账户。

在 Admin → 客户端 Key → 钱包图标里按币种分别设置。

**「不限额」与「上限 0」是两回事。** 前者随便花，后者一分都不能花。界面把它们分开，
配置里 `null` 与 `"0"` 也分开。

**软额度与硬额度。**

- **硬额度**要求每条候选路都能算出一个**保证不被突破**的上界：由配置里声明的
  `contextWindow`、`maxOutputTokens` 与轮次上限推出，输入侧取最贵的单价。算不出就
  **拒绝那条路**，而不是拿本地 token 估算冒充保证——估算偏低时，超出的部分是已经花掉的钱。
- **软额度**允许没有上界的预留。原生积分本来就没有可证的调用前上界，这是软额度存在的理由。
  代价是：**带上限的软额度会被突破**，它是警戒线不是闸门。

**没有用量证据时不计费，也不当作没发生。** 上游没给出可确认的用量，那一笔记为**待结算**，
既不计入已用，也不释放。绝不用 0 顶替——0 的意思是"确认没花钱"，与"不知道花了多少"是两回事。

---

## 三、遗留 Key 的迁移

首次带着 `gateway.json` 启动时，会为每个尚未立户的遗留 Key 建立**积分开账余额**：
已用取 `total_credits`，上限取 `max_credits`。这是一笔开账余额，不是把历史请求重放一遍。

- **幂等**：已经立过户的 Key 永不再导。遗留的 `total_credits` 在网关接管后仍被老路径累加，
  再导一次要么二次扣费、要么让启动失败。
- **有数字表示不了就不封存**：那个 Key 不立户，日志点名说明原因，迁移保持开着；
  修好数据重启即可补上。全部导完才封存，此后新建的 Key 从零开始。
- **删 Key 不删账本**：账本记得所有出现过的 key_id，启动时据此抬高新 Key 的 id 下界，
  已删 Key 的身份永不重用——否则新 Key 会继承前一个同 id Key 的余额与历史。

**「重置统计」动不了账本。** 它只清分析视图。客户端 Key 页上的「积分上限」管的是
**既有 Kiro 路径**；网关接管的别名由账本管。

---

## 四、上线与回滚

**上线**

1. 写 `gateway.json`，先只放一个别名、一条绑定，`enabled: false`。重启。
2. Admin → 设置 → 多上游网关，确认「当前接管的别名」为空（全部停用的模型不算接管）。
3. 在该页的**路由预览**里填 Key id 与别名推演：它只做判定，不预留额度，也不向上游发任何请求。
   预览会逐条说明每个候选为什么可用或被拒。
4. 给这个 Key 设好对应币种的额度，再把绑定 `enabled` 打开并保存。
5. 用一个真实请求验证，然后在钱包面板里核对已用量。

**回滚**

- 把别名的全部绑定停用：该别名立刻不再被接管，请求回到既有 Kiro 路径。**已在飞的请求**
  按它开始时的那份快照走完，不会被改到脚下。
- 彻底停用：删除或改名 `gateway.json` 后重启，网关回到惰性状态。
  `billing.db` 留着，历史不丢。

---

## 五、已知的协议限制（是拒绝，不是降级）

- **跨协议流式不支持。** 流转换器只提取用量与终结信号，不做帧级协议转换。上下游协议不一致的
  路承不住流式请求，会被跳过换下一条协议一致的路——而不是把 Chat 的帧发给一个等着
  Anthropic 事件的客户端。
- **请求转换只有 Anthropic → Chat Completions / Responses 两个方向。** 其余方向直接拒绝，
  不做近似。
- **带厂商签名的状态无法跨厂商。** 推理签名、`previous_response_id` 之类在另一家那里
  重新签不出来也不存在，携带它们的转换一律拒绝，而不是悄悄丢掉——丢掉会让请求看起来正常
  却丢失了上下文，表现为模型"变笨"。
- **Kiro 路由由既有通道执行**，不走网关自己的传输层。网关层的跨路重试对它不适用；
  Kiro 自身的凭据轮换是等价物，且早已有界。Kiro 这一路**失败**时（且尚未向客户端发出内容）
  网关会换到直连备路。

---

## 六、这些数字是什么，不是什么

- **原生 credit** 是上游 `meteringEvent` 下发的真实计费量，逐位记入账本。
- **按钱计费**的金额由配置里的售价乘以原生用量算出，全程十进制定点数，从不经过浮点。
- **本地缓存计量**（`allowSimulatedCache`）是模拟，只拆分 token 计数，
  **永远变不成钱**——账本只认原生用量。
- **会话粘性命中**不等于缓存命中。粘性只说明这次和上次用了同一个凭据。
- **拿不到的原生计数就是拿不到**，界面显示为未知，不会被归一成 0。
