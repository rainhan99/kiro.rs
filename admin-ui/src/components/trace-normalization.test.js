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

test('malformed evidence is not rendered as trusted numbers', () => {
  expect(normalizationSummary({ transformedBlocks: -1 })).toBeNull()
  expect(normalizationSummary('bad')).toBeNull()
})
