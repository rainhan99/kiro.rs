import type { TurnUsage } from '@/types/chat'

/**
 * 把一轮用量渲染成一行字。
 *
 * 抽成纯函数是为了钉住 SC-9 在**显示层**也成立：解析层保住了 null，
 * 显示层一个 `?? 0` 照样能把它毁掉，而那一行字才是用户真正看到的东西。
 */
export function formatUsage(usage: TurnUsage | undefined): string {
  if (!usage) return '用量未知'

  const n = (v: number | null) => (v === null ? '未知' : v.toLocaleString())
  const parts = [`输入 ${n(usage.inputTokens)}`, `输出 ${n(usage.outputTokens)}`]

  if (usage.cacheReadTokens !== null && usage.cacheReadTokens > 0) {
    parts.push(`缓存读 ${usage.cacheReadTokens.toLocaleString()}`)
  }

  // `null` = 上游没报，`0` = 上游明确报了零。两者在界面上必须分得开：
  // 「不知道花了多少」和「没花钱」是完全不同的两句话。
  if (usage.credits === null) {
    parts.push('计费未知')
  } else {
    const unit = (usage.credits === 1 ? usage.creditUnit : usage.creditUnitPlural) ?? 'credits'
    parts.push(`计费 ${usage.credits} ${unit}`)
  }

  return parts.join(' · ')
}
