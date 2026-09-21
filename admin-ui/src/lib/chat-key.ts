/**
 * 对话页用哪个客户端 Key 调 `/v1/messages`。
 *
 * 为什么需要这一套：`/v1/*` 要的是**客户端 Key**（`client_api_keys.json`
 * 里的 `sk-…`），而管理界面手里只有会话 token。两者是不同的密钥空间。
 *
 * 为什么不新增一个「内部对话」端点：那会绕开 `KeyContext`，本轮用量与
 * 计费的归属就得另起一套。走既有的客户端 Key，这一页的花费在统计页里
 * 单独可见，也能随时从 API Keys 页删掉。
 *
 * 为什么这不算扩大权限面：持有管理凭据的人本来就能调
 * `POST /api/admin/client-keys` 铸一个可用的 Key——这里用的正是那个端点。
 */

/** 对话页专用 Key 的名字。 */
export const CHAT_KEY_NAME = '内置对话'

type KeyRow = { id: number; name: string; isSystem?: boolean; disabled?: boolean }

export type ChatKeyDeps = {
  /** 本地缓存的明文。有就直接用。 */
  cached: string | null
  list: () => Promise<KeyRow[]>
  create: () => Promise<{ id: number; key: string }>
  rotate: (id: number) => Promise<{ id: number; key: string }>
}

/**
 * 拿到一个能用的客户端 Key 明文。
 *
 * 顺序是：本地缓存 → 服务端同名 Key（轮换取回明文）→ 新建。
 *
 * 中间那一步存在的理由：列表接口**故意脱敏**，已存在的 Key 拿不回明文。
 * 不轮换取回而直接新建的话，用户每清一次浏览器就多堆一个 Key。
 */
export async function pickChatKey(deps: ChatKeyDeps): Promise<string> {
  const cached = deps.cached?.trim()
  if (cached) return cached

  const rows = await deps.list()
  const reusable = rows.find(
    (r) =>
      r.name === CHAT_KEY_NAME &&
      // 系统 Key（id=0，即 config.apiKey）绝不能轮换——那会把所有现有
      // 客户端踢下线。按 isSystem 判断而不是按 id，名字可能被手改过。
      !r.isSystem &&
      // 禁用的复用了也没用：轮换出来照样是禁用的，对话会直接 401
      // 而用户不知道为什么。
      !r.disabled,
  )

  if (reusable) {
    return (await deps.rotate(reusable.id)).key
  }
  return (await deps.create()).key
}
