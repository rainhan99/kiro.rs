import { describe, expect, test } from 'bun:test'
import { calibrationRows, THIN_SAMPLE_THRESHOLD } from './calibration-rows'

const stable = {
  model: 'claude-sonnet-4', endpoint: 'ide', samples: 240,
  minWindowTokens: 199000, maxWindowTokens: 200500, meanWindowTokens: 200000,
  lastSeen: '2026-09-18T00:00:00Z',
}

describe('calibration observation rows', () => {
  test('稳定的分母显示均值，并保留样本数', () => {
    const [row] = calibrationRows([stable])
    expect(row.window).toBe('200,000')
    expect(row.samples).toBe(240)
    expect(row.unstable).toBe(false)
    expect(row.thin).toBe(false)
  })

  test('跨度大时不显示均值，改为显示区间——均值会被读成一个窗口值', () => {
    const [row] = calibrationRows([{ ...stable, minWindowTokens: 120000, maxWindowTokens: 260000 }])
    expect(row.unstable).toBe(true)
    expect(row.window).toBe('120,000 – 260,000')
    expect(row.window).not.toContain('200,000')
  })

  test('样本太少要标出来，否则轶事会被当成结论', () => {
    const [row] = calibrationRows([{ ...stable, samples: THIN_SAMPLE_THRESHOLD - 1 }])
    expect(row.thin).toBe(true)
  })

  test('字段缺失或类型不对的条目整条丢弃，不编造数字', () => {
    expect(calibrationRows([{ ...stable, meanWindowTokens: null }])).toEqual([])
    expect(calibrationRows([{ ...stable, model: 42 }])).toEqual([])
    expect(calibrationRows([null, 'x', 7])).toEqual([])
    expect(calibrationRows(null)).toEqual([])
    expect(calibrationRows({})).toEqual([])
  })

  test('min 为 0 时不做除法，不判为不稳定', () => {
    const [row] = calibrationRows([{ ...stable, minWindowTokens: 0 }])
    expect(row.unstable).toBe(false)
  })
})
