import { describe, expect, test } from 'bun:test'
import { formatUsage } from './chat-usage'

const usage = (over = {}) => ({
  inputTokens: null, outputTokens: null, cacheCreationTokens: null,
  cacheReadTokens: null, credits: null, creditUnit: null, creditUnitPlural: null,
  ...over,
})

describe('每轮用量的显示', () => {
  /// SC-9 在显示层也必须成立。解析层保住了 null，显示层一个 `?? 0`
  /// 照样能把它毁掉——而那一行字才是用户真正看到的东西。
  test('未知就写「未知」，不写 0', () => {
    const text = formatUsage(usage())
    expect(text).toContain('未知')
    expect(text).not.toMatch(/输入 0|输出 0|计费 0/)
  })

  test('上游明确报 0 时显示 0，与「未知」分得开', () => {
    expect(formatUsage(usage({ credits: 0 }))).toContain('计费 0')
    expect(formatUsage(usage({ credits: 0 }))).not.toContain('计费未知')
  })

  test('有值就如实显示，不重算', () => {
    const text = formatUsage(usage({
      inputTokens: 1234, outputTokens: 56,
      credits: 0.0169543708291874, creditUnitPlural: 'credits',
    }))
    expect(text).toContain('0.0169543708291874')
    expect(text).toContain('56')
  })

  test('整个 usage 缺失时说「用量未知」而不是崩', () => {
    expect(formatUsage(undefined)).toBe('用量未知')
  })

  /// 缓存读为 0 时不显示——那是常态，每行都挂个「缓存读 0」是噪音。
  test('缓存读为 0 或未知时不显示那一段', () => {
    expect(formatUsage(usage({ cacheReadTokens: 0 }))).not.toContain('缓存读')
    expect(formatUsage(usage())).not.toContain('缓存读')
    expect(formatUsage(usage({ cacheReadTokens: 900 }))).toContain('缓存读 900')
  })
})
