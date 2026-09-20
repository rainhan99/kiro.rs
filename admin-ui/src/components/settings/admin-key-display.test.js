import { describe, expect, test } from 'bun:test'
import { maskAdminKey, currentKeyRow } from './admin-key-display'

describe('当前登录密钥的展示', () => {
  /// 管理密钥现在**永远是用户自设的密码**（F1 之后不再生成随机串）。
  /// 密码没有「前缀不是秘密」这种结构，露出任何一段都是泄露。
  ///
  /// 这条是真机跑出来的：原来的实现有一句「太短就原样返回」，
  /// 那是给 32 位随机串写的理由；换成用户密码之后，它直接把密码打在屏幕上。
  test('任何长度的密钥都完全遮住，不露出任何一段', () => {
    for (const key of ['test1234', 'a', 'short', '我的密码', 'sk-admin-vsUr5HI0EsOJHJRz4qn5Tr9G']) {
      const masked = maskAdminKey(key)
      expect(masked).not.toContain(key)
      for (const ch of new Set(key)) {
        expect(masked).not.toContain(ch)
      }
    }
  })

  /// 连长度都不该透露——密码的长度本身就是给爆破用的情报。
  test('遮罩长度固定，不随密钥长度变化', () => {
    expect(maskAdminKey('ab')).toBe(maskAdminKey('a-very-long-password-here'))
  })

  /// 没登录/没密钥时不能显示 "undefined" 或 "null" —— 那看起来像是坏了。
  test('没有密钥时给出明确说法', () => {
    expect(maskAdminKey(null)).toBe('（未登录）')
    expect(maskAdminKey('')).toBe('（未登录）')
    expect(maskAdminKey('   ')).toBe('（未登录）')
  })

  test('遮罩看得出是「有东西但被遮住了」，不是空白', () => {
    expect(maskAdminKey('test1234').trim().length).toBeGreaterThan(0)
  })
})

describe('说明文案必须与实际行为一致', () => {
  /// 这条修的是一个真实的错误文案：原来写「同时是 API 的主密钥
  /// （config.json 的 apiKey）」「下游客户端需要同步换成新密钥」。
  /// 两句都是错的——update_admin_key 只改 adminApiKey（handlers.rs:959-962），
  /// 下游用的是另一个 key，完全不受影响。
  /// 错误文案会吓得人不敢轮换管理密钥，或者换完以为自己弄坏了客户端。
  test('不得声称管理密钥就是客户端 API Key', () => {
    const text = currentKeyRow().description
    expect(text).not.toContain('apiKey')
    expect(text).toContain('adminApiKey')
  })

  test('要说清轮换它不影响下游客户端', () => {
    const text = currentKeyRow().rotateHint
    expect(text).toContain('不影响')
  })
})
