import axios, { type AxiosInstance } from 'axios'
import { storage } from '@/lib/storage'

/**
 * Admin API 的统一客户端。
 *
 * 从前每个 api 模块各建一个 axios 实例、各抄一份「带上 x-api-key」的
 * 拦截器——八份一模一样的代码。引入会话过期之后这变成了真问题：
 * 会话**一定**会在使用中失效，而没有任何一处在处理 401，用户只会看到
 * 一堆原始错误，不知道自己该重新登录。
 *
 * 所以收拢成一处，同时装上 401 处理。
 */

type Handler = () => void

const handlers: Handler[] = []
/** 同一轮过期里只通知一次，见 `notifyAuthFailure`。 */
let armed = true

/**
 * 这个错误是不是「凭证不好使了」。
 *
 * 只认 401。
 * - **403 不算**：它的意思是「你是谁我知道，但你不能做这个」。清掉凭证
 *   让人重新登录既解决不了问题，又白白把人踢出去。
 * - **网络错误更不算**：断网时把人登出，等网络回来他还得重新输密码，
 *   而他的凭证从头到尾都是好的。
 */
export function isAuthFailure(error: unknown): boolean {
  const status = (error as { response?: { status?: number } })?.response?.status
  return status === 401
}

/** 注册「凭证失效」的处理器。App 用它退回登录页。 */
export function onAuthFailure(handler: Handler): void {
  handlers.push(handler)
}

/**
 * 通知一次凭证失效。
 *
 * 一次过期会让页面上并发的好几个请求同时 401。登出只该发生一次，
 * 否则会连着弹好几个「已过期」的提示。重新登录后由 `rearmAuthFailure`
 * 解除抑制。
 */
export function notifyAuthFailure(): void {
  if (!armed) return
  armed = false
  for (const handler of handlers) handler()
}

/** 重新登录后允许下一次过期再触发。 */
export function rearmAuthFailure(): void {
  armed = true
}

/**
 * 建一个带鉴权与 401 处理的 admin API 客户端。
 *
 * 发出去的凭证可能是**会话 token**（登录后拿到的），也可能是
 * **原始管理密钥**（升级上来的浏览器里存的还是它，后端仍然接受）。
 * 两者都放在同一个位置，因为它们在协议上是同一个东西：`x-api-key`。
 */
export function createAdminClient(timeout = 15000): AxiosInstance {
  const instance = axios.create({
    baseURL: '/api/admin',
    timeout,
    headers: { 'Content-Type': 'application/json' },
  })

  instance.interceptors.request.use((config) => {
    const credential = storage.getApiKey()
    if (credential) config.headers['x-api-key'] = credential
    return config
  })

  instance.interceptors.response.use(
    (response) => response,
    (error) => {
      if (isAuthFailure(error)) {
        storage.removeApiKey()
        notifyAuthFailure()
      }
      return Promise.reject(error)
    },
  )

  return instance
}

/** 仅供测试：重置模块级状态。 */
export const __resetAuthHandlers = Object.assign(
  () => {
    handlers.length = 0
    armed = true
  },
  {
    notify: notifyAuthFailure,
    rearm: rearmAuthFailure,
  },
)
