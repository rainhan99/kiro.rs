/**
 * 会话有效期的可选值。
 *
 * 抽出来是为了让「默认是不过期」这条能被测试钉住——它决定了升级上来的
 * 用户会不会某天突然被要求重新登录。
 */
export const TTL_OPTIONS: { hours: number; label: string }[] = [
  { hours: 0, label: '不过期' },
  { hours: 1, label: '1 小时' },
  { hours: 8, label: '8 小时' },
  { hours: 24, label: '1 天' },
  { hours: 24 * 7, label: '7 天' },
]

export function describeTtl(hours: number): string {
  return TTL_OPTIONS.find((o) => o.hours === hours)?.label ?? `${hours} 小时`
}
