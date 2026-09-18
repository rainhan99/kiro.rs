/** 网关配置表单的纯逻辑。
 *
 * # 校验在这里是为了「当场说清哪里不对」，服务端才是契约
 *
 * 后端有同一套校验，且以它为准。这里重复一遍只为把错误指到具体那一格上，
 * 而不是保存后甩一句笼统的失败。两边若有分歧，以服务端为准——所以这里只做
 * 「服务端一定也会拒绝」的判断，绝不放宽。
 *
 * # 金额与数值一律以字符串留在草稿里
 *
 * 一是编辑中的半截输入（`2.`、``）必须原样保留，解析成数字再显示会把光标下的
 * 内容吞掉；二是金额本来就不能过浮点：`0.123456789012345678` 走一趟 double
 * 就变值了。
 */

import type {
  BillingUnit,
  GatewayConfig,
  GatewayConfigView,
  ModelBinding,
  RoutingMode,
  TokenPrices,
  Upstream,
  UpstreamKind,
} from '../../types/gateway'

export interface PricesDraft {
  currency: BillingUnit
  input: string
  output: string
  cacheRead: string
  cacheWrite: string
  cacheWrite1h: string
}

export interface UpstreamDraft {
  id: string
  name: string
  kind: UpstreamKind
  enabled: boolean
  weight: string
  baseUrl: string
  /** 只在用户主动填写时才有值；未填表示沿用服务端已存的密钥。 */
  apiKey?: string
  /** 只读：服务端是否已存有密钥。 */
  hasApiKey: boolean
  /** 显式清除已存的密钥。 */
  clearApiKey?: boolean
  allowPrivateNetwork: boolean
  kiroGroup: string
}

export interface BindingDraft {
  id: string
  upstreamId: string
  upstreamModel: string
  enabled: boolean
  priorityTier: string
  weight: string
  contextWindow: string
  maxOutputTokens: string
  supportsTools: boolean
  supportsImages: boolean
  supportsReasoning: boolean
  allowModelSubstitution: boolean
  billingUnit: BillingUnit
  costPrices: PricesDraft | null
  sellPrices: PricesDraft | null
}

export interface ModelDraft {
  id: string
  displayName: string
  routingMode: RoutingMode | ''
  affinityTtlSecs: string
  bindings: BindingDraft[]
}

export interface GatewayDraft {
  defaultRoutingMode: RoutingMode
  affinityTtlSecs: string
  maxAttempts: string
  requestTimeoutSecs: string
  upstreams: UpstreamDraft[]
  models: ModelDraft[]
}

export interface GatewayEditor {
  draft: GatewayDraft
  revision: number
  /** 服务端版本已经变了，而本地还有未保存的改动。 */
  stale: boolean
  managedModels: string[]
}

const emptyPrices = (currency: BillingUnit): PricesDraft => ({
  currency,
  input: '0',
  output: '0',
  cacheRead: '0',
  cacheWrite: '0',
  cacheWrite1h: '',
})

export function emptyUpstream(id: string): UpstreamDraft {
  return {
    id,
    name: id,
    kind: 'anthropic',
    enabled: true,
    weight: '10',
    baseUrl: 'https://api.example.com',
    hasApiKey: false,
    allowPrivateNetwork: false,
    kiroGroup: '',
  }
}

export function emptyBinding(id: string, upstreamId: string): BindingDraft {
  return {
    id,
    upstreamId,
    upstreamModel: 'model-name',
    enabled: true,
    priorityTier: '0',
    weight: '10',
    contextWindow: '200000',
    maxOutputTokens: '8000',
    supportsTools: true,
    supportsImages: false,
    supportsReasoning: false,
    allowModelSubstitution: false,
    billingUnit: 'CNY',
    costPrices: emptyPrices('CNY'),
    sellPrices: emptyPrices('CNY'),
  }
}

function pricesToDraft(prices: TokenPrices | null | undefined): PricesDraft | null {
  if (!prices) return null
  return {
    currency: prices.currency,
    input: prices.input,
    output: prices.output,
    cacheRead: prices.cacheRead,
    cacheWrite: prices.cacheWrite,
    cacheWrite1h: prices.cacheWrite1h ?? '',
  }
}

export function toDraft(config: GatewayConfig): GatewayDraft {
  return {
    defaultRoutingMode: config.defaultRoutingMode,
    affinityTtlSecs: String(config.affinityTtlSecs),
    maxAttempts: String(config.maxAttempts),
    requestTimeoutSecs: String(config.requestTimeoutSecs),
    upstreams: config.upstreams.map((u) => ({
      id: u.id,
      name: u.name,
      kind: u.kind,
      enabled: u.enabled,
      weight: String(u.weight),
      baseUrl: u.baseUrl ?? '',
      hasApiKey: u.hasApiKey ?? false,
      allowPrivateNetwork: u.allowPrivateNetwork ?? false,
      kiroGroup: u.kiroGroup ?? '',
    })),
    models: config.models.map((m) => ({
      id: m.id,
      displayName: m.displayName ?? '',
      routingMode: m.routingMode ?? '',
      affinityTtlSecs: m.affinityTtlSecs == null ? '' : String(m.affinityTtlSecs),
      bindings: m.bindings.map((b) => ({
        id: b.id,
        upstreamId: b.upstreamId,
        upstreamModel: b.upstreamModel,
        enabled: b.enabled,
        priorityTier: String(b.priorityTier),
        weight: String(b.weight),
        contextWindow: String(b.contextWindow),
        maxOutputTokens: String(b.maxOutputTokens),
        supportsTools: b.supportsTools,
        supportsImages: b.supportsImages,
        supportsReasoning: b.supportsReasoning,
        allowModelSubstitution: b.allowModelSubstitution ?? false,
        billingUnit: b.billingUnit,
        costPrices: pricesToDraft(b.costPrices),
        sellPrices: pricesToDraft(b.sellPrices),
      })),
    })),
  }
}

export function createEditor(view: GatewayConfigView): GatewayEditor {
  return {
    draft: toDraft(view.config),
    revision: view.revision,
    stale: false,
    managedModels: view.managedModels ?? [],
  }
}

/** 接收一次后台刷新。
 *
 * 默认**保留**正在编辑的草稿，只把版本号与"服务端已变化"的事实带过来——
 * 一次轮询把人打了一半的表单冲掉，比晚一点发现版本冲突糟得多。`replace`
 * 是用户明确选择丢弃本地改动时才用的。
 */
export function receiveEditor(
  editor: GatewayEditor | null,
  view: GatewayConfigView,
  replace = false,
): GatewayEditor {
  if (!editor || replace) return createEditor(view)
  return {
    draft: editor.draft,
    revision: view.revision,
    stale: view.revision !== editor.revision,
    managedModels: view.managedModels ?? [],
  }
}

/** 十进制金额。与账本的 `Amount` 同一套规则：无符号、无科学计数法、最多 18 位小数。 */
const DECIMAL = /^\d+(\.\d{1,18})?$/

function decimal(value: string, label: string, errors: Record<string, string>, key: string) {
  if (!DECIMAL.test(value)) {
    errors[key] = `${label}必须是十进制数字（不接受科学计数法、负号，小数最多 18 位）`
    return false
  }
  return true
}

function integer(
  value: string,
  label: string,
  errors: Record<string, string>,
  key: string,
  min = 0,
  max = Number.MAX_SAFE_INTEGER,
): number | null {
  if (!/^\d+$/.test(value)) {
    errors[key] = `${label}必须是整数`
    return null
  }
  const parsed = Number(value)
  if (parsed < min || parsed > max) {
    errors[key] = `${label}必须在 ${min} 到 ${max} 之间`
    return null
  }
  return parsed
}

function validatePrices(
  prices: PricesDraft,
  unit: BillingUnit,
  path: string,
  errors: Record<string, string>,
): TokenPrices | null {
  let ok = true
  if (prices.currency !== unit) {
    errors[`${path}.currency`] = `价格币种（${prices.currency}）与该绑定的计费单位（${unit}）不一致`
    ok = false
  }
  for (const field of ['input', 'output', 'cacheRead', 'cacheWrite'] as const) {
    if (!decimal(prices[field], field, errors, `${path}.${field}`)) ok = false
  }
  if (prices.cacheWrite1h !== '' && !decimal(prices.cacheWrite1h, 'cacheWrite1h', errors, `${path}.cacheWrite1h`)) {
    ok = false
  }
  if (!ok) return null
  return {
    currency: prices.currency,
    input: prices.input,
    output: prices.output,
    cacheRead: prices.cacheRead,
    cacheWrite: prices.cacheWrite,
    cacheWrite1h: prices.cacheWrite1h === '' ? null : prices.cacheWrite1h,
  }
}

export function validateDraft(draft: GatewayDraft): { config?: GatewayConfig; errors: Record<string, string> } {
  const errors: Record<string, string> = {}

  const affinityTtlSecs = integer(draft.affinityTtlSecs, '会话保持时长', errors, 'affinityTtlSecs', 1, 86400)
  const maxAttempts = integer(draft.maxAttempts, '最大尝试次数', errors, 'maxAttempts', 1, 10)
  const requestTimeoutSecs = integer(draft.requestTimeoutSecs, '请求超时', errors, 'requestTimeoutSecs', 1, 3600)

  const seenUpstreams = new Set<string>()
  const upstreams: Upstream[] = []
  draft.upstreams.forEach((u, index) => {
    const path = `upstreams.${index}`
    if (!u.id.trim()) errors[`${path}.id`] = '上游 id 不能为空'
    else if (seenUpstreams.has(u.id)) errors[`${path}.id`] = `上游 id "${u.id}" 重复`
    seenUpstreams.add(u.id)

    const weight = integer(u.weight, '权重', errors, `${path}.weight`, 0, 1_000_000)

    // Kiro 走既有凭据池：地址与凭据都不在这份配置里。
    if (u.kind === 'kiro') {
      if (u.baseUrl.trim()) errors[`${path}.baseUrl`] = 'Kiro 上游使用既有凭据池，不能覆盖 baseUrl'
      if (u.apiKey || u.hasApiKey) errors[`${path}.apiKey`] = 'Kiro 上游使用既有凭据池，不能单独配置密钥'
    } else {
      if (!u.baseUrl.trim()) errors[`${path}.baseUrl`] = '直连上游必须填写 baseUrl'
      if (u.kiroGroup.trim()) errors[`${path}.kiroGroup`] = '只有 Kiro 上游能指定凭据分组'
    }

    const upstream: Upstream = {
      id: u.id,
      name: u.name || u.id,
      kind: u.kind,
      enabled: u.enabled,
      weight: weight ?? 0,
      allowPrivateNetwork: u.allowPrivateNetwork,
    }
    if (u.kind !== 'kiro') upstream.baseUrl = u.baseUrl
    if (u.kind === 'kiro' && u.kiroGroup.trim()) upstream.kiroGroup = u.kiroGroup
    // 掩码值绝不回传：省略表示沿用服务端已存的密钥，空串才是显式清除。
    if (u.clearApiKey) upstream.apiKey = ''
    else if (u.apiKey) upstream.apiKey = u.apiKey
    upstreams.push(upstream)
  })

  const models = draft.models.map((m, mi) => {
    const modelPath = `models.${mi}`
    if (!m.id.trim()) errors[`${modelPath}.id`] = '模型别名不能为空'
    const seenBindings = new Set<string>()
    const bindings: ModelBinding[] = m.bindings.map((b, bi) => {
      const path = `${modelPath}.bindings.${bi}`
      if (!b.id.trim()) errors[`${path}.id`] = '绑定 id 不能为空'
      else if (seenBindings.has(b.id)) errors[`${path}.id`] = `绑定 id "${b.id}" 重复`
      seenBindings.add(b.id)

      if (!seenUpstreams.has(b.upstreamId)) {
        errors[`${path}.upstreamId`] = `绑定指向的上游 "${b.upstreamId}" 不存在`
      }
      if (!b.upstreamModel.trim()) errors[`${path}.upstreamModel`] = '必须填写上游真实模型名'

      const priorityTier = integer(b.priorityTier, '优先级层', errors, `${path}.priorityTier`, 0, 255)
      const weight = integer(b.weight, '权重', errors, `${path}.weight`, 0, 1_000_000)
      const contextWindow = integer(b.contextWindow, '上下文窗口', errors, `${path}.contextWindow`, 1)
      const maxOutputTokens = integer(b.maxOutputTokens, '输出上限', errors, `${path}.maxOutputTokens`, 1)

      // 原生积分的成本是上游下发的证据，不是本地价目表。
      if (b.billingUnit === 'kiroCredit') {
        if (b.costPrices || b.sellPrices) {
          errors[`${path}.billingUnit`] = '原生积分计费不接受价格表；积分成本以上游下发的计量为准'
        }
      } else if (!b.sellPrices) {
        errors[`${path}.sellPrices`] = '按钱计费必须填写售价，否则不知道该向客户收多少'
      }

      const costPrices = b.costPrices ? validatePrices(b.costPrices, b.billingUnit, `${path}.costPrices`, errors) : null
      const sellPrices = b.sellPrices ? validatePrices(b.sellPrices, b.billingUnit, `${path}.sellPrices`, errors) : null

      return {
        id: b.id,
        upstreamId: b.upstreamId,
        upstreamModel: b.upstreamModel,
        enabled: b.enabled,
        priorityTier: priorityTier ?? 0,
        weight: weight ?? 0,
        contextWindow: contextWindow ?? 0,
        maxOutputTokens: maxOutputTokens ?? 0,
        supportsTools: b.supportsTools,
        supportsImages: b.supportsImages,
        supportsReasoning: b.supportsReasoning,
        allowModelSubstitution: b.allowModelSubstitution,
        billingUnit: b.billingUnit,
        costPrices,
        sellPrices,
      }
    })
    const model = {
      id: m.id,
      displayName: m.displayName || null,
      routingMode: m.routingMode === '' ? null : m.routingMode,
      affinityTtlSecs:
        m.affinityTtlSecs === ''
          ? null
          : integer(m.affinityTtlSecs, '会话保持时长', errors, `${modelPath}.affinityTtlSecs`, 1, 86400),
      bindings,
    }
    return model
  })

  if (Object.keys(errors).length > 0) return { errors }

  return {
    config: {
      defaultRoutingMode: draft.defaultRoutingMode,
      affinityTtlSecs: affinityTtlSecs as number,
      maxAttempts: maxAttempts as number,
      requestTimeoutSecs: requestTimeoutSecs as number,
      upstreams,
      models,
    },
    errors,
  }
}
