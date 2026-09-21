import type { ChatSession, ChatMessage } from '@/types/chat'

/**
 * 对话记录的持久化。
 *
 * 存在浏览器本地。**界面上要如实说明这一点**——它不跨设备、不跨浏览器，
 * 清缓存就没了。假装它是云端同步的会让人在丢了记录时觉得是 bug。
 */

const KEY = 'kiro.chat.sessions'

/** 保留的会话数上限。 */
export const MAX_SESSIONS = 50

function isMessage(v: unknown): v is ChatMessage {
  const m = v as ChatMessage
  return (
    !!m &&
    typeof m === 'object' &&
    (m.role === 'user' || m.role === 'assistant') &&
    typeof m.content === 'string'
  )
}

function isSession(v: unknown): v is ChatSession {
  const s = v as ChatSession
  return (
    !!s &&
    typeof s === 'object' &&
    typeof s.id === 'string' &&
    typeof s.title === 'string' &&
    typeof s.model === 'string' &&
    Array.isArray(s.messages) &&
    s.messages.every(isMessage) &&
    typeof s.updatedAt === 'number'
  )
}

/**
 * 读出所有会话。
 *
 * 任何异常都退化成空列表：localStorage 里可能是用户手改过的、别的版本
 * 写过的、被截断过的东西。对话记录没了很糟，**整个管理台打不开更糟**。
 */
export function loadSessions(): ChatSession[] {
  try {
    const raw = localStorage.getItem(KEY)
    if (!raw) return []
    const parsed: unknown = JSON.parse(raw)
    if (!Array.isArray(parsed)) return []
    return parsed.filter(isSession)
  } catch {
    return []
  }
}

/**
 * 写回所有会话。返回是否真的存下了。
 *
 * 超上限时丢最旧的**整个会话**，不截断任何一个会话的消息——后者会让
 * 用户以为对话还在，其实内容已经没了。
 *
 * 配额满时返回 `false` 而不是静静吞掉：调用方要能告诉用户「这条没存下」。
 */
export function saveSessions(sessions: ChatSession[]): boolean {
  const kept = [...sessions]
    .sort((a, b) => a.updatedAt - b.updatedAt)
    .slice(-MAX_SESSIONS)
  try {
    localStorage.setItem(KEY, JSON.stringify(kept))
    return true
  } catch {
    return false
  }
}

export function newSession(model: string): ChatSession {
  return {
    id: `${Date.now()}-${Math.random().toString(36).slice(2, 10)}`,
    title: '新对话',
    model,
    messages: [],
    updatedAt: Date.now(),
  }
}
