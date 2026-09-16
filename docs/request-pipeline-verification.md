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
