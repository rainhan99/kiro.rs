# 请求管线：400、缓存与可验证配置

本次构建、测试结果与二进制校验值见 [交付验证记录](request-pipeline-verification.md)。

## 启用与回退

可以在 **系统设置 → 请求管线** 配置已有功能，或把 `config.pipeline.example.json` 的 `requestPipeline` 段合并进自己的配置。按下述工具兼容性要求选择顶层 `toolCompatibilityMode`，并关闭 `cacheMeteringEnabled`。不要覆盖已有密钥、账号、代理和管理端配置。管线配置修改后**重启生效**，不是热更新。

### 网页配置闭环

- 入口：`/admin/#/settings?s=pipeline`；桌面侧栏和窄屏设置导航均可进入。
- 表单覆盖执行模式、billing 净化、缓存策略、Agent Mode、入口/最终 body/文本/工具结果/图片字节预算、原文存储/分页参数、图片策略和审计开关。没有为尚未实现的 token 校准、Map-Reduce 等能力提供空开关；三阶段差距见 [需求覆盖核对](request-pipeline-coverage.md)。
- 点击“保存配置（重启生效）”才提交。页面分别展示**草稿、已保存配置、当前生效配置**；保存不会重建正在运行的原文存储，也不会中断活动请求或自动重启。
- 可选预算留空表示 `null`（停用该项），`0` 无效；整数范围和跨字段关系均由前后端校验。本编辑器固定 `kiroOnly:true`、`allowSimulatedCache:false`，旧配置若不同会先明确提示再随保存调整。
- 后台刷新、网络失败或保存冲突不会覆盖草稿；“重新读取”会先确认放弃未保存的修改。离开该分区会丢弃尚未保存的本地草稿，页面有提示。
- 管理员认证沿用现有规则。`GET /api/admin/request-pipeline` 返回 `effectiveConfig`（启动快照）、`savedConfig`（磁盘）、`revision` 和 `restartRequired`；`editable` 表示可通过本接口保存，`runtimeEditable:false` 只表示不支持热更新，**不再表示网页只读**。
- `PUT /api/admin/request-pipeline` 请求为 `{ "config": <完整 requestPipeline>, "revision": "<GET 返回值>" }`。服务端持有与其他管理配置相同的写锁，重新读取磁盘，仅替换 `requestPipeline`，保留其他字段和密钥；过期版本返回 **409 / configuration_conflict**，不覆盖他人的设置。不要在多个进程或外部编辑器中同时写同一文件；版本/写锁不声称是跨进程事务锁。
- 配置文件或其目录不可写、文件不存在时明确返回错误，不假报成功；原子替换需要**目录写权限**，只挂载单个不可替换文件的部署可能需要调整为挂载配置目录。临时文件先同步再替换，保留权限和符号链接目标；Unix 还同步父目录。替换完成后目录同步失败返回 **persistence_uncertain**，提示“磁盘可能已更新”，应重新读取确认。非 Unix 平台未验证断电持久性，不作同等保证。

本次代码与离线测试不修改现有运行配置，也不自动启用各模块；安装新构建后才会出现这个页面。

示例开启方案 C 的各模块。**示例中的 8 MB / 600 KB / 400 KB 是本地预算，不是 Kiro 官方阈值或实测上限。** 可设为 `null` 关闭单项限制；默认配置所有 wire 限制均为 `null`，默认不注入 cachePoint、不启用原文卸载。

| 配置 | 行为 |
| --- | --- |
| `mode: enforce` | 执行所配置的净化、图片/原文处理、缓存标记和本地预算拒绝 |
| `mode: audit` | 保留旧转换行为，仅观察 wire；不执行净化、卸载、切片或缓存标记 |
| `mode: off` | 同样保留旧转换；另设 `auditEnabled:false` 关闭 wire 审计 |
| `stripBillingHeader` | 仅删除第一个 system 块开头精确匹配的 `x-anthropic-billing-header:` 一行；不删除引文、后续块/行或任意“无效头” |
| `cacheStrategy: off/static-prefix` | 关闭或只在转换后的 system 历史消息末尾添加一个 `cachePoint:{type:default}`；不标记增长中的当前消息 |
| `limits` | 独立检查最终 JSON body 字节、最大 UTF-8 文本值、整个工具结果 JSON、单张 base64 字节；不是 token 计数 |
| `images.strategy: preserve` | 原样保留图片，不走旧有损压缩/历史去重 |
| `images.strategy: lossless-tiles` | 静态 PNG/JPEG/WebP 解码后按像素无损 PNG 切片；另附坐标；动画、容量不足明确报错 |
| `artifacts.enabled` | 仅卸载超长历史用户文本和工具结果文本，保留原文，注入本地 read/search 工具 |
| `toolResults.strategy: join/lossless-chunks` | `join` 为默认与既有行为（单个 text 条目）；`lossless-chunks` 把超过 `chunkBytes` 的工具结果切成多个 text 条目，逐字节可还原、切点落在 UTF-8 边界、与 `tool_use_id` 的配对不变。**不是卸载也不是摘要**：全文照发 |
| `toolCatalog.strategy: inline/on-demand` | `inline` 为默认。`on-demand` 在工具声明体积超过 `budgetBytes` 时改为分页目录 + 按需揭示 |
| `chunkedMap.strategy: off/model-invoked` | `off` 为默认。`model-invoked` 向模型提供分块处理工具。**这不是无损机制**，见下 |
| `admission: off/declared-ceiling` | `off` 为默认。`declared-ceiling` 在发送前按**上游声明**的 `maxInputTokens` 拒绝，返回 **413 / local_token_admission** |
| `recovery: off/lossless-retry` | `off` 为默认。`lossless-retry` 在分类明确的长度拒绝后，对 payload 施加一次无损修正并同模型重发一次 |
| `allowSimulatedCache:false` | 无论旧计量开关如何，客户端路径不采用模拟缓存分摊；原生用量缺失不补出“命中” |
| `kiroOnly:true` | 拒绝配置外部 countTokensApiUrl；推理/搜索仍走现有 Kiro 路径 |

## 按需工具发现与分块处理（阶段 3）

两项性质完全不同，文档分开写，不合并到一个标题下。

### 按需工具发现（可达性无损）

`toolCatalog.strategy: on-demand`：客户端声明的工具 schema 序列化字节数超过 `budgetBytes` 时，不再一次性全发，改为提供两个服务端工具——`kiro_tool_catalog_list` 分页列出名称与描述，`kiro_tool_schema_read` 按名索取完整 schema 并让这些工具在后续轮次真正声明给上游，从而可被调用。

**没有任何工具被移除**，网关也不替模型判断哪些工具重要。目录直接取自客户端自己的声明，名称与描述**原样透传，不摘要、不改写、不按相关性排序**——一个按自己猜测的相关性给工具列表重排序的网关，正是在做它声称不做的那个选择。分页稳定完整：沿 `next_offset` 走到 null 恰好得到全集，每项一次，保持声明顺序。被揭示的工具与客户端声明逐字一致；索取一个不存在的名字是**报错**而不是静默省略（静默省略会让模型以为某个工具不存在，而它其实存在）。

真实代价有两条，照写不打折扣：选工具时看到的是名称与描述而非完整 schema；够到一个工具需要多花轮次。页大小、单次揭示数量和目录轮次都有上限。

### 分块处理（**不是**无损机制）

`chunkedMap.strategy: model-invoked` 默认关闭。开启后也只是把 `kiro_context_map` 工具**提供**给模型，由模型显式调用；网关**不会**背着模型自动套用分块，模型因此不会在不知情的情况下拿到由碎片推出的结论。它只在存在原文会话时出现——没有原文就没有可切的对象。

**把文本切块分别处理，意味着没有任何一轮同时看到超过一块。**任何需要把两块原文放在一起才能得出的结论，在这个机制下都得不出来。这是切块本身的性质，不是实现质量缺陷，再多测试也不会把它变成"与一次性读全文等价"。它**不是"不降智"的实现**，本文档、配置注释、工具描述和返回结果都按这个措辞写。无损的替代路径是原文分页读取（`kiro_context_read`）——读得下就应该用它。

无损的部分说清楚：切分逐字节无损（与工具结果分片共用同一个 UTF-8 安全切分器，区间首尾相接、落在字符边界、按序拼接可还原原文）；每块结果都带**字节区间**，答案的每一部分都能追溯到所依据的原文；**合并发生在模型自己的上下文里**——各块结果一起返回由模型自行关联，所以只有 map 阶段是碎片化的；原文始终留在 artifact 存储里，随时可完整读取。

块数超过 `maxChunks` 直接**报错**，不静默只处理一部分——静默处理一部分会让模型以为它看过全文；某一块失败同样中止并指明区间。每一块都是一次**真实的上游轮次**（同模型、无工具、非流式、输出有上限），用量与普通轮次一样经 tracer 记录。分块处理**不比读原文便宜**，它是在原文放不下时的一种处理方式，代价已经写明。

## 准入、恢复与被动校准（阶段 2）

`admission` 与 `recovery` 都**默认关闭**，因为它们会改变请求的实际走向；观测与校准默认生效，因为它们只让既有数字更诚实。

`admission: declared-ceiling` 在发送前比较**估算输入 token**与上游声明的 `maxInputTokens`，超出即返回 413 / `local_token_admission`（与字节预算的 `local_payload_limit` 用不同错误码，便于分清拒绝来自哪一侧）。三条边界必须清楚：判据是**本地启发式估算**而不是上游自己的计数，因此**可能拒掉上游本来会接受的请求**；上游没有声明上限时**不拦截**——未知不是无限也不是零；写死的模型名窗口表永远不会被当作上限，拿猜测做拦截等于把猜测升级成门禁。准入只读该凭据已缓存的模型列表，不触发刷新，不改模型、不换凭据、不截断。关闭审计不会连带关掉准入。

`recovery: lossless-retry` 只在拒绝分类**明确指向长度预算**时触发（协议配对错误改尺寸不会通过，未分类的拒绝不得被当作长度问题）。它对 payload 施加一次无损修正——目前唯一可用的是工具结果分片——然后同模型重发**一次**：没有第三次、不换补救手段、不换账号碰运气。它**不改变正常请求的稳态形状**，修正只作用于这一次重试，因此接受性未验证的形状只会在上游**已经拒绝**常规形状之后发出。修正后若字节毫无变化则不重发（比较时会先剔除每次转换都重新生成的 `conversationId` / `agentContinuationId`，否则整串比较必然不同，这条约束会形同虚设）。恢复成功一次**不构成**该形状被普遍接受的证据。

开启 `recovery` 的成本要知道：修正体在请求构造阶段就会预先生成，也就是**每个请求都多做一次转换与序列化**，即便它从未被拒绝、修正体从未被使用。大 payload 下这是实打实的 CPU 与内存开销。关闭时（默认）只有一次函数调用即返回，没有额外成本。

被动分母校准不需要任何配置，也不需要探测流量。上游的 `contextUsageEvent` 只给百分比、不说分母，本项目此前用一张写死的模型名窗口表去乘它；但原生 `metadataEvent.tokenUsage` 与该百分比会在**同一次响应**里一起到达，两者一比即可反推上游实际使用的分母。样本只在两半都完整时产生：缺百分比、缺原生字段、用量是估算值、响应中断、输入为 0、百分比 ≤ 0 一律不取样；百分比 ≥ 100 也丢弃——上游确实会给出 100，若那是钳位值，反推出的分母会等于本次输入量本身，从而系统性低估窗口并把这个低估值记成观测。

样本按模型与端点聚合，连同样本数、最小值、最大值与均值一起呈现，并独立于 trace 保留期存活（它描述的是上游算术而不是某一次请求）。`GET /api/admin/context-calibration` 与设置页的只读面板都会标明：这是**在 N 个样本上对上游算术的观察**，不是实测或公布的上游上限。min/max 跨度大时界面**不显示均值**，只显示区间——显示均值会让人误以为拿到了一个窗口值。校准只观察：聚合值只被那个只读接口读取，请求路径里没有任何消费者，它不修改配置，也不会自己流入准入。

`toolResults.strategy: lossless-chunks` 是**接受性未验证**的实验特性，默认关闭。上游 `toolResults[].content` 在 schema 上本就是数组，此前恒为单条目是转换器的选择；但本仓库从未向 Kiro 发送过多于一个条目的载荷，本项目也不允许为探测而发试探流量，因此**无法离线证明上游接受它**。与 `static-prefix` 同为可撤销策略：开启后若出现 400，改回 `join`，不会自动改形或重试去绕过拒绝。开启前可用离线检查肉眼验收线上形状——`--inspect-request` 输出的 `metrics.toolResultEntryCount` 大于 `toolResultCount` 即表示分片已生效，同时 `largestTextBytes` 应降到 `chunkBytes` 以内。

Token 预算报告随请求构造审计一并输出（`tokenMetrics`）：按当前轮 / 历史 / 工具声明 / 工具结果 / 图片 / 其它给出互不重叠的分项，总量等于各项之和；该凭据已缓存的模型列表声明了 `maxInputTokens` 时附上限与余量，余量可为负（已越界）且不夹到 0。未缓存或上游未声明时上限与余量都是 `null`——**未知就是未知**，不按模型名推断，也不从上游拒绝反推；上限查询只读缓存，不会为了填一个报告字段去触发刷新或阻塞真实请求。这些数字全部标注 `source:"estimate"`：它们是本地估算，不是 `metadataEvent.tokenUsage`，不构成缓存或计费证据；**字节预算与 token 预算是两个口径，互不替代**。本阶段只展示不参与准入，按 `maxInputTokens` 做准入属于阶段 2。

输入 token 估算已修正为递归遍历整棵内容树。此前只统计第一层 `text`，`tool_result` 正文、`tool_use.input`、`thinking` 与图片实际按 0 计——一个带大段文件内容的完整 agentic 轮次与仅一句提问估得一样多。修正后这些请求的估算值会上升；上游返回原生用量时仍以原生为准，修正只影响原生用量缺失时的兜底值，已落盘的历史用量日志不会重算。

`agentMode` 的 body 和 IDE header 使用同一配置（vibe/spec）；不要为测缓存而反复切换。`toolCompatibilityMode:raw` 是示例中的独立选择：保留客户端工具定义；继续使用原来的 `claude-code` 模式则仍按既有兼容规则映射内置工具和 schema。

新管线不会因长度错误换模型、缩短思考预算、总结/裁剪历史或盲目换号重试。显式预算不再被反序列化上限或模型名后缀静默覆盖。Kiro 不支持的输入（例如 assistant prefill）直接报错，不静默丢弃。

HTTP 请求头与 prompt 里的“头部文本”必须分开看：本项目由 endpoint 构造出站 HTTP 头，不透传任意客户端头；`amz-sdk-invocation-id` 仍按请求生成，不能为追求缓存而固定。`x-anthropic-billing-header:` 在此指 system 文本里的已知生成行，删除它才会改变模型输入。出站头变化是否参与 Kiro 的服务端缓存键未知，不能仅凭名称或尺寸断言其导致缓存未命中。

网关返回的已完成搜索记录和不透明 `redacted_thinking` 可在后续轮次重放：转为带说明的完整 JSON 历史文本（不重新执行搜索、不解码思考数据），避免丢失来源或拒绝自己的输出。孤立/重复工具结果、未完成配对和畸形内容块明确报错，不通过删除历史“修复”。

## 400 的处理边界

`CONTENT_LENGTH_EXCEEDS_THRESHOLD` 本身没有指明是总 body、单字段、图片还是模型 token 窗口，因此不再统一显示“Context window is full”。保留上游 400，提示检查 wire 证据；不自动重发。

本地预算超限返回 **413 / local_payload_limit**，显示测量值和配置上限；选号前检查不受 endpoint 影响的字段预算，再在 endpoint 注入 profile/转换字段后检查全部预算。总 body 限制只用于最终形状（CLI 会移除部分字段，不能提前误拒绝）。不会截断内容以换取成功。小于本地预算也不保证 Kiro 接受：未知的上游限制仍可能产生 400。

系统指令、schema、当前用户要求、推理和工具入参不会被原文卸载。工具结果和历史文本存为租户+会话隔离的不可变原文，模型可调用 `kiro_context_read` 按 UTF-8 字节区间读取、`kiro_context_search` 做字面搜索；返回分页信息，工具不会读取任意文件或访问外部服务。活动请求持有租约防止中途过期；存储满/ID 过期/轮次超限明确失败。

存储为有界内存，进程重启后清空；原始客户端历史应继续保留。客户端没有会话 UUID 时，网关为本次内部多轮分配 UUID。切片可保留像素、原文工具可保留信息可访问性，但**不能证明与全文一次性输入具有相同推理效果**。需要全文整体推理且超过上游可接受范围时，应停止并调整任务，不承诺“不可能的无限上下文”。

## 离线验证（不需要账号，不发网络请求）

```sh
cargo test --locked --no-default-features
cd admin-ui
bun install --frozen-lockfile
bun run build
cd ..
cargo build --locked --release
./target/release/kiro-rs --config config.pipeline.example.json --check-config
./target/release/kiro-rs --config config.pipeline.example.json --inspect-request tests/fixtures/pipeline-request.json
```

两个诊断命令都在初始化凭证、HTTP provider、后台模型/版本刷新、Redis 之前退出；不生成配置或凭证文件。`--inspect-request` 输出脱敏测量、配置指纹、变换状态和本地预算判定。它应用已知的 endpoint 转换（包括 CLI 移除字段），但仍是 **offline-endpoint-without-credentials / construction-only**，不伪称测过真实 profile/header 注入或缓存。日志写 stderr，stdout 保持单个 JSON 文档。退出码 0 表示配置/本地预算检查通过，2 表示配置、输入或预算检查失败。

自动测试覆盖 A/B 重复构造、C 静态前缀稳定、D invocation header 与 body 分离、E nonce 净化后相同、F 同 scope 换凭据的构造、G scope 变化及三类本地边界；还覆盖租户隔离、UTF-8 完整重建、图片像素重建、内部工具配对/混合工具/轮次上限、部分用量/重复快照和 SQLite 迁移。**这些通过不等于缓存已经命中。**

## 正常业务流量中的真实举证

启用 `traceEnabled:true` 和 `requestPipeline.auditEnabled:true`，展开管理端请求日志的“请求管线证据”，或查询 `GET /api/admin/traces/{trace_id}/pipeline`（需要已有管理端认证）。

- `wire_audit`：最终 endpoint body 的尺寸、cachePoint 数、模式、credential ID、model/agent、profile/scope 指纹、header 名称和字节数估算。审计发生在本地最终预算检查与 HTTP 发送之前，不能单独证明请求已发出；提前拒绝样本另有 `stage` 标注。头部字节来自构造的 HeaderMap，不是抓包后的 HTTP/2 帧或传输层自动补充头。不开启原文抓包；不记录 Authorization 值、profile 原文、提示词或图片数据。
- `native_usage`：实际 provider 调用的最后一份完整 `metadataEvent.tokenUsage`，四项必须存在且为非负整数。重复快照取最后一份，不在单条流内相加；多次内部调用各记一次。
- `nativeCacheReadInputTokens > 0` 才是该样本真实读取证据；`cacheWriteInputTokens > 0` 表示该样本真实写入。字段缺失、无样本、stream 中断未获得完整数据均不得算命中。`allSamplesNative:null` 不推断采样完整性。
- 本地 CacheMeter 的 simulated 值、耗时下降、构造指纹相同都不构成命中证据。汇总估算与逐轮 native 证据分开查看。

指纹使用进程随机密钥的 HMAC-SHA256；只有 `fingerprintEpoch` 相同才能比较。`wireFingerprint` 表示完整 body；`semanticFingerprint` 仅排除 conversationId/agentContinuationId；`staticPrefixFingerprint` 表示标记的静态历史+工具+模型设置；`scopeFingerprint` 是诊断分组，**不是声称已知服务端 cache key**。

把真实工作中自然发生的首次调用记作 A（冷/热未知，不主动清缓存），重复输入记作 B，新增一轮记作 C；记录原生 read/write 和上述指纹。D/E 主要在离线证明构造不变。F/G 仅记录业务自然换号/切换的样本，不强制切账号，不推断同 profile 一定共享缓存。不实施线上阈值二分、循环重放或并发压力测试。出现 400/429/风控提示时停止额外验证，使用已有证据排查。

cachePoint 的 wire 形状参考 Kiro-account-manager 的 translator/types（研究快照 `503d93432a1717020574715bc495441a96cddfda`），但其命中结论不能替代本部署的原生证据。`static-prefix` 为可撤销的实验策略；若正常调用出现不支持字段的 400，配置回 `off`，不要自动重试试探其他形状。

## 回滚

先把 `cacheStrategy` 改为 `off`、`artifacts.enabled` 改为 `false`、图片改为 `preserve`、`toolResults.strategy` 改为 `join` 并重启，可分别回退变换。

注意二进制回退：`requestPipeline` 使用 `deny_unknown_fields`，因此新版本写过（或网页保存过）的配置文件含有旧版本不认识的字段时，旧二进制会**拒绝启动**而不是忽略它们。回退二进制前需同步删除对应配置段。这不是本次新增的行为，历次配置扩展都有同样性质。若需要完全旧转换，设 `mode:off`；该模式也会恢复旧图像/工具描述转换行为，不适合作为保真模式。真实/估算来源校验与敏感日志修正不回退。SQLite 仅增加独立证据表，不改旧 trace 字段；证据随 trace 清理，未发布或自动重启运行中的实例。
