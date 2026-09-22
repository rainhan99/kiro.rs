# Kiro 跨上游可移植历史设计

## 状态

本设计于 2026-09-22 逐段确认。它解决的具体问题是：Claude Code 或其他 Anthropic
客户端经 cc-switch 从 Claude 官方 API 切到 kiro.rs 后，会把先前上游产生的内容块放进
后续请求历史；Kiro 适配器无法表达其中一部分形状，当前实现又没有完整递归地在转换前
处理这些块，因而在 `pipeline_preparation` 阶段以
`content block type is unsupported by the Kiro adapter` 拒绝请求。

目标不是让 Kiro 原生理解所有 Claude 私有协议状态，而是建立一层明确、可审计、确定性
的历史兼容边界：尽量保留人可读的业务上下文，绝不把上游专属的不透明状态转发给 Kiro，
绝不静默遗漏内容，并让跨上游会话能够在安全边界内继续运行。

## 已确认的约束

- 历史内容允许转换为可移植文本；当前轮输入必须忠实表达，不能为了成功而悄悄降级。
- 本平台的告警、路径、哈希和转换原因只留在本地日志与 trace，不进入 Kiro prompt。
- `signature`、`encrypted_content`、`redacted_thinking.data` 等 provider-bound 数据不发给
  Kiro；本地审计也不保存其明文。
- 工具调用的 ID、调用与结果配对、结果错误状态和顺序必须保持。
- 不获取 URL，不解码或解释加密字段，不执行历史 server tool，不自动重试探测上游能力。
- 转换必须确定、幂等、原子；转换失败时不得发送半转换请求。
- 协议兼容属于正确性，不受 `PipelineMode::Off/Audit/Enforce` 控制。

## 非目标

- 不承诺 Claude 与 Kiro 对同一段历史产生等价推理结果。
- 不为 PDF、Office 文件或图片增加 OCR、全文解析器或外部抓取服务。
- 不伪造 Claude thinking signature，也不尝试把加密/脱敏思考恢复成文本。
- 不把未知角色猜成 `user` 或 `assistant`，不通过删除一半工具调用来修复配对。
- 不在本阶段建立跨请求的持久历史数据库；客户端仍是会话历史的权威持有者。

## 方案选择

采用独立的 `PortableHistoryNormalizer`，放在 Anthropic 入站模型和 Kiro converter 之间。

未采用的方案：

1. **继续 `drop`**：请求可能成功，但业务历史会静默或半静默丢失，用户已明确拒绝。
2. **让 converter 认识所有外部块**：会把协议兼容、隐私取舍和 Kiro wire 构造耦合在
   一起；每个调用点都可能形成不同的遗漏规则，无法集中审计。
3. **原样 JSON 文本化所有未知块**：虽然容易实现，但会把 `encrypted_content`、签名、
   大段 base64 和内部字段一起送给 Kiro。现有 `normalize_server_history` 正存在这一风险，
   本设计会替换它。

独立归一化层让 converter 只接收它明确支持的规范形状，并把“内容如何降级”的策略保留
在一个可以单独测试和审计的模块中。

## 当前输入与历史边界

归一化器先找到最后一条有效、非空的 `user` 消息，它是本次生成的 current frontier。
该消息的整棵内容树，包括其 `tool_result.content`，都属于当前输入；此前所有消息属于历史。

- `portable-text` 只对历史执行兼容转换。
- 当前输入出现 Kiro 无法忠实表达的块时返回
  `portable_history.current_unexpressible`，不发送请求。
- 最后一条有效 user 消息之后若还有 assistant 内容，属于 assistant prefill；Kiro 无法按该
  前缀续写，继续明确拒绝。
- 没有有效最终 user 消息、消息角色未知或消息内容畸形时，不猜测边界，直接拒绝。
- 显式选择旧 `drop` 的操作者仍获得原有有损行为；这是为配置兼容保留的 deprecated
  escape hatch，不是 `portable-text` 的行为。

这一边界使“恢复旧会话”和“提交一个新的当前附件”得到不同处理：前者可被安全文本化，
后者若无法完整送达就让客户端知道，而不是给出一个基于缺失输入的答案。

## 历史内容映射

遍历必须覆盖顶层 `message.content` 和任意深度的 `tool_result.content`。路径以
`messages[3].content[1].content[0]` 形式记录，最大递归深度沿用或严于现有 token 遍历的
防护；超过深度属于畸形输入而不是可降级内容。

| 输入形状 | 历史映射 | 发送给 Kiro 的内容 |
| --- | --- | --- |
| `text` | 原样保留 | 原文本 |
| Kiro 支持的 user base64 `image` | 原样保留 | 既有图片通道 |
| 历史 URL 图片或 assistant 图片 | 不联网、不迁移角色 | 安全附件说明文本 |
| `tool_use` | 保留 `id/name/input` | 原工具调用 |
| `tool_result` | 保留 `tool_use_id/is_error`，递归处理 content | 原工具结果关系与规范化正文 |
| `thinking` | 保留可读 `thinking`，移除 `signature` | 思考文本，不含 Claude 签名 |
| `redacted_thinking` | 不读取、不转发 `data` | 中性说明：历史中存在已脱敏思考 |
| `server_tool_use` | 只接受已完成且配对明确的历史搜索 | 带“已完成历史记录”标签的引用文本，不重新执行 |
| `web_search_tool_result` | 保留公开可读的 title、URL、snippet/error；删除加密字段 | 不可信引用资料文本 |
| 文本型 `document` | 复制明确的 UTF-8 文本字段；text MIME 的 base64 可在本地有界解码 | 带文档来源标签的引用文本 |
| PDF、二进制或 URL `document` | 不解析、不抓取 | 文件名/标题、媒体类型、可用大小的附件说明；无哈希和内部告警 |
| 其他未知块 | 只从白名单字段提取人可读文本；没有可读字段时生成类型占位符 | 有界、带“历史引用”标签的文本 |

未知块的白名单只包括协议中明确作为展示内容的字符串，例如 `text`、字符串
`content`、`title`、`url`、`name`、`message`。不得序列化整个对象，不得递归捞取任意
字符串，因为任意字段可能是签名、密钥或 provider 私有状态。标签明确说明这些是历史
引用数据，不是新指令或待执行工具。

文本化不做摘要或改写。标签和元数据有固定长度上限；转换结果不得因标签无限膨胀。
二进制 base64 不进入文本通道。所有现有入口和最终 wire 预算仍在归一化后执行。

## 组件和接口

新增 `src/pipeline/portable_history.rs`，公开窄接口：

```rust
pub fn normalize(
    payload: &MessagesRequest,
    strategy: UnexpressibleStrategy,
    fingerprint: &dyn SensitiveFingerprint,
) -> Result<NormalizationOutcome, PortableHistoryError>;

pub struct NormalizationOutcome {
    pub payload: MessagesRequest,
    pub report: NormalizationReport,
}
```

函数接收不可变请求并返回新值，从接口上保证失败不会留下半修改状态。实现可在内部 clone
后原地处理，但只有完整归一化、结构校验和工具配对校验都成功才返回结果。

`NormalizationReport` 只包含：块路径、原类型、动作枚举、输入/输出字节数和使用现有
进程 epoch 密钥生成的 HMAC 指纹。它不包含正文、URL、文件名、tool ID、签名、加密数据
或原始 JSON。报告的动作包括 `preserved`、`portable_text`、`opaque_redacted`、
`legacy_dropped`；无变化块不逐条记录，避免 trace 无界增长，只保留总数。

`RequestPipeline::prepare` 改为返回 `PrepareOutcome`，携带现有 artifact session 和本次
归一化报告。handler 将报告随本请求传到现有 wire audit/trace sink；没有启用审计时只
保留结构化 warn 计数，不持久化报告。报告永远不写回 `MessagesRequest.metadata`。

现有 `normalize_server_history` 迁入新模块并按上述白名单重写。`expressible.rs` 保留策略
枚举和 legacy drop/refuse 所需逻辑，但 `portable-text` 由新模块实现。converter 的
`extract_tool_result_content` 改为返回 `Result`；归一化后若仍遇到未知块，返回内部不变量
错误，删除 `_ => {}` 静默忽略路径。

## 调用时序

```text
Anthropic 请求反序列化与入口字节上限
  → thinking 配置覆盖
  → PortableHistoryNormalizer（所有 PipelineMode 都执行）
  → 结构与工具配对校验
  → Enforce 专属变换（billing、图片、artifact 等；非 Enforce 可跳过）
  → 基于规范化 payload 的 token/缓存指纹/预算计算
  → Kiro converter（只接受规范形状）
  → endpoint 变换与最终 wire 预算
  → Kiro 上游
```

归一化必须发生在当前 `PipelineMode` 提前返回之前。任何 token 估算、缓存指纹或最终大小
证据都以规范化后的 payload 为准，避免审计数字描述一个实际上没有发送的请求。

工具配对校验在归一化后执行，但归一化器本身不得创建、删除或改写客户端 `tool_use_id`。
已完成 server search 使用自己的严格配对校验后文本化，不进入客户端工具配对集合。

## 配置与迁移

`requestPipeline.unexpressible` 扩展为：

```json
"unexpressible": "portable-text" | "refuse" | "drop"
```

- `portable-text` 是新安装和字段缺省时的默认值。
- 现有配置中显式写出的 `drop` 或 `refuse` 保持原值和原语义，不自动重写磁盘文件。
- `drop` 在配置文档和后台 UI 标记为 deprecated/有损；新 UI 默认选择
  `portable-text`。
- 旧的 `prefill` alias 继续解析，避免无关的配置启动回归。
- 后台保存仍提交完整 requestPipeline；TypeScript 联合类型、草稿缺省值、选项说明和
  前后端往返测试必须同步更新。
- 与已有 requestPipeline 新字段一样，使用新值保存配置后回退到旧二进制会因
  `deny_unknown_fields`/未知枚举值而拒绝启动；回滚文档必须要求先改回 `drop/refuse`。

策略与 `PipelineMode` 正交。即使 mode 为 `off` 或 `audit`，`portable-text/refuse/drop`
仍执行；mode 只决定后续优化和强制预算变换。

## 错误与原子性

新增 typed error，内部稳定代码为：

- `portable_history.malformed`
- `portable_history.tool_pairing`
- `portable_history.current_unexpressible`
- `portable_history.budget_exceeded`
- `portable_history.invariant_violation`

结构、配对和当前输入错误使用 HTTP 400；归一化后超过本地已配置预算使用 HTTP 413。
两者都沿用 Anthropic 兼容 error envelope，并对客户端显示稳定代码、无敏感内容的块路径
和安全原因。`invariant_violation` 对客户端使用通用消息，详细位置仅进入本地日志。

任何错误都发生在 provider 调用之前：不选号重试、不换账号、不发送部分请求。输入
payload 只有在完整 outcome 成功后才被替换。日志和 trace 失败不得反过来让业务请求
失败，但也不得把报告拼进 prompt 作为兜底。

## 安全与本地可观测性

本地 `wire_audit` 增加 `normalization` 摘要：策略、扫描块数、转换块数、按动作/原类型的
计数、是否触及历史、不透明数据总字节数和有界事件列表。详细事件使用 keyed fingerprint
并带 `fingerprintEpoch`；它们只能在同一进程 epoch 内关联，不能还原内容。

以下内容既不进入 Kiro 请求，也不进入日志/SQLite 明文字段：

- Claude thinking signature；
- redacted thinking data；
- web search encrypted content；
- document/image base64；
- 未知块原始 JSON；
- 本平台生成的路径、错误说明和审计哈希。

发送前测试与可选 debug assertion 会对最终 Kiro JSON 使用哨兵值做负向检查。生产逻辑不
通过扫描任意用户文本关键词来判定泄漏，因为业务正文可能合法包含相同词；泄漏保证来自
白名单构造和类型边界。

## 测试策略

### 单元测试

- 表驱动覆盖映射表中每种内容块，包含顶层和多层 `tool_result.content`。
- 同一请求归一化两次，第二次报告无新动作且序列化 JSON 完全一致。
- 原请求在成功与失败路径都不被修改。
- unknown block 只提取白名单字段；嵌入 sentinel 的签名、加密数据和 base64 在输出中均
  不存在。
- 文本 document 保留 UTF-8；非法 base64、非 UTF-8、超深嵌套和畸形内容明确失败。
- URL 图片/document 不产生任何 HTTP、DNS 或 provider 调用。
- 工具 ID、顺序、`is_error` 保持；孤儿、重复、未完成和歧义配对全部失败。
- 最后 user 当前输入中的未知块失败，同形状位于较早历史时转换成功。
- 尾部 assistant prefill 和未知角色失败。

### 配置和模式测试

- `portable-text/refuse/drop` 的 serde、缺省值和旧显式配置兼容。
- `Off/Audit/Enforce` 三种 mode 都执行归一化；只有既有 Enforce 变换保持原门控。
- 后台 GET/PUT 往返、草稿缺键 fallback、选项标签和旧配置保存测试同步更新。
- CLI `--check-config` 与 `--inspect-request` 能接受并展示新策略；inspect 只显示脱敏摘要。

### 转换器与端到端测试

- converter 面对未归一化未知块返回 invariant error，不能成功且遗漏内容。
- 使用脱敏 cc-switch 夹具重现原错误：历史嵌套未知块经 `portable-text` 后成功到达 fake
  Kiro upstream。
- fake upstream 捕获的最终 JSON 包含可读业务历史和完整工具关系，不包含本平台告警、
  路径、signature、encrypted/redacted data 或文档 base64 sentinel。
- `refuse` 保持零上游调用；显式 legacy `drop` 保持旧行为并产生本地报告。
- 归一化后的 token/字节审计与真正发送的 wire 一致。

完成后执行至少：

```sh
cargo fmt --all --check
cargo test --workspace --locked
cargo test --locked -p kiro-rs --no-default-features
cd admin-ui && bun test && bun run build
```

需要回环端口和 macOS SystemConfiguration 的 Rust 测试必须在允许这些能力的环境运行；
沙箱权限错误不能记作代码失败。

## 交付与回滚

交付时更新 request pipeline 运维文档和配置示例，明确 `portable-text` 的能力边界：它保留
可读历史，不保证跨模型语义等价，也不会读取二进制附件。管理后台展示“仅本地审计，
不会发送给 Kiro”。

运行时回滚优先把 `unexpressible` 改为 `refuse`，得到最严格且无损失误判的行为；确需旧
兼容时才使用 deprecated `drop`。二进制回滚前必须把 `portable-text` 改成旧版本认识的
值。此功能不做数据库破坏性迁移；新增 trace JSON 字段对旧记录读取保持兼容。

## 验收标准

1. 脱敏复现夹具不再在 `pipeline_preparation` 报 unsupported content block，并能完成 fake
   upstream 调用。
2. 可读历史、工具关系和错误状态保留；当前不可表达输入仍明确拒绝。
3. 最终 Kiro wire 中不存在 provider-bound 不透明数据或本平台审计信息。
4. 无内容块被 converter 静默忽略；未知规范形状必定报 invariant error。
5. 三种 PipelineMode 和三种 unexpressible 策略行为有自动测试证明。
6. 归一化幂等、原子、无外部网络访问，失败时上游调用次数为零。
7. 完整 Rust/前端测试、无默认特性测试、格式检查和生产构建通过。

