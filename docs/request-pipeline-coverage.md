# 原始三阶段需求覆盖核对

结论：**三个阶段均未全部满足。** 此前实现覆盖了请求管线的一部分，不能用“方案 C 已落地”替代对下面全部需求的验收。网页配置本轮补齐，但不会自动补齐尚未实现的推理/预算算法。

本表根据工作树实际调用路径核对；没有发送 Kiro 测试流量。

| 阶段 | 原要求 | 状态 | 代码事实与缺口 |
| --- | --- | --- | --- |
| 1 | 最终 payload Budget Report | 部分 | `src/kiro/provider.rs` 在 endpoint 转换后审计、检查再发送；`src/pipeline/mod.rs::WireMetrics` 只有 body/text/tool-result/image 字节指标。没有最终输入 token 分项、模型 token 上限或剩余 token 预算。 |
| 1 | 工具结果无损分片 | 未实现内联分片 | `converter.rs::extract_tool_result_content` 将文本数组合并为单串，Kiro ToolResult 仍只有一个 text map。artifact 分页读取不等于把全部无损分片同时内联发送。 |
| 1 | 修正递归 token 统计 | 未实现 | `src/token.rs::count_all_tokens_local` 只统计 content 字符串或第一层 text，漏计嵌套 tool_result.content、tool_use.input、thinking 等，也不统计最终 wire。 |
| 1 | 将上游 400 改为结构化错误类型 | 未实现 | `provider.rs` 将 400 拼接为 anyhow 字符串，`handlers.rs::map_provider_error` 仍用 contains 分类并返回通用 invalid_request_error。本地 LocalPayloadLimit/413 已有类型，但不等于上游 400 已结构化。 |
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

后续开发仍应按用户给定顺序补齐：阶段 1 缺项 → 阶段 2 的被动准入/恢复/校准 → 阶段 3 的同模型分块处理和按需工具目录。任何长度恢复都不能盲目原样重发、自动降级模型或启动线上阈值二分。
