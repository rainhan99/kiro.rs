import { describe, expect, test } from 'bun:test'
import { CHAT_KEY_NAME, pickChatKey } from './chat-key'

const deps = (over = {}) => ({
  cached: null,
  list: async () => [],
  create: async () => ({ id: 9, key: 'sk-fresh' }),
  rotate: async () => { throw new Error('不该轮换') },
  ...over,
})

describe('对话用的客户端 Key', () => {
  /// 本地已经有明文就直接用，不去动服务端——否则每次刷新都轮换一次，
  /// 而轮换会把上一次的 Key 作废。
  test('本地已有明文时直接复用，不碰服务端', async () => {
    const calls = []
    const got = await pickChatKey(deps({
      cached: 'sk-cached',
      list: async () => { calls.push('list'); return [] },
    }))
    expect(got).toBe('sk-cached')
    expect(calls).toEqual([])
  })

  /// localStorage 被清过：服务端那条还在，但列表是脱敏的
  /// （handlers.rs 的 mask_client_key），拿不回明文。此时轮换取回，
  /// 而不是再铸一个——否则每清一次浏览器就堆一个 Key。
  test('本地没有但服务端已有同名 Key 时，轮换取回而不是新建', async () => {
    const calls = []
    const got = await pickChatKey(deps({
      list: async () => { calls.push('list'); return [{ id: 7, name: CHAT_KEY_NAME }] },
      create: async () => { calls.push('create'); return { id: 9, key: 'sk-new' } },
      rotate: async (id) => { calls.push(`rotate:${id}`); return { id, key: 'sk-rotated' } },
    }))
    expect(got).toBe('sk-rotated')
    expect(calls).toEqual(['list', 'rotate:7'])
  })

  test('两边都没有时才新建', async () => {
    const calls = []
    const got = await pickChatKey(deps({
      list: async () => { calls.push('list'); return [] },
      create: async () => { calls.push('create'); return { id: 9, key: 'sk-fresh' } },
    }))
    expect(got).toBe('sk-fresh')
    expect(calls).toEqual(['list', 'create'])
  })

  /// 系统 Key（id=0，即 config.apiKey）绝不能被轮换——那会把所有现有
  /// 客户端踢下线。它恰好也可能叫这个名字（用户手改过），所以要按
  /// isSystem 判断而不是按名字。
  test('绝不轮换系统 Key，哪怕它同名', async () => {
    const got = await pickChatKey(deps({
      list: async () => [{ id: 0, name: CHAT_KEY_NAME, isSystem: true }],
      create: async () => ({ id: 9, key: 'sk-fresh' }),
    }))
    expect(got).toBe('sk-fresh')
  })

  /// 被禁用的同名 Key 也不该复用——轮换出来照样是禁用的，
  /// 对话会直接 401 而用户不知道为什么。
  test('跳过被禁用的同名 Key', async () => {
    const got = await pickChatKey(deps({
      list: async () => [{ id: 3, name: CHAT_KEY_NAME, disabled: true }],
      create: async () => ({ id: 9, key: 'sk-fresh' }),
    }))
    expect(got).toBe('sk-fresh')
  })
})
