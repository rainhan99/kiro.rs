import { describe, expect, test } from 'bun:test'
import { createEditor, receiveEditor, validateDraft, emptyUpstream, emptyBinding } from './gateway-form'

const view = () => ({
  revision: 3,
  managedModels: ['opus5'],
  config: {
    defaultRoutingMode: 'sticky',
    affinityTtlSecs: 900,
    maxAttempts: 3,
    requestTimeoutSecs: 120,
    upstreams: [
      { id: 'u1', name: '主用', kind: 'anthropic', enabled: true, weight: 10, baseUrl: 'https://api.example.test', hasApiKey: true },
    ],
    models: [
      {
        id: 'opus5',
        bindings: [
          {
            id: 'b1', upstreamId: 'u1', upstreamModel: 'real-model', enabled: true,
            priorityTier: 0, weight: 10, contextWindow: 200000, maxOutputTokens: 8000,
            supportsTools: true, supportsImages: true, supportsReasoning: false,
            billingUnit: 'CNY',
            costPrices: { currency: 'CNY', input: '1', output: '1', cacheRead: '0', cacheWrite: '0' },
            sellPrices: { currency: 'CNY', input: '2', output: '2', cacheRead: '0', cacheWrite: '0' },
          },
        ],
      },
    ],
  },
})

describe('草稿的建立与刷新', () => {
  test('数值与金额一律以字符串入草稿，编辑中的半截输入不会被吞掉', () => {
    const editor = createEditor(view())
    expect(editor.revision).toBe(3)
    expect(editor.draft.upstreams[0].weight).toBe('10')
    expect(editor.draft.models[0].bindings[0].sellPrices.input).toBe('2')
    // 用户打了一半的小数点不该被解析成数字后丢掉。
    editor.draft.models[0].bindings[0].sellPrices.input = '2.'
    const { errors } = validateDraft(editor.draft)
    expect(errors['models.0.bindings.0.sellPrices.input']).toBeTruthy()
    expect(editor.draft.models[0].bindings[0].sellPrices.input).toBe('2.')
  })

  test('后台刷新不得冲掉正在编辑的草稿', () => {
    const editor = createEditor(view())
    editor.draft.affinityTtlSecs = '600'
    const refreshed = receiveEditor(editor, { ...view(), revision: 4 })
    expect(refreshed.draft.affinityTtlSecs).toBe('600')
    expect(refreshed.revision).toBe(4)
    expect(refreshed.stale).toBe(true)
  })

  test('明确要求丢弃时才用服务端的值覆盖', () => {
    const editor = createEditor(view())
    editor.draft.affinityTtlSecs = '600'
    const replaced = receiveEditor(editor, { ...view(), revision: 4 }, true)
    expect(replaced.draft.affinityTtlSecs).toBe('900')
    expect(replaced.stale).toBe(false)
  })
})

describe('金额绝不经过浮点', () => {
  test('十八位小数原样往返', () => {
    const v = view()
    v.config.models[0].bindings[0].sellPrices.input = '0.123456789012345678'
    const editor = createEditor(v)
    const { config, errors } = validateDraft(editor.draft)
    expect(errors).toEqual({})
    expect(config.models[0].bindings[0].sellPrices.input).toBe('0.123456789012345678')
  })

  test('拒绝浮点写法与负数，而不是四舍五入成一个看起来正常的值', () => {
    for (const bad of ['1e-7', '-1', '1.2.3', 'abc', '']) {
      const editor = createEditor(view())
      editor.draft.models[0].bindings[0].sellPrices.input = bad
      const { config, errors } = validateDraft(editor.draft)
      expect(config).toBeUndefined()
      expect(errors['models.0.bindings.0.sellPrices.input']).toBeTruthy()
    }
  })
})

describe('计价与币种必须自洽', () => {
  test('按钱计费的绑定缺售价就拒绝——那意味着不知道该向客户收多少', () => {
    const editor = createEditor(view())
    editor.draft.models[0].bindings[0].sellPrices = null
    const { config, errors } = validateDraft(editor.draft)
    expect(config).toBeUndefined()
    expect(errors['models.0.bindings.0.sellPrices']).toContain('售价')
  })

  test('价格的币种与绑定的计费单位不一致就拒绝', () => {
    const editor = createEditor(view())
    editor.draft.models[0].bindings[0].sellPrices.currency = 'USD'
    const { errors } = validateDraft(editor.draft)
    expect(errors['models.0.bindings.0.sellPrices.currency']).toBeTruthy()
  })

  test('原生积分不接受价格表——积分的成本是上游下发的证据，不是本地价目', () => {
    const editor = createEditor(view())
    const binding = editor.draft.models[0].bindings[0]
    binding.billingUnit = 'kiroCredit'
    const { errors } = validateDraft(editor.draft)
    expect(errors['models.0.bindings.0.billingUnit']).toBeTruthy()
  })
})

describe('引用完整性', () => {
  test('绑定指向不存在的上游要当场指出，而不是等保存才报错', () => {
    const editor = createEditor(view())
    editor.draft.models[0].bindings[0].upstreamId = 'nope'
    const { config, errors } = validateDraft(editor.draft)
    expect(config).toBeUndefined()
    expect(errors['models.0.bindings.0.upstreamId']).toContain('nope')
  })

  test('删除仍被引用的上游要被拦住，并说清是谁在用它', () => {
    const editor = createEditor(view())
    editor.draft.upstreams = []
    const { errors } = validateDraft(editor.draft)
    expect(errors['models.0.bindings.0.upstreamId']).toContain('u1')
  })

  test('重复的上游 id 与绑定 id 都要拒绝', () => {
    const editor = createEditor(view())
    editor.draft.upstreams.push({ ...editor.draft.upstreams[0] })
    expect(validateDraft(editor.draft).errors['upstreams.1.id']).toBeTruthy()

    const other = createEditor(view())
    other.draft.models[0].bindings.push({ ...other.draft.models[0].bindings[0] })
    expect(validateDraft(other.draft).errors['models.0.bindings.1.id']).toBeTruthy()
  })
})

describe('Kiro 上游的约束', () => {
  test('Kiro 用既有凭据池，不接受 baseUrl 或密钥', () => {
    const editor = createEditor(view())
    editor.draft.upstreams[0].kind = 'kiro'
    const { errors } = validateDraft(editor.draft)
    expect(errors['upstreams.0.baseUrl']).toBeTruthy()
  })

  test('只有 Kiro 上游能设凭据分组', () => {
    const editor = createEditor(view())
    editor.draft.upstreams[0].kiroGroup = 'team-a'
    const { errors } = validateDraft(editor.draft)
    expect(errors['upstreams.0.kiroGroup']).toBeTruthy()
  })
})

describe('密钥', () => {
  test('未改动的密钥不回传，避免把掩码当成新密钥写回去', () => {
    const editor = createEditor(view())
    const { config } = validateDraft(editor.draft)
    expect('apiKey' in config.upstreams[0]).toBe(false)
    expect(editor.draft.upstreams[0].hasApiKey).toBe(true)
  })

  test('填了新密钥才回传；显式清空则传空串', () => {
    const editor = createEditor(view())
    editor.draft.upstreams[0].apiKey = 'brand-new'
    expect(validateDraft(editor.draft).config.upstreams[0].apiKey).toBe('brand-new')

    const cleared = createEditor(view())
    cleared.draft.upstreams[0].clearApiKey = true
    expect(validateDraft(cleared.draft).config.upstreams[0].apiKey).toBe('')
  })
})

describe('新增条目有可用的默认值', () => {
  test('新上游与新绑定开箱即可通过校验', () => {
    const editor = createEditor(view())
    editor.draft.upstreams.push(emptyUpstream('u2'))
    editor.draft.models[0].bindings.push(emptyBinding('b2', 'u2'))
    const { config, errors } = validateDraft(editor.draft)
    expect(errors).toEqual({})
    expect(config.upstreams[1].id).toBe('u2')
    expect(config.models[0].bindings[1].upstreamId).toBe('u2')
  })
})
