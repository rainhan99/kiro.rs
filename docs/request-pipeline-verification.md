# 方案 C：交付验证记录

本记录区分代码与离线构造验证、真实上游运行证据。**没有发送 Kiro 验证请求，没有探测上限，没有换号实验，也没有把 simulated 数字算作缓存成功。**

## 构建环境

- 平台：`aarch64-apple-darwin`（Apple Silicon macOS）。
- Rust：1.98.1；任务隔离目录 `/private/tmp/kiro-rust.4ZfiZ9`，未修改系统 Rust 或 shell 配置。
- 基线：`22d2c2d0695ba350890072c19990f54782827ae5`；工作分支 `codex/request-pipeline`。
- `Cargo.lock` 和 `admin-ui/bun.lock` 未修改。
- 原运行配置、账号凭据、运行实例未改动，尚未部署。

## 已执行验证

| 验证 | 结果 | 证明范围 |
| --- | --- | --- |
| `cargo test --locked --no-default-features --quiet` | 806 单元 + 3 CLI 测试通过，0 失败；1 个手动浏览器 fixture 默认忽略 | 无默认特性 Rust 回归 |
| `cargo test --locked --quiet` | 806 单元 + 3 CLI 测试通过，0 失败；1 个手动浏览器 fixture 默认忽略 | 默认特性 Rust 回归 |
| 修改文件的 `rustfmt --check` | 通过 | Rust 格式 |
| `git diff --check` | 通过 | 差异空白检查 |
| `bun test src/components/settings/request-pipeline-form.test.js` | 6 项通过、23 条断言 | 表单数值、跨字段约束、安全开关、草稿/版本保留 |
| `bun test`（admin-ui，积分单位修正后） | 12 项通过、39 条断言 | 上述表单测试及 6 项凭据积分显示回归 |
| `bun run build`（admin-ui） | TypeScript + Vite 生产构建通过 | 包含新增请求管线设置；不等于浏览器视觉验收 |
| `cargo build --locked --release` | 默认特性优化构建通过 | 包含最新管理端静态资源的本机二进制 |
| release `--check-config` + `--inspect-request` | JSON 完整解析且断言通过 | 配置有效、本地预算通过、零网络请求、无 native 缓存证据 |

产物：`target/release/kiro-rs`，Mach-O arm64（不能直接作为 Linux 服务端程序运行）。SHA256：

```text
ac0069528c7b45c6ab8a333cc53dc46dad79a13cfffa898295107c17073b12f1
```

release 离线 fixture 实际输出：body 1080 字节、cachePoint 1 个、`networkRequests:0`、`localBudgetAccepted:true`、`nativeCacheEvidence:null`、`evidenceType:construction-only`。这些数值只对应仓库中的小型合成示例，不是用户故障请求或上游接受上限。

## 关键断言

- 凭据额度与用量使用 Kiro 积分，不显示美元：卡片、紧凑列表、余额弹窗共用积分格式化；原数值、两位小数、负余额与比例计算保留。6 项真实组件静态渲染测试先复现旧 `$` 输出失败，再通过；没有访问真实账号或上游服务，不代表浏览器视觉验收。
- 网页配置的真实 loopback HTTP + 临时文件测试验证管理员认证、保存、启动/保存值分离、旧版本与并发覆盖拒绝、无效值拒绝、其他字段/密钥保留、I/O 失败、符号链接和权限保留。原先 6 项因缺少保存接口/字段而失败，接线后通过；认证基线原本即通过。
- 目录同步失败单独进行故障注入：真实写入/替换文件，仅替换同步操作。断言返回 `persistence_uncertain` 且磁盘可能已经更新；实际观察 red → green。Unix 成功路径执行文件及父目录同步；没有断电实验，也不声称非 Unix 同等持久性。

- A/B/C/D/E/F/G **仅在离线 fixture 中验证构造性质**：同 body、静态前缀、变化的 HTTP invocation ID、billing nonce、凭据 ID 与 profile/model/Agent Mode。服务端缓存隔离键仍未知。
- 本地 text/tool-result/base64 image 边界分别测试；没有拿这些值冒充上游阈值。
- CLI 在凭据和网络初始化之前退出：有效输入为单个 JSON，畸形配置/超限返回退出码 2；没有原文回显或凭据文件生成。
- 不裁剪历史、思考预算或长工具说明，不自动降级模型。未知/不可表达的输入明确失败。
- 原文 read/search 可完整重建 UTF-8 内容，租户/会话隔离，容量/过期/轮次受限；无损切片按像素重建测试通过。
- 私有工具不泄漏给客户端执行；客户端工具参数与顺序保留；畸形/未完成工具 JSON 的回归实际经历 red → green。
- 真实网关搜索输出（包括无 tool_use_id 的旧 Contract A）能够再次经过 prepare，保留完整 JSON。模糊配对明确报错。
- native 用量必须有完整四项非负整数；重复事件取最后快照，每次实际模型调用单独记录。缺失不能填零后宣称命中。
- 指纹使用进程级随机密钥 HMAC；审计不写提示词、图片、凭据值或 HTTP 头值。

## 尚待正常业务流量确认

本地测试不能证明 Kiro 接受实验性 `cachePoint`，不能证明缓存命中，也不能证明按需原文读取与全文一次输入具有相同推理效果。真实验收应按 [操作文档](request-pipeline.md) 观察自然业务样本的 `cacheReadInputTokens` / `cacheWriteInputTokens`；没有完整原生证据时保持“未验证”。

原文存储为进程内有界内存，重启后清空；客户端应保留原始对话。配置为启动时加载，修改后重启生效。

## 本轮浏览器验收边界与阶段差距

已启动仅含临时配置、无账号、无 inference provider 的本机管理端 fixture，浏览器访问被应用内浏览器以 `ERR_BLOCKED_BY_CLIENT` 拦截。没有绕过限制，**没有宣称浏览器交互、视觉或窄屏验收通过**；临时服务已停止、临时配置已清理。生产实例未启动或重启。

三阶段完整要求仍有缺项，见 [逐项覆盖核对](request-pipeline-coverage.md)。本轮补齐网页配置，不代表递归 token 统计、结构化上游 400、修改后重试、模型阈值校准、增量记忆或 Map-Reduce 已实现。

## 2026-09-23：Portable cross-provider history 最终验证

验证代码提交：`c74e50365d5bde110819aefce60ea3e797c0059e`（`feat/portable-history`）。以下是本次在 Apple Silicon macOS 上重新执行的结果；未使用账号、凭据、真实请求或生产数据。

| 命令 | 退出状态与计数 | 说明 |
| --- | --- | --- |
| `cargo fmt --all` | 0 | 未产生格式化差异。 |
| `cargo fmt --all --check` | 0 | Rust 格式检查通过。 |
| `git diff --check` | 0 | 工作树差异无空白错误；运行后 `git diff --stat` 与 `git status --short` 均为空。 |
| `cargo test --workspace --locked`（未提权） | 101；1,190 通过、76 失败、1 忽略 | macOS 受限执行环境拒绝 loopback/系统配置访问，报 `SystemConfiguration` NULL object 与 `Operation not permitted`；不是产品断言失败。 |
| 同一 `cargo test --workspace --locked`（提权的权威重跑） | 0；库 1,266 通过、0 失败、1 忽略；CLI 7 通过；desktop 库 16 通过；`kiro_rs` doctest 3 通过、3 忽略；其余两个测试目标 0 测试 | 完整 Rust workspace 通过。忽略项包含既有手工浏览器 fixture 和既有 doctest。 |
| `cargo test --locked -p kiro-rs --no-default-features`（未提权） | 101；1,190 通过、76 失败、1 忽略 | 同一 macOS loopback/系统配置沙箱限制。 |
| 同一无默认特性命令（提权的权威重跑） | 0；库 1,266 通过、0 失败、1 忽略；CLI 7 通过；doctest 3 通过、3 忽略 | 不依赖默认 native-tls 特性。 |
| `cd admin-ui && bun test` | 0；108 通过、0 失败、268 条 `expect()`，15 个文件 | 前端完整测试通过。 |
| `cd admin-ui && bun run build` | 0；TypeScript 与 Vite 生产构建完成 | 仅见既有 Node `module.register()` 弃用警告；Vite 转换 2,612 个模块。 |
| `cd admin-ui && bun test src/components/settings/request-pipeline-form.test.js src/components/trace-normalization.test.js` | 0；16 通过、0 失败、70 条 `expect()` | 配置表单与仅本地、内容安全的审计 UI 覆盖。 |
| `cargo test --test pipeline_cli cc_switch_history_is_inspected_without_network_or_opaque_leakage`（未提权） | 101；0 通过、1 失败、6 过滤 | 测试的 macOS `sandbox-exec` 网络拒绝 harness 在受限执行环境中不能应用 sandbox profile。 |
| 同一 CLI 隐私命令（提权的权威重跑） | 0；1 通过、0 失败、6 过滤 | 网络拒绝 harness 下的离线 CLI 断言通过。 |
| `cargo test -p kiro-rs cc_switch_history_reaches_fake_kiro_without_private_fields --lib` | 0；1 通过、0 失败、1,266 过滤 | fake-upstream 最终 wire 回归通过。 |
| `jq empty config.pipeline.example.json tests/fixtures/portable-history-cc-switch.json` | 0 | 示例配置和已净化 fixture 都是有效 JSON。 |
| `cargo run --locked -- --config config.pipeline.example.json --check-config` | 0；`configurationValid: true`，`networkRequests: 0` | 只做本地配置构造。 |
| `cargo run --locked -- --config config.pipeline.example.json --inspect-request tests/fixtures/portable-history-cc-switch.json` | 0；`configurationValid: true`，`networkRequests: 0`，`normalization.transformedBlocks: 7` | 输出未含 fixture 的私有哨兵或私有内容；仅记录聚合归一化结果。 |

两个 focused privacy 回归共同覆盖离线边界：CLI 在网络拒绝 harness 中构造请求，final-wire 使用内存 fake upstream；它们断言私有字段、原始附件/结构和本地归一化元数据不会出现在可发送 body 中，同时保留公开的工具配对和可读历史。该证据是**离线构造与序列化证据，不是 Kiro 实时接受、缓存或推理等价性的证据**。

分支卫生 RED：`git diff master...HEAD --check` 返回 2，唯一输出是 `docs/superpowers/specs/2026-09-22-portable-history-design.md:272: new blank line at EOF.`；`git blame` 将该行归于本分支已有的设计提交 `1812672`。经明确授权，Task 7 仅删除该文件 EOF 的额外空行（无产品代码变更）。GREEN：包含这两个文档变更的暂存内容执行 `git diff --cached --merge-base master --check` 返回 0；提交后的同一 `master...HEAD` 检查见本任务报告。未推送或合并任何分支。

## 2026-09-23：最终审查修复波次验证

本波次以 `daa0ac230a6188e88c12a4fcce7dc763392f5d1f` 为审查基线，一次性修复 4 个 Important 和 3 个 Minor 项。空尾消息现在会在所有策略中移除且不计作内容丢失，最终 wire 使用真正的 current frontier；直接 converter 只有显式 `drop` 可以丢弃非空 assistant prefill。已提供的工具参数、错误状态和搜索字段在归一化时校验，畸形请求保持原子性并返回安全 HTTP 400。

已完成搜索记录在三个策略、三个 mode 中保留安全的公开投影。公开搜索错误使用经过白名单校验的 `error_code`，与空成功结果区分；接受的六个错误代码依据 [Anthropic web-search 文档](https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool)。Drop prefill 按原块分类和原字节数仅报告一次；`opaqueBytes` 同时计算 thinking signature、redacted data 和 encrypted search content 的 UTF-8 字节数。每次成功且发生转换的 preparation 都输出仅包含策略和聚合计数的本地 WARN，`auditEnabled: false` 时也适用。

| 验证命令或范围 | 本波次结果 |
| --- | --- |
| `cargo test -p kiro-rs --lib final_review -- --nocapture`（实现前） | RED，退出 101：11 项失败，分别命中尾部/frontier、直接 prefill、畸形输入/原子性/400、公开搜索错误、legacy 搜索回放、Drop 计数、WARN 缺失和 opaqueBytes 漏计。 |
| `cargo test -p kiro-rs --lib final_review_known_malformed_shapes_fail_atomically_in_every_strategy`（实现前） | RED，退出 101：27 种畸形输入在三个策略中全部被接受或错误分类。 |
| `cargo test -p kiro-rs --lib final_review`（修复后） | GREEN，退出 0：11 通过；其中覆盖 27 种畸形输入 × 3 策略 × 3 mode、36 个空尾最终 wire 场景、9 个 legacy 搜索回放场景以及精确 21 字节混合 opaque 计数。 |
| `cargo test -p kiro-rs --lib pipeline::portable_history::tests` | 退出 0：26 通过。 |
| `cargo test -p kiro-rs --lib anthropic::converter::tests` | 退出 0：104 通过。 |
| `cargo test -p kiro-rs --lib pipeline::tests` | 最终退出 0：43 通过。日志捕获最初并行运行时受进程级 tracing callsite 注册影响；单独和串行诊断通过，随后将真实 preparation/WARN 捕获测试隔离到子进程，完整并行组通过。 |
| `cargo test -p kiro-rs --lib anthropic::handlers::tests` | 受限执行 40 通过、1 项因 SystemConfiguration NULL object 失败；提权权威重跑退出 0，41 通过。 |
| `cargo test -p kiro-rs --lib anthropic::websearch_loop::tests` | 退出 0：77 通过。 |
| `cargo test -p kiro-rs --lib cc_switch_history_reaches_fake_kiro_without_private_fields` | 退出 0：1 通过。 |
| `cargo test --test pipeline_cli cc_switch_history_is_inspected_without_network_or_opaque_leakage` | 受限执行因 `sandbox-exec: sandbox_apply: Operation not permitted` 失败；提权权威重跑退出 0，1 通过，网络拒绝 harness 生效。 |
| `cargo test --workspace --locked` | 受限执行退出 101：1,201 通过、76 个既有 macOS SystemConfiguration/loopback 权限失败、1 忽略；提权同命令退出 0：库 1,277 通过、1 忽略，CLI 7 通过，desktop 库 16 通过，doctest 3 通过、3 忽略，其余零测试目标通过。 |
| `cargo test --locked -p kiro-rs --no-default-features` | 受限执行退出 101：1,201 通过、同类环境失败 76、1 忽略；提权同命令退出 0：库 1,277 通过、1 忽略，CLI 7 通过，doctest 3 通过、3 忽略。 |
| `cd admin-ui && bun test src/components/settings/request-pipeline-form.test.js src/components/trace-normalization.test.js` | 退出 0：16 通过、70 条断言。 |
| `cd admin-ui && bun test` | 退出 0：108 通过、268 条断言、15 个文件。 |
| `cd admin-ui && bun run build` | 退出 0：TypeScript/Vite 生产构建通过，2,612 个模块；仅有既有 Node `module.register()` 弃用警告。 |
| `cargo fmt --all --check`、`git diff --check`、`git diff master...HEAD --check` | 均退出 0。 |
| `jq empty config.pipeline.example.json tests/fixtures/portable-history-cc-switch.json` | 退出 0。 |
| `cargo run --locked -- --config config.pipeline.example.json --check-config` | 退出 0：`configurationValid: true`、`networkRequests: 0`。 |
| `cargo run --locked -- --config config.pipeline.example.json --inspect-request tests/fixtures/portable-history-cc-switch.json` | 退出 0：`networkRequests: 0`、9 块扫描、7 块转换、`opaqueBytes: 86`；stderr 的新增 WARN 仅含策略和上述聚合计数。 |

离线 CLI 的 stdout/stderr 已检查，不含 fixture 的签名、加密/脱敏值、可读内容哨兵、附件 base64、文件名或搜索 URL。所有权威 Rust 运行均无产品测试失败；现有手工浏览器测试和 doctest 忽略项保持不变。本波次证据仍是**离线构造、序列化和回归验证，不是实时 Kiro 接受或跨模型推理等价性证明**。没有调用真实 Kiro、探测能力、重试账号、部署、推送或合并。
