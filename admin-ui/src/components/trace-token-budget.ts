/**
 * 请求管线证据里的 token 预算分项。
 *
 * 这些数字是**本地估算**，不是上游的 metadataEvent.tokenUsage，不构成缓存命中或
 * 计费证据。模型上限来自该凭据缓存的模型列表；未缓存或上游未声明时必须显示为
 * 未知，绝不能显示 0 —— 0 会被读成「没有上限」或「已用满」。
 */
export type TokenBudgetRow = { label: string; value: string }

const SECTIONS: [string, string][] = [
  ['current', '当前轮'],
  ['history', '历史'],
  ['tools', '工具声明'],
  ['toolResults', '工具结果'],
  ['images', '图片'],
  ['other', '其它'],
]

function count(value: unknown): string | null {
  return typeof value === 'number' && Number.isFinite(value)
    ? value.toLocaleString('en-US')
    : null
}

/** 上限/余量缺失时的文案。与 0 严格区分。 */
export const UNKNOWN_CEILING = '未知（上游未声明或该凭据未缓存模型列表）'

export function tokenBudgetRows(metrics: unknown): TokenBudgetRow[] | null {
  if (metrics === null || typeof metrics !== 'object' || Array.isArray(metrics)) return null
  const m = metrics as Record<string, unknown>
  const total = count(m.total)
  if (total === null) return null

  const rows: TokenBudgetRow[] = [{ label: '合计（估算）', value: total }]
  for (const [key, label] of SECTIONS) {
    const value = count(m[key])
    if (value !== null) rows.push({ label, value })
  }
  rows.push({ label: '模型输入上限', value: count(m.maxInputTokens) ?? UNKNOWN_CEILING })
  // 余量可为负：已越界时必须如实显示负数，不夹到 0，否则会掩盖越界程度。
  rows.push({ label: '余量', value: count(m.headroom) ?? UNKNOWN_CEILING })
  return rows
}
