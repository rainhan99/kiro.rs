import { useCallback, useEffect, useRef, useState } from 'react'
import { loadSessions, saveSessions, newSession } from '@/lib/chat-store'
import { createAccumulator, feed, finish } from '@/lib/chat-stream'
import { ensureChatKey, fetchModels, streamMessage } from '@/api/chat'
import type { ChatSession, ChatMessage } from '@/types/chat'

/** 单轮最大输出。够长，又不至于一次跑飞。 */
const MAX_TOKENS = 4096

/**
 * 对话页的状态机。
 *
 * 流式期间**只动一个 state**（`streaming.text`），不重建整个 sessions
 * 数组——否则每来一个分片都要重渲染整棵消息树，长回复会把输入框卡死。
 */
export function useChat() {
  const [sessions, setSessions] = useState<ChatSession[]>(() => loadSessions())
  const [activeId, setActiveId] = useState<string | null>(() => loadSessions()[0]?.id ?? null)
  const [models, setModels] = useState<string[]>([])
  const [ready, setReady] = useState(false)
  const [initError, setInitError] = useState<string | null>(null)
  const [streaming, setStreaming] = useState<string | null>(null)

  const keyRef = useRef<string | null>(null)
  const abortRef = useRef<AbortController | null>(null)
  /** 节流提交：每帧最多刷一次，而不是每个分片刷一次。 */
  const pendingRef = useRef<string | null>(null)
  const frameRef = useRef<number | null>(null)

  useEffect(() => {
    let alive = true
    ;(async () => {
      try {
        const key = await ensureChatKey()
        const list = await fetchModels(key)
        if (!alive) return
        keyRef.current = key
        setModels(list)
        setReady(true)
      } catch (err) {
        if (alive) setInitError(err instanceof Error ? err.message : String(err))
      }
    })()
    return () => {
      alive = false
    }
  }, [])

  const persist = useCallback((next: ChatSession[]) => {
    setSessions(next)
    saveSessions(next)
  }, [])

  const active = sessions.find((s) => s.id === activeId) ?? null

  const create = useCallback(() => {
    const s = newSession(models[0] ?? 'claude-sonnet-4')
    persist([...sessions, s])
    setActiveId(s.id)
  }, [models, persist, sessions])

  const remove = useCallback(
    (id: string) => {
      const next = sessions.filter((s) => s.id !== id)
      persist(next)
      if (activeId === id) setActiveId(next[next.length - 1]?.id ?? null)
    },
    [activeId, persist, sessions],
  )

  const stop = useCallback(() => abortRef.current?.abort(), [])

  const send = useCallback(
    async (text: string) => {
      const session = sessions.find((s) => s.id === activeId)
      if (!session || !keyRef.current || streaming !== null) return

      const user: ChatMessage = { role: 'user', content: text }
      const withUser = {
        ...session,
        messages: [...session.messages, user],
        // 首轮用户输入当标题——比「新对话」有用得多。
        title: session.messages.length === 0 ? text.slice(0, 24) : session.title,
        updatedAt: Date.now(),
      }
      persist(sessions.map((s) => (s.id === session.id ? withUser : s)))

      const acc = createAccumulator()
      const controller = new AbortController()
      abortRef.current = controller
      setStreaming('')

      const flush = () => {
        frameRef.current = null
        if (pendingRef.current !== null) setStreaming(pendingRef.current)
      }

      let aborted = false
      let failure: string | null = null
      try {
        await streamMessage({
          clientKey: keyRef.current,
          model: session.model,
          messages: withUser.messages,
          maxTokens: MAX_TOKENS,
          signal: controller.signal,
          onChunk: (chunk) => {
            feed(acc, chunk)
            pendingRef.current = acc.text
            // 节流到下一帧：每个分片都 setState 会让长回复卡死输入框。
            if (frameRef.current === null) {
              frameRef.current = requestAnimationFrame(flush)
            }
          },
        })
      } catch (err) {
        if (controller.signal.aborted) {
          aborted = true
        } else {
          failure = err instanceof Error ? err.message : String(err)
        }
      } finally {
        if (frameRef.current !== null) cancelAnimationFrame(frameRef.current)
        frameRef.current = null
        pendingRef.current = null
        abortRef.current = null
        setStreaming(null)
      }

      const turn = finish(acc, { aborted })
      const assistant: ChatMessage = {
        role: 'assistant',
        content: turn.text,
        usage: turn.usage,
        aborted: turn.aborted,
        error: failure ?? turn.error,
      }
      setSessions((current) => {
        const next = current.map((s) =>
          s.id === session.id
            ? { ...s, messages: [...withUser.messages, assistant], updatedAt: Date.now() }
            : s,
        )
        saveSessions(next)
        return next
      })
    },
    [activeId, persist, sessions, streaming],
  )

  // 切走时别让上一轮继续跑——它会把增量写进一个已经不在屏幕上的会话。
  useEffect(() => () => abortRef.current?.abort(), [])

  return {
    sessions,
    active,
    activeId,
    models,
    ready,
    initError,
    streaming,
    setActiveId,
    create,
    remove,
    send,
    stop,
  }
}
