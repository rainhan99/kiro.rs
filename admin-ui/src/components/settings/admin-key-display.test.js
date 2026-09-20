import { describe, expect, test } from 'bun:test'
import { maskAdminKey, currentKeyRow } from './admin-key-display'

describe('当前登录密钥的展示', () => {
  /// 桌面版免登录后用户从没见过这个密钥，而从浏览器或另一台机器连过来时
  /// 需要它。所以要能看见——但默认脱敏：设置页常在别人能看到屏幕的场合打开。
  test('默认脱敏，保留首尾便于核对是不是同一个', () => {
    expect(maskAdminKey('sk-admin-vsUr5HI0EsOJHJRz4qn5Tr9G')).toBe('sk-admin-…qn5Tr9G')
  })

  test('短到脱不了敏的就原样给出，不要造出比原文还长的东西', () => {
    expect(maskAdminKey('sk-admin-')).toBe('sk-admin-')
    expect(maskAdminKey('abc')).toBe('abc')
  })

  /// 没登录/没密钥时不能显示 "undefined" 或 "null" —— 那看起来像是坏了。
  test('没有密钥时给出明确说法', () => {
    expect(maskAdminKey(null)).toBe('（未登录）')
    expect(maskAdminKey('')).toBe('（未登录）')
    expect(maskAdminKey('   ')).toBe('（未登录）')
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
