/** 多上游网关的配置与账目视图。
 *
 * 金额一律是十进制字符串，前后端都不经过浮点——`0.123456789012345678` 这样的
 * 数字 double 存不下，一次往返就会悄悄变值。
 */

export type RoutingMode = 'sticky' | 'weighted_random'
export type UpstreamKind = 'kiro' | 'anthropic' | 'openai_chat' | 'openai_responses'
export type BillingUnit = 'kiroCredit' | 'CNY' | 'USD'
export type BudgetEnforcement = 'soft' | 'hard'

export interface TokenPrices {
  currency: BillingUnit
  /** 每百万 token 的价格，十进制字符串。 */
  input: string
  output: string
  cacheRead: string
  cacheWrite: string
  cacheWrite1h?: string | null
}

export interface Upstream {
  id: string
  name: string
  kind: UpstreamKind
  enabled: boolean
  weight: number
  baseUrl?: string | null
  /** 读取时不会出现；写入时省略表示沿用原值。 */
  apiKey?: string | null
  /** 只读：是否已配置密钥。 */
  hasApiKey?: boolean
  allowPrivateNetwork?: boolean
  kiroGroup?: string | null
}

export interface ModelBinding {
  id: string
  upstreamId: string
  /** 上游真实模型名，替换对外别名。 */
  upstreamModel: string
  enabled: boolean
  priorityTier: number
  weight: number
  contextWindow: number
  maxOutputTokens: number
  supportsTools: boolean
  supportsImages: boolean
  supportsReasoning: boolean
  allowModelSubstitution?: boolean
  billingUnit: BillingUnit
  /** 成本价（付给上游）。 */
  costPrices?: TokenPrices | null
  /** 售价（向客户收取）。按钱计费必须有。 */
  sellPrices?: TokenPrices | null
}

export interface PublicModel {
  id: string
  displayName?: string | null
  routingMode?: RoutingMode | null
  affinityTtlSecs?: number | null
  bindings: ModelBinding[]
}

export interface GatewayConfig {
  defaultRoutingMode: RoutingMode
  affinityTtlSecs: number
  maxAttempts: number
  requestTimeoutSecs: number
  upstreams: Upstream[]
  models: PublicModel[]
}

export interface GatewayConfigView {
  revision: number
  config: GatewayConfig
  /** 当前真正被接管的别名；定义了却全部停用的不在其中。 */
  managedModels: string[]
}

export interface SaveGatewayConfig {
  revision: number
  config: GatewayConfig
}

/** 后端的结构化错误码。每一种对应一个明确的补救动作。 */
export type GatewayErrorCode =
  | 'gateway_not_configured'
  | 'invalid_configuration'
  | 'configuration_conflict'
  | 'quota_exceeded'
  | 'persistence_error'

export interface Budget {
  unit: BillingUnit
  enforcement: BudgetEnforcement
  /** `null` 表示不限额，与 `"0"`（一分都不能花）是两回事。 */
  limit: string | null
  used: string
  reserved: string
  available: string | null
  cycle: number
  inFlight: number
  pending: number
  customerPending: number
  maxInFlight: number
  maxPending: number
  allowedModels: string[]
  allowedUpstreams: string[]
}

export interface BudgetsView {
  keyId: number
  budgets: Budget[]
}

export interface BudgetUpdate {
  unit: BillingUnit
  limit: string | null
  enforcement: BudgetEnforcement
  maxInFlight: number
  maxPending: number
  allowedModels?: string[]
  allowedUpstreams?: string[]
}

export interface PreviewRequest {
  keyId: number
  model: string
  sessionId?: string
  needsTools?: boolean
  needsImages?: boolean
  needsReasoning?: boolean
}

export interface PreviewCandidate {
  bindingId: string
  upstreamId: string
  upstreamModel: string
  unit: string
  eligible: boolean
  /** 被拒的原因；通过时为 null。 */
  refusal: string | null
}

export interface PreviewView {
  managed: boolean
  mode: string | null
  candidates: PreviewCandidate[]
  /** 按当前配置会被选中的绑定；全部被拒时为 null。 */
  selected: string | null
}
