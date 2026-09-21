import { beforeEach, describe, expect, test } from 'bun:test'
import { loadSessions, saveSessions, MAX_SESSIONS, newSession } from './chat-store'

const mem = new Map()
globalThis.localStorage = {
  getItem: (k) => (mem.has(k) ? mem.get(k) : null),
  setItem: (k, v) => mem.set(k, String(v)),
  removeItem: (k) => mem.delete(k),
}
beforeEach(() => mem.clear())

const session = (over = {}) => ({
  id: 'a', title: '第一个', model: 'claude-sonnet-4',
  messages: [], updatedAt: 1, ...over,
})

describe('会话持久化', () => {
  test('存进去再读出来是同一份', () => {
    const s = [session()]
    saveSessions(s)
    expect(loadSessions()).toEqual(s)
  })

  test('没存过时返回空数组，不是 null', () => {
    expect(loadSessions()).toEqual([])
  })

  /// localStorage 里可能是任何东西：用户手改过、别的版本写过、被截断过。
  /// 这一页不能因此白屏——对话记录没了很糟，整个管理台打不开更糟。
  test('损坏的数据不抛异常，退化成空列表', () => {
    mem.set('kiro.chat.sessions', '{ 这不是 JSON')
    expect(loadSessions()).toEqual([])
    mem.set('kiro.chat.sessions', '"一个字符串"')
    expect(loadSessions()).toEqual([])
  })

  test('形状不对的条目被丢弃，好的保留', () => {
    mem.set('kiro.chat.sessions', JSON.stringify([
      session({ id: 'ok' }), { nope: true }, null, 42,
      session({ id: 'bad', messages: 'not-an-array' }),
    ]))
    expect(loadSessions().map((s) => s.id)).toEqual(['ok'])
  })

  /// 超上限时丢最旧的**整个会话**，而不是截断某个会话的消息——
  /// 后者会让用户以为对话还在，其实内容已经没了。
  test('超过上限时丢最旧的会话，不截断消息内容', () => {
    const long = '很长的内容'.repeat(50)
    const many = Array.from({ length: MAX_SESSIONS + 3 }, (_, i) =>
      session({ id: `s${i}`, updatedAt: i, messages: [{ role: 'user', content: long }] }))
    saveSessions(many)
    const back = loadSessions()
    expect(back).toHaveLength(MAX_SESSIONS)
    expect(back.map((s) => s.id)).not.toContain('s0')
    expect(back[back.length - 1].messages[0].content).toHaveLength(long.length)
  })

  /// 配额满了要如实报告，不能假装存下了——用户会以为记录还在。
  test('写入失败时返回 false 而不是静静吞掉', () => {
    const original = globalThis.localStorage.setItem
    globalThis.localStorage.setItem = () => {
      throw new Error('QuotaExceededError')
    }
    expect(saveSessions([session()])).toBe(false)
    globalThis.localStorage.setItem = original
    expect(saveSessions([session()])).toBe(true)
  })
})

describe('新建会话', () => {
  test('id 唯一，标题有默认值', () => {
    const a = newSession('claude-sonnet-4')
    const b = newSession('claude-sonnet-4')
    expect(a.id).not.toBe(b.id)
    expect(a.title.length).toBeGreaterThan(0)
    expect(a.messages).toEqual([])
  })
})
