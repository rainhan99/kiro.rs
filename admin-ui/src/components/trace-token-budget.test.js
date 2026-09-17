import { describe, expect, test } from 'bun:test'
import { tokenBudgetRows, UNKNOWN_CEILING } from './trace-token-budget'

const full = {
  source: 'estimate', total: 1234, current: 100, history: 900,
  tools: 120, toolResults: 100, images: 14, other: 0,
  maxInputTokens: 200000, headroom: 198766,
}

const rowFor = (metrics, label) => tokenBudgetRows(metrics)?.find((r) => r.label === label)?.value

describe('pipeline token budget rows', () => {
  test('每个分项都出现，合计标注为估算', () => {
    const rows = tokenBudgetRows(full)
    expect(rows?.[0]).toEqual({ label: '合计（估算）', value: '1,234' })
    for (const label of ['当前轮', '历史', '工具声明', '工具结果', '图片', '其它']) {
      expect(rows?.some((r) => r.label === label)).toBe(true)
    }
  })

  test('上限未知时显示未知，绝不显示 0——0 会被读成没有上限或已用满', () => {
    const unknown = { ...full, maxInputTokens: null, headroom: null }
    expect(rowFor(unknown, '模型输入上限')).toBe(UNKNOWN_CEILING)
    expect(rowFor(unknown, '余量')).toBe(UNKNOWN_CEILING)
    expect(rowFor(unknown, '模型输入上限')).not.toBe('0')
  })

  test('余量为负时如实显示负数，不夹到 0', () => {
    expect(rowFor({ ...full, headroom: -5000 }, '余量')).toBe('-5,000')
  })

  test('真实的 0 上限仍显示 0，与未知区分', () => {
    expect(rowFor({ ...full, maxInputTokens: 0 }, '模型输入上限')).toBe('0')
  })

  test('没有 tokenMetrics 或缺少合计时不渲染，不编造数字', () => {
    expect(tokenBudgetRows(null)).toBeNull()
    expect(tokenBudgetRows(undefined)).toBeNull()
    expect(tokenBudgetRows([])).toBeNull()
    expect(tokenBudgetRows({ current: 5 })).toBeNull()
  })
})
