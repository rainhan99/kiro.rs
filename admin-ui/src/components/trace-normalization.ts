export interface NormalizationSummary {
  strategy: 'portable-text' | 'refuse' | 'drop'
  scannedBlocks: number
  transformedBlocks: number
  opaqueBytes: number
  eventsTruncated: boolean
}

const strategies = new Set<NormalizationSummary['strategy']>(['portable-text', 'refuse', 'drop'])

function nonNegativeSafeInteger(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0
}

export function normalizationSummary(value: unknown): NormalizationSummary | null {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return null
  const record = value as Record<string, unknown>
  if (!strategies.has(record.strategy as NormalizationSummary['strategy'])
    || !nonNegativeSafeInteger(record.scannedBlocks)
    || !nonNegativeSafeInteger(record.transformedBlocks)
    || !nonNegativeSafeInteger(record.opaqueBytes)
    || typeof record.eventsTruncated !== 'boolean') return null
  return {
    strategy: record.strategy as NormalizationSummary['strategy'],
    scannedBlocks: record.scannedBlocks,
    transformedBlocks: record.transformedBlocks,
    opaqueBytes: record.opaqueBytes,
    eventsTruncated: record.eventsTruncated,
  }
}
