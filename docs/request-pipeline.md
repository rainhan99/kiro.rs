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
| `allowSimulatedCache:false` | 无论旧计量开关如何，客户端路径不采用模拟缓存分摊；原生用量缺失不补出“命中” |
| `kiroOnly:true` | 拒绝配置外部 countTokensApiUrl；推理/搜索仍走现有 Kiro 路径 |

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

先把 `cacheStrategy` 改为 `off`、`artifacts.enabled` 改为 `false`、图片改为 `preserve` 并重启，可分别回退变换。若需要完全旧转换，设 `mode:off`；该模式也会恢复旧图像/工具描述转换行为，不适合作为保真模式。真实/估算来源校验与敏感日志修正不回退。SQLite 仅增加独立证据表，不改旧 trace 字段；证据随 trace 清理，未发布或自动重启运行中的实例。
