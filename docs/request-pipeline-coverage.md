# 原始三阶段需求覆盖核对

结论：**阶段 1 已全部实现（2026-09-17），阶段 2、3 仍未全部满足。** 阶段 1 的四项按「量具 → 账单 → 分类 → 补救」一条链交付，spec 见 `docs/superpowers/specs/2026-09-17-request-pipeline-phase1.md`。阶段 2、3 的推理/预算算法没有因此自动补齐。

本表根据工作树实际调用路径核对；没有发送 Kiro 测试流量。

| 阶段 | 原要求 | 状态 | 代码事实与缺口 |
| --- | --- | --- | --- |
| 1 | 最终 payload Budget Report | 已实现（只报不拦） | `pipeline::WireTokenMetrics` 给出最终线上 token 分项（当前轮/历史/工具声明/工具结果/图片/其它），分项互不重叠且总量等于各项之和；已知时附模型 `maxInputTokens` 与余量（余量可为负，不夹 0），未缓存时如实为 null，绝不猜测或从拒绝反推。与字节口径并列展示，`source:"estimate"` 标注。**本阶段只展示，不参与准入**——准入是阶段 2。 |
| 1 | 工具结果无损分片 | 已实现（默认关，接受性未验证） | `toolResults.strategy=lossless-chunks` 时超过 `chunkBytes` 的正文切成多个 text 条目，逐字节可还原、切点落在 UTF-8 边界、配对不变。上游 `content` 本就是数组，此前单条目是转换器选择而非 schema 限制。**上游是否接受多条目未经验证**（本仓库从未发过，且不允许探测流量），故默认 `join`，与 static-prefix 同为可撤销实验特性。`--inspect-request` 的 `toolResultEntryCount` 可在发送前肉眼验收形状。 |
| 1 | 修正递归 token 统计 | 已实现 | 实测旧实现不是「漏计」而是**零计**：`tool_result` 正文、`tool_use.input`、`thinking` 各自填充与空基线都得 1（全靠 `.max(1)` 兜底），图片得 1（应 1600），一个带大段文件内容的完整 agentic 轮次与仅一句提问同为 10。现按整棵内容树递归、深度有界，图片复用 `image_resize::estimate_image_tokens` 不另造公式；document 块在 enforce 下由 pipeline 直接拒绝，故不为其臆造公式。最终线上另有 `measure_wire_tokens`。估算上升会连带影响「无原生用量」请求的用量与积分显示，已获用户明示批准，不保留旧口径开关。 |
| 1 | 将上游 400 改为结构化错误类型 | 已实现 | `kiro::error::UpstreamRequestError` 携带状态码、解析出的 reason/message、保留的原始报文与一次性完成的分类；主 API 与 MCP 两条路径共用同一构造器，`Display` 各自保留原前缀故日志与 trace 片段逐字不变。顺带修掉一个具体缺陷：旧实现在**拼接后的字符串**上做分类，该串不是合法 JSON，导致 endpoint 层的字段确认必然解析失败并退化为裸子串匹配——报文里任何位置偶然出现关键词都会被误判。策略未变：`CONTENT_LENGTH_EXCEEDS_THRESHOLD` 仍不归因于 body/字段/图片/上下文窗口中的任何一个。 |
| 2 | 接入 maxInputTokens 准入 | 未实现 | `available_models.rs::TokenLimits` 有字段且模型列表会展示/合并，但没有接到请求准入或 Budget Report。 |
| 2 | 解析 ContextUsage breakdown | 未实现 | `events/context_usage.rs` 只有 contextUsagePercentage，消费处由百分比估总数；没有 breakdown 数据模型和展示。 |
| 2 | 一次修改后重试 | 未实现 | 上游 400 直接结束；没有“分类明确 → 无损修改实际 payload → 同模型仅再发一次”的恢复状态机。已有空回答重试不算长度纠错重试。 |
| 2 | 按模型/端点阈值校准 | 未实现 | 有 endpoint/model/scope 审计，限制仍是单份静态配置；没有自然成功/失败样本聚合、边界置信度、持久化或回流准入。拒绝线上探测不等于已经做了被动校准。 |
| 3 | artifact store | 已实现（有边界） | `pipeline/artifacts.rs` 为租户/会话隔离、有界内存、TTL、活动租约的不可变原文存储；默认关闭，重启清空。不是持久化增量记忆。 |
| 3 | kiro_context_read 内部工具循环 | 已实现（有边界） | handlers 路由到 context loop，网关执行 read/search，成对追加结果后继续请求 Kiro；保留 UTF-8 原文分页，轮数有上限。模型是否读取全部内容不保证。 |
| 3 | Kiro 同模型增量记忆和 Map-Reduce | 未实现 | 当前循环沿用模型，但没有增量记忆状态、分块 map、reduce 调度、输出溯源或完整性验收。不能把同模型内部工具循环当作此项完成。 |
| 3 | Kiro-only 工具目录分页 | 未实现（按需项） | converter 仍发送整个工具列表；无工具目录发现、schema 分页和按需选择机制。 |
| 管理 | 网页配置现有能力 | 本轮已接线 | 表单、读取/保存 API、双端校验、版本冲突、原子保存、已保存/生效对照；重启生效。浏览器视觉验收被本机浏览器拦截，未宣称通过。 |

## 不应混为一谈的验收条件

- 原文可恢复 ≠ 模型已读取全部原文 ≠ 与全量内联具有相同推理效果。
- 字节 Budget Report ≠ token Budget Report；`maxInputTokens` 解析存在 ≠ 已参与 admission。
- 静态 cachePoint 或指纹一致 ≠ 真实命中；只有实际完整上游 native usage 支持 cache read/write 结论。
- Web 配置已保存 ≠ 当前进程已生效；本轮不做服务重启或真实配置变更。
- 语义摘要/增量记忆/Map-Reduce 可能丢细节；即便后续实现，也不能未经任务级质量验证就叫“不降智”。

后续开发按用户给定顺序继续：阶段 2 的被动准入/恢复/校准 → 阶段 3 的同模型分块处理和按需工具目录。任何长度恢复都不能盲目原样重发、自动降级模型或启动线上阈值二分。

阶段 1 交付后新增的已知边界：`lossless-chunks` 的上游接受性只能由真实业务流量确认，离线通过不等于上游接受；token 分项是估算，与原生 `metadataEvent.tokenUsage` 不可混用；`maxInputTokens` 只读该凭据已缓存的模型列表，不触发刷新，因此冷启动期间会显示未知。
