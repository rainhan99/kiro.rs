import axios from 'axios'
import { storage } from '@/lib/storage'

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
 * 安全相关的配置。
 *
 * 与上面两个不同，**这个要鉴权**——它能改变桌面端的进门方式。
 * 所以另起一个带 x-api-key 的实例，与项目里其它 admin API 同一个范式。
 */
const authed = axios.create({
  baseURL: '/api/admin',
  timeout: 15000,
  headers: { 'Content-Type': 'application/json' },
})
authed.interceptors.request.use((config) => {
  const key = storage.getApiKey()
  if (key) config.headers['x-api-key'] = key
  return config
})

export async function fetchSecurityConfig(): Promise<{ requireAuthOnLaunch: boolean }> {
  const { data } = await authed.get('/config/security')
  return data
}

export async function setSecurityConfig(requireAuthOnLaunch: boolean): Promise<void> {
  await authed.put('/config/security', { requireAuthOnLaunch })
}
