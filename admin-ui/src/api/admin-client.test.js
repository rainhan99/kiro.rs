import { describe, expect, test, beforeEach } from 'bun:test'
import { isAuthFailure, onAuthFailure, __resetAuthHandlers } from './admin-client'

beforeEach(() => __resetAuthHandlers())

describe('识别鉴权失效', () => {
  test('401 是鉴权失效', () => {
    expect(isAuthFailure({ response: { status: 401 } })).toBe(true)
  })

  /// 403 不是。它的意思是「你是谁我知道，但你不能做这个」——清掉凭证
  /// 让人重新登录既解决不了问题，又白白把人踢出去。
  test('403 不是鉴权失效', () => {
    expect(isAuthFailure({ response: { status: 403 } })).toBe(false)
  })

  /// 网络错误更不是。断网时把人登出，等网络回来他还得重新输密码——
  /// 而他的凭证从头到尾都是好的。
  test('网络错误不是鉴权失效', () => {
    expect(isAuthFailure({ message: 'Network Error' })).toBe(false)
    expect(isAuthFailure({ code: 'ECONNABORTED' })).toBe(false)
    expect(isAuthFailure(new Error('boom'))).toBe(false)
  })

  test('其它状态码都不是', () => {
    for (const status of [400, 404, 409, 429, 500, 502]) {
      expect(isAuthFailure({ response: { status } })).toBe(false)
    }
  })
})

describe('鉴权失效的通知', () => {
  test('注册的处理器会被调用', () => {
    const seen = []
    onAuthFailure(() => seen.push('a'))
    onAuthFailure(() => seen.push('b'))
    __resetAuthHandlers.notify()
    expect(seen).toEqual(['a', 'b'])
  })

  /// 一次过期会让页面上并发的好几个请求同时 401。登出只该发生一次，
  /// 否则会连着弹好几个「已过期」的提示。
  test('同一轮里只通知一次', () => {
    let count = 0
    onAuthFailure(() => count++)
    __resetAuthHandlers.notify()
    __resetAuthHandlers.notify()
    __resetAuthHandlers.notify()
    expect(count).toBe(1)
  })

  test('重新登录后可以再次触发', () => {
    let count = 0
    onAuthFailure(() => count++)
    __resetAuthHandlers.notify()
    __resetAuthHandlers.rearm()
    __resetAuthHandlers.notify()
    expect(count).toBe(2)
  })
})
