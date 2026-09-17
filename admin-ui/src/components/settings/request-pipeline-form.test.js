import { describe, expect, test } from 'bun:test'
import { createEditor, receiveEditor, validateDraft } from './request-pipeline-form'

const config = () => ({
  mode: 'enforce', stripBillingHeader: true, cacheStrategy: 'off', agentMode: 'vibe',
  ingressMaxBytes: 52428800,
  limits: { bodyBytes: null, textFieldBytes: null, toolResultBytes: null, imageBase64Bytes: null },
  artifacts: { enabled: false, thresholdBytes: 131072, maxStoreBytes: 67108864, maxArtifactBytes: 16777216, ttlSecs: 3600, readBytes: 16384, maxRounds: 4 },
  toolResults: { strategy: 'join', chunkBytes: 400000 },
  images: { strategy: 'preserve', tileMaxBase64Bytes: 400000, maxTiles: 32, maxPixels: 40000000 },
  auditEnabled: true, allowSimulatedCache: false, kiroOnly: true,
})
const snapshot = () => ({ source: 'startup', runtimeEditable: false, editable: true, effectiveConfig: config(), savedConfig: config(), revision: 'one', restartRequired: false })

describe('pipeline configuration editor', () => {
  test('blank optional limit submits null; zero is rejected instead of disabling the limit', () => {
    const { draft } = createEditor(snapshot())
    expect(validateDraft(draft).config?.limits.bodyBytes).toBeNull()
    draft['limits.bodyBytes'] = '0'
    expect(validateDraft(draft).errors['limits.bodyBytes']).toBeTruthy()
    expect(validateDraft(draft).config).toBeUndefined()
  })
  test('rejects non-integer, unsafe, empty required and malformed number inputs', () => {
    for (const value of ['1.5', 'NaN', 'Infinity', '', '9007199254740993', '1e4', '-1']) {
      const { draft } = createEditor(snapshot())
      draft.ingressMaxBytes = value
      expect(validateDraft(draft).errors.ingressMaxBytes).toBeTruthy()
    }
  })
  test('enforces ingress and artifact cross-field budgets', () => {
    const { draft } = createEditor(snapshot())
    draft['limits.bodyBytes'] = '52428801'
    draft['artifacts.thresholdBytes'] = '16777217'
    draft['artifacts.maxStoreBytes'] = '1000000'
    const result = validateDraft(draft)
    expect(result.errors['limits.bodyBytes']).toBeTruthy()
    expect(result.errors['artifacts.thresholdBytes']).toBeTruthy()
    expect(result.errors['artifacts.maxArtifactBytes']).toBeTruthy()
  })
  test('inactive chunk budget does not block a small ingress', () => {
    // 与后端 validate() 同一陷阱：关着的预算不得让配置无法保存。
    const { draft } = createEditor(snapshot())
    draft.ingressMaxBytes = '1024'
    expect(validateDraft(draft).config?.toolResults.strategy).toBe('join')
    draft['toolResults.strategy'] = 'lossless-chunks'
    expect(validateDraft(draft).errors['toolResults.chunkBytes']).toBeTruthy()
  })
  test('active chunk budget must fit the tool-result limit or chunking is pointless', () => {
    const { draft } = createEditor(snapshot())
    draft['toolResults.strategy'] = 'lossless-chunks'
    draft['limits.toolResultBytes'] = '100000'
    expect(validateDraft(draft).errors['toolResults.chunkBytes']).toBeTruthy()
    draft['toolResults.chunkBytes'] = '50000'
    expect(validateDraft(draft).errors['toolResults.chunkBytes']).toBeFalsy()
  })
  test('active tile budget must fit ingress; inactive budget remains valid', () => {
    const { draft } = createEditor(snapshot())
    draft.ingressMaxBytes = '1024'
    expect(validateDraft(draft).config).toBeDefined()
    draft['images.strategy'] = 'lossless-tiles'
    expect(validateDraft(draft).errors['images.tileMaxBase64Bytes']).toBeTruthy()
  })
  test('saving legacy unsafe config explicitly produces Kiro-only config without simulated cache', () => {
    const data = snapshot()
    data.savedConfig.kiroOnly = false
    data.savedConfig.allowSimulatedCache = true
    const editor = createEditor(data)
    expect(editor.safetyAdjusted).toBe(true)
    const result = validateDraft(editor.draft).config
    expect(result.kiroOnly).toBe(true)
    expect(result.allowSimulatedCache).toBe(false)
    expect(data.savedConfig.kiroOnly).toBe(false)
  })
  test('background refresh preserves edited draft and revision; explicit reload replaces both', () => {
    const editor = createEditor(snapshot())
    editor.draft.mode = 'audit'
    const incoming = snapshot()
    incoming.revision = 'two'
    incoming.savedConfig.mode = 'off'
    const unchanged = receiveEditor(editor, incoming)
    expect(unchanged.draft.mode).toBe('audit')
    expect(unchanged.revision).toBe('one')
    const replaced = receiveEditor(editor, incoming, true)
    expect(replaced.draft.mode).toBe('off')
    expect(replaced.revision).toBe('two')
  })
})
