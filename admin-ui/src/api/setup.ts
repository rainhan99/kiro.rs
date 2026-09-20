import axios from 'axios'

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
