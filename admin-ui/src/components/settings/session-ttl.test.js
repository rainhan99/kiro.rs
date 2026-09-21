import { describe, expect, test } from 'bun:test'
import { TTL_OPTIONS, describeTtl } from './session-ttl'

describe('会话有效期选项', () => {
  /// 默认必须是「不过期」。这条决定了升级上来的用户会不会某天突然
  /// 被要求重新登录——一个他没做过任何操作就发生的变化。
  test('第一项是不过期，对应 0', () => {
    expect(TTL_OPTIONS[0]).toEqual({ hours: 0, label: '不过期' })
  })

  test('选项按时长递增，没有重复', () => {
    const hours = TTL_OPTIONS.map((o) => o.hours)
    expect(hours).toEqual([...hours].sort((a, b) => a - b))
    expect(new Set(hours).size).toBe(hours.length)
  })

  test('describeTtl 认得每个选项，也不会对意外值崩', () => {
    for (const o of TTL_OPTIONS) expect(describeTtl(o.hours)).toBe(o.label)
    expect(describeTtl(999)).toContain('999')
  })
})
