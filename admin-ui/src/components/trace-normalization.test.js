import { expect, test } from 'bun:test'
import { normalizationSummary } from './trace-normalization'

test('normalization evidence exposes counts but no arbitrary content', () => {
  const summary = normalizationSummary({
    strategy: 'portable-text', scannedBlocks: 9, transformedBlocks: 3,
    opaqueBytes: 40, eventsTruncated: false,
    events: [{ path: 'messages[1]', originalType: 'document', fingerprint: 'abc', text: 'SECRET' }],
  })
  expect(summary).toEqual({ strategy: 'portable-text', scannedBlocks: 9, transformedBlocks: 3, opaqueBytes: 40, eventsTruncated: false })
  expect(JSON.stringify(summary)).not.toContain('SECRET')
})

const validEvidence = () => ({
  strategy: 'portable-text', scannedBlocks: 9, transformedBlocks: 3,
  opaqueBytes: 40, eventsTruncated: false,
})

test('each numeric counter rejects malformed values independently', () => {
  for (const field of ['scannedBlocks', 'transformedBlocks', 'opaqueBytes']) {
    for (const value of [-1, 1.5, Number.MAX_SAFE_INTEGER + 1, '9', Infinity, NaN]) {
      expect(normalizationSummary({ ...validEvidence(), [field]: value })).toBeNull()
    }
  }
})

test('strategy and truncation types are independently validated', () => {
  for (const strategy of [null, 'unknown', 1, {}]) {
    expect(normalizationSummary({ ...validEvidence(), strategy })).toBeNull()
  }
  for (const eventsTruncated of [null, 0, 'false', {}]) {
    expect(normalizationSummary({ ...validEvidence(), eventsTruncated })).toBeNull()
  }
  expect(normalizationSummary('bad')).toBeNull()
})
