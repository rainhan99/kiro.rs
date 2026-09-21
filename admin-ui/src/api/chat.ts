import { createAdminClient } from './admin-client'
import { pickChatKey, CHAT_KEY_NAME } from '@/lib/chat-key'
import type { ChatMessage } from '@/types/chat'

/**
 * 对话走 `/v1/messages`——与 Claude Code 等客户端**同一条路**。
 *
 * 不新增推理端点：新端点会绕开 `KeyContext`，本轮用量与计费的归属就得
 * 另起一套，而那套迟早会和主路径漂移。
 */

const CACHED_KEY = 'kiro.chat.clientKey'
const admin = createAdminClient()

export function parseModels(raw: unknown): string[] {
  const data = (raw as { data?: unknown })?.data
  if (!Array.isArray(data)) return []
  return data
    .map((m) => (m as { id?: unknown })?.id)
    .filter((id): id is string => typeof id === 'string')
}

/** 拿到（必要时铸出）对话用的客户端 Key。 */
export async function ensureChatKey(): Promise<string> {
  const key = await pickChatKey({
    cached: safeRead(CACHED_KEY),
    list: async () => {
      const { data } = await admin.get('/client-keys')
      return (data?.keys ?? []).map((k: Record<string, unknown>) => ({
        id: k.id as number,
        name: k.name as string,
        isSystem: Boolean(k.isSystem),
        disabled: Boolean(k.disabled),
      }))
    },
    create: async () => {
      const { data } = await admin.post('/client-keys', {
        name: CHAT_KEY_NAME,
        description: '管理界面「对话」页专用',
      })
      return { id: data.id, key: data.key }
    },
    rotate: async (id: number) => {
      const { data } = await admin.post(`/client-keys/${id}/rotate`)
      return { id: data.id, key: data.key }
    },
  })
  safeWrite(CACHED_KEY, key)
  return key
}

export async function fetchModels(clientKey: string): Promise<string[]> {
  const resp = await fetch('/v1/models', { headers: { 'x-api-key': clientKey } })
  if (!resp.ok) throw new Error(`拉取模型列表失败（HTTP ${resp.status}）`)
  return parseModels(await resp.json())
}

/**
 * 发一轮对话，流式读回。
 *
 * 用裸 `fetch` 而不是 axios：要的是 `ReadableStream`，axios 在浏览器里
 * 拿不到未完成的响应体。
 */
export async function streamMessage(args: {
  clientKey: string
  model: string
  messages: ChatMessage[]
  maxTokens: number
  signal: AbortSignal
  onChunk: (text: string) => void
}): Promise<void> {
  const resp = await fetch('/v1/messages', {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      'x-api-key': args.clientKey,
      'anthropic-version': '2023-06-01',
    },
    body: JSON.stringify({
      model: args.model,
      max_tokens: args.maxTokens,
      stream: true,
      messages: args.messages.map((m) => ({ role: m.role, content: m.content })),
    }),
    signal: args.signal,
  })

  if (!resp.ok || !resp.body) {
    // 把上游的错误体透出来。413 的 local_payload_limit、429 的配额提示
    // 都在里面，显示成「网络错误」等于把线索扔了。
    const detail = await resp.text().catch(() => '')
    throw new Error(detail || `请求失败（HTTP ${resp.status}）`)
  }

  const reader = resp.body.getReader()
  const decoder = new TextDecoder()
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    // stream: true —— 多字节字符可能跨分片，交给 TextDecoder 缝合。
    args.onChunk(decoder.decode(value, { stream: true }))
  }
}

function safeRead(key: string): string | null {
  try {
    return localStorage.getItem(key)
  } catch {
    return null
  }
}

function safeWrite(key: string, value: string): void {
  try {
    localStorage.setItem(key, value)
  } catch {
    // 存不下就每次重新轮换，功能仍可用，只是多一次请求。
  }
}
