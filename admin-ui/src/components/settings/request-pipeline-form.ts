import type { PipelineConfig, RequestPipelineSnapshot } from '../../types/request-pipeline'

export type PipelineDraft = Record<string, string | boolean>
export interface PipelineEditor {
  draft: PipelineDraft
  baseline: PipelineDraft
  revision: string | null
  safetyAdjusted: boolean
}
export const numericFields = [
  { key: 'ingressMaxBytes', label: '入口请求上限', min: 1024, max: 104857600, unit: '字节' },
  { key: 'limits.bodyBytes', label: '最终请求体本地上限', min: 1, max: 104857600, unit: '字节', optional: true },
  { key: 'limits.textFieldBytes', label: '单个文本字段上限', min: 1, max: 104857600, unit: '字节', optional: true },
  { key: 'limits.toolResultBytes', label: '工具结果上限', min: 1, max: 104857600, unit: '字节', optional: true },
  { key: 'limits.imageBase64Bytes', label: '图片 Base64 上限', min: 1, max: 104857600, unit: '字节', optional: true },
  { key: 'artifacts.thresholdBytes', label: '转存阈值', min: 1024, max: 1073741824, unit: '字节' },
  { key: 'artifacts.maxArtifactBytes', label: '单个内容上限', min: 1024, max: 1073741824, unit: '字节' },
  { key: 'artifacts.maxStoreBytes', label: '内存存储总上限', min: 1024, max: 1073741824, unit: '字节' },
  { key: 'artifacts.ttlSecs', label: '内容有效期', min: 60, max: 86400, unit: '秒' },
  { key: 'artifacts.readBytes', label: '每次读取上限', min: 256, max: 65536, unit: '字节' },
  { key: 'artifacts.maxRounds', label: '读取轮数上限', min: 1, max: 16, unit: '轮' },
  { key: 'toolResults.chunkBytes', label: '工具结果分片上限', min: 1024, max: 104857600, unit: '字节' },
  { key: 'images.tileMaxBase64Bytes', label: '单块 Base64 上限', min: 4096, max: 104857600, unit: '字节' },
  { key: 'images.maxTiles', label: '切片数量上限', min: 1, max: 128, unit: '块' },
  { key: 'images.maxPixels', label: '解码像素上限', min: 1, max: 100000000, unit: '像素' },
] as const

export function flattenConfig(config: PipelineConfig): PipelineDraft {
  const flat: PipelineDraft = {}
  for (const [key, value] of Object.entries(config)) {
    if (value !== null && typeof value === 'object') {
      for (const [nested, item] of Object.entries(value)) {
        flat[`${key}.${nested}`] = typeof item === 'boolean' ? item : item === null ? '' : String(item)
      }
    } else flat[key] = typeof value === 'boolean' ? value : String(value)
  }
  return flat
}

export function createEditor(snapshot: RequestPipelineSnapshot): PipelineEditor {
  const config = snapshot.savedConfig ?? snapshot.effectiveConfig
  const draft = flattenConfig(config)
  draft.kiroOnly = true
  draft.allowSimulatedCache = false
  return { draft, baseline: { ...draft }, revision: snapshot.revision, safetyAdjusted: !config.kiroOnly || config.allowSimulatedCache }
}
export function receiveEditor(editor: PipelineEditor | null, snapshot: RequestPipelineSnapshot, replace = false): PipelineEditor {
  return replace || !editor ? createEditor(snapshot) : editor
}
export function validateDraft(draft: PipelineDraft): { config?: PipelineConfig; errors: Record<string, string> } {
  const errors: Record<string, string> = {}
  const numbers: Record<string, number | null> = {}
  for (const field of numericFields) {
    const raw = String(draft[field.key] ?? '').trim()
    if (!raw && 'optional' in field) { numbers[field.key] = null; continue }
    const value = Number(raw)
    if (!/^\d+$/.test(raw) || !Number.isSafeInteger(value) || value < field.min || value > field.max) {
      errors[field.key] = `请输入 ${field.min.toLocaleString()}–${field.max.toLocaleString()} 的整数${'optional' in field ? '；留空停用，0 无效' : ''}`
    } else numbers[field.key] = value
  }
  const n = (key: string) => numbers[key] as number
  for (const field of numericFields.filter((field) => field.key.startsWith('limits.'))) {
    if (n(field.key) > n('ingressMaxBytes')) errors[field.key] = '不能超过入口请求上限'
  }
  if (n('artifacts.thresholdBytes') > n('artifacts.maxArtifactBytes')) errors['artifacts.thresholdBytes'] = '转存阈值不能超过单个内容上限'
  if (n('artifacts.maxArtifactBytes') > n('artifacts.maxStoreBytes')) errors['artifacts.maxArtifactBytes'] = '单个内容上限不能超过内存存储总上限'
  if (draft['images.strategy'] === 'lossless-tiles' && n('images.tileMaxBase64Bytes') > n('ingressMaxBytes')) errors['images.tileMaxBase64Bytes'] = '启用切片时，单块上限不能超过入口请求上限'
  if (draft['toolResults.strategy'] === 'lossless-chunks') {
    // 与后端 validate() 同构：未启用时不校验，避免一个关着的预算卡住配置。
    if (n('toolResults.chunkBytes') > n('ingressMaxBytes')) errors['toolResults.chunkBytes'] = '启用分片时，分片上限不能超过入口请求上限'
    const toolResultLimit = numbers['limits.toolResultBytes']
    if (toolResultLimit !== null && n('toolResults.chunkBytes') > toolResultLimit) errors['toolResults.chunkBytes'] = '启用分片时，分片上限不能超过工具结果上限，否则分片没有意义'
  }
  if (Object.keys(errors).length) return { errors }
  return { errors, config: {
    mode: draft.mode as PipelineConfig['mode'], stripBillingHeader: draft.stripBillingHeader === true,
    cacheStrategy: draft.cacheStrategy as PipelineConfig['cacheStrategy'], agentMode: draft.agentMode as PipelineConfig['agentMode'],
    ingressMaxBytes: n('ingressMaxBytes'),
    limits: { bodyBytes: numbers['limits.bodyBytes'], textFieldBytes: numbers['limits.textFieldBytes'], toolResultBytes: numbers['limits.toolResultBytes'], imageBase64Bytes: numbers['limits.imageBase64Bytes'] },
    artifacts: { enabled: draft['artifacts.enabled'] === true, thresholdBytes: n('artifacts.thresholdBytes'), maxArtifactBytes: n('artifacts.maxArtifactBytes'), maxStoreBytes: n('artifacts.maxStoreBytes'), ttlSecs: n('artifacts.ttlSecs'), readBytes: n('artifacts.readBytes'), maxRounds: n('artifacts.maxRounds') },
    toolResults: { strategy: draft['toolResults.strategy'] as PipelineConfig['toolResults']['strategy'], chunkBytes: n('toolResults.chunkBytes') },
    images: { strategy: draft['images.strategy'] as PipelineConfig['images']['strategy'], tileMaxBase64Bytes: n('images.tileMaxBase64Bytes'), maxTiles: n('images.maxTiles'), maxPixels: n('images.maxPixels') },
    auditEnabled: draft.auditEnabled === true, kiroOnly: true, allowSimulatedCache: false,
  } }
}
