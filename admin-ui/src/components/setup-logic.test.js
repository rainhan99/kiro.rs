import { describe, expect, test } from 'bun:test'
import {
  validateSetupForm,
  MIN_ADMIN_KEY_LEN,
  decideEntryScreen,
  applySetupCompleted,
  applyLoggedIn,
  applyLoggedOut,
} from './setup-logic'

describe('初始化表单校验', () => {
  test('密码太短要拦住，并说清最短多少', () => {
    const e = validateSetupForm({ token: 'T', password: 'abc', confirm: 'abc' })
    expect(e.password).toContain(String(MIN_ADMIN_KEY_LEN))
  })

  /// 长度按**字符**算不按字节算——「密码密码」是 4 个字符 12 个字节。
  /// 前后端必须用同一个口径，否则前端放行、后端 400，用户看到的是
  /// 「我明明填对了」。
  test('长度按字符算，与后端口径一致', () => {
    expect(validateSetupForm({ token: 'T', password: '密码密码', confirm: '密码密码' }).password)
      .toBeTruthy()
    expect(validateSetupForm({ token: 'T', password: '密码密码密码密码', confirm: '密码密码密码密码' }).password)
      .toBeUndefined()
  })

  /// 两次不一致必须拦在前端。这是设密码，打错了当场进不去，
  /// 而唯一的补救是重启服务拿新 token。
  test('两次输入不一致要拦住', () => {
    const e = validateSetupForm({ token: 'T', password: 'goodpassword', confirm: 'goodpasswerd' })
    expect(e.confirm).toBeTruthy()
  })

  test('需要 token 时缺了要拦住', () => {
    expect(validateSetupForm({ token: '', password: 'goodpassword', confirm: 'goodpassword' }).token)
      .toBeTruthy()
  })

  /// 桌面端自己持有 token，表单里没有这个字段，就不该因为它为空而拦。
  test('不需要输 token 时，token 为空不算错', () => {
    const e = validateSetupForm(
      { token: '', password: 'goodpassword', confirm: 'goodpassword' },
      { tokenProvided: true },
    )
    expect(e.token).toBeUndefined()
    expect(Object.keys(e)).toHaveLength(0)
  })

  test('全对时没有任何错误', () => {
    expect(validateSetupForm({ token: 'T', password: 'goodpassword', confirm: 'goodpassword' }))
      .toEqual({})
  })
})

describe('进门看到哪一屏', () => {
  const at = (o) => decideEntryScreen({ probed: true, initialized: null, hasKey: false, ...o })

  test('还没探到状态 → 什么都不画', () => {
    expect(decideEntryScreen({ probed: false, initialized: null, hasKey: false })).toBe('loading')
    // 闪一下登录页再跳走，看起来像是掉线了
    expect(decideEntryScreen({ probed: false, initialized: true, hasKey: true })).toBe('loading')
  })

  test('未初始化 → 初始化页，哪怕本地存着旧密钥', () => {
    expect(at({ initialized: false, hasKey: true })).toBe('setup')
  })

  test('已初始化 + 本地有密钥 → 直接进', () => {
    expect(at({ initialized: true, hasKey: true })).toBe('console')
  })

  test('已初始化 + 本地没密钥 → 登录页', () => {
    expect(at({ initialized: true, hasKey: false })).toBe('login')
  })

  /// 探测失败（服务刚起来、网络抖动）时**不能**猜「未初始化」——
  /// 那会把一个已经配好的实例的初始化页摆到人脸上，看起来像是配置丢了。
  /// 保守回落：有密钥就照常进，没有就登录页。
  test('状态探测失败时不猜未初始化', () => {
    expect(at({ initialized: null, hasKey: false })).toBe('login')
    expect(at({ initialized: null, hasKey: true })).toBe('console')
  })
})

describe('状态转移', () => {
  /// **这条是真机跑出来的 bug。**
  ///
  /// 设完密码后页面卡在初始化页不动，再点一次就报「已经初始化了」。
  /// 原因：`initialized` 只在挂载时探一次，设置成功后没人更新它，
  /// 于是屏幕计算仍然看到 `initialized === false`。
  ///
  /// 教训：测了纯函数不等于测了状态机。转移本身必须有断言。
  test('设置成功后必须进控制台，不能卡在初始化页', () => {
    const before = { probed: true, initialized: false, hasKey: false }
    expect(decideEntryScreen(before)).toBe('setup')

    const after = applySetupCompleted(before)
    expect(decideEntryScreen(after)).toBe('console')
  })

  test('登录成功后进控制台', () => {
    const before = { probed: true, initialized: true, hasKey: false }
    expect(decideEntryScreen(before)).toBe('login')
    expect(decideEntryScreen(applyLoggedIn(before))).toBe('console')
  })

  /// 登出回到登录页，而不是初始化页——实例已经被认领了。
  test('登出回到登录页而不是初始化页', () => {
    const state = { probed: true, initialized: true, hasKey: true }
    expect(decideEntryScreen(applyLoggedOut(state))).toBe('login')
  })
})
