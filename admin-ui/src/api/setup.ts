import axios from 'axios'
import { createAdminClient } from './admin-client'

/**
 * 首次初始化的两个端点。
 *
 * 它们**不带** x-api-key——未初始化的实例还没有密钥可验，这是鸡生蛋的
 * 唯一出口。所以这里另起一个 axios 实例，而不是复用带拦截器的那个：
 * 复用会把 localStorage 里可能残留的旧密钥带上去，让人以为鉴权起作用了。
 */
const api = axios.create({
  baseURL: '/api/admin',
  timeout: 15000,
  headers: { 'Content-Type': 'application/json' },
})

/**
 * 实例是否已经被认领。
 *
 * 探测失败时返回 `null` 而不是 `false`：分不清「没探到」和「未初始化」
 * 会把初始化页摆到已配好的实例上。调用方据此保守回落（见
 * `decideEntryScreen`）。
 */
export async function fetchSetupStatus(): Promise<boolean | null> {
  try {
    const { data } = await api.get<{ initialized: boolean }>('/setup/status')
    return typeof data?.initialized === 'boolean' ? data.initialized : null
  } catch {
    return null
  }
}

export async function performSetup(req: {
  setupToken: string
  adminKey: string
}): Promise<void> {
  await api.post('/setup', req)
}

/**
 * 用管理密码换一个会话 token。
 *
 * 这是登录。之后所有请求带的是 token 而不是密码——密码因此不会长期
 * 躺在 localStorage 里。
 */
export async function createSession(key: string): Promise<{
  token: string
  expiresAt: number | null
}> {
  const { data } = await api.post('/session', { key })
  return data
}

/** 登出：作废当前这一个 token。别的设备与密码本身都不受影响。 */
export async function deleteSession(): Promise<void> {
  await authed.delete('/session').catch(() => {
    // 登出失败不该挡着人登出。本地凭证无论如何都要清掉——
    // 服务端那条记录最多留到过期。
  })
}

/**
 * 会话有效期（小时）。0 表示不过期。
 *
 * 要鉴权：它能改变谁进得来。
 */
const authed = createAdminClient()

export async function fetchSessionTtl(): Promise<number> {
  const { data } = await authed.get('/config/security')
  return typeof data?.adminSessionTtlHours === 'number' ? data.adminSessionTtlHours : 0
}

export async function setSessionTtl(hours: number): Promise<void> {
  await authed.put('/config/security', { adminSessionTtlHours: hours })
}
