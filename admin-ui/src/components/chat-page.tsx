import { useEffect, useRef, useState } from 'react'
import { MessageSquarePlus, Trash2, Square, Send } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { useChat } from '@/hooks/use-chat'
import { formatUsage } from './chat-usage'

/**
 * 对话页。
 *
 * 走 `/v1/messages`——与其它客户端同一条路。一期只做纯文本多轮：
 * 工具调用展示、图片输入、thinking 展开都不在范围内。
 */
export function ChatPage() {
  const chat = useChat()
  const [draft, setDraft] = useState('')
  const bottomRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [chat.active?.messages.length, chat.streaming])

  const submit = (e: React.FormEvent) => {
    e.preventDefault()
    const text = draft.trim()
    if (!text || chat.streaming !== null) return
    setDraft('')
    void chat.send(text)
  }

  if (chat.initError) {
    return (
      <div className="p-6 text-sm text-destructive">
        对话功能初始化失败：{chat.initError}
      </div>
    )
  }

  return (
    <div className="flex h-[calc(100vh-8rem)] gap-4">
      {/* 会话列表 */}
      <aside className="flex w-56 shrink-0 flex-col gap-2 border-r pr-3">
        <Button size="sm" variant="outline" onClick={chat.create} disabled={!chat.ready}>
          <MessageSquarePlus className="h-3.5 w-3.5" />
          新对话
        </Button>
        <div className="flex-1 space-y-1 overflow-y-auto">
          {chat.sessions
            .slice()
            .reverse()
            .map((s) => (
              <div
                key={s.id}
                className={`group flex items-center gap-1 rounded-md px-2 py-1.5 text-[12.5px] ${
                  s.id === chat.activeId ? 'bg-muted' : 'hover:bg-muted/60'
                }`}
              >
                <button
                  className="flex-1 truncate text-left"
                  onClick={() => chat.setActiveId(s.id)}
                  title={s.title}
                >
                  {s.title}
                </button>
                <button
                  className="opacity-0 transition-opacity group-hover:opacity-60 hover:opacity-100"
                  onClick={() => chat.remove(s.id)}
                  title="删除"
                >
                  <Trash2 className="h-3.5 w-3.5" />
                </button>
              </div>
            ))}
        </div>
        <p className="px-1 pb-1 text-[11px] leading-snug text-muted-foreground">
          对话保存在本浏览器，不跨设备，清缓存会丢。
        </p>
      </aside>

      {/* 消息区 */}
      <section className="flex min-w-0 flex-1 flex-col">
        {!chat.active ? (
          <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
            {chat.ready ? '新建一个对话开始' : '正在准备…'}
          </div>
        ) : (
          <>
            <div className="mb-2 flex items-center gap-2 text-[12.5px]">
              <span className="text-muted-foreground">模型</span>
              <select
                value={chat.active.model}
                disabled={chat.streaming !== null || chat.models.length === 0}
                onChange={(e) => {
                  const model = e.target.value
                  chat.setActiveId(chat.active!.id)
                  chat.active!.model = model
                }}
                className="h-7 rounded-md border bg-background px-2"
              >
                {chat.models.length === 0 ? (
                  <option>没有可用模型，请先添加凭据</option>
                ) : (
                  chat.models.map((m) => <option key={m}>{m}</option>)
                )}
              </select>
            </div>

            <div className="flex-1 space-y-4 overflow-y-auto pr-2">
              {chat.active.messages.map((m, i) => (
                <div key={i} className="space-y-1">
                  <div
                    className={`whitespace-pre-wrap rounded-lg px-3 py-2 text-[13px] ${
                      m.role === 'user' ? 'bg-muted' : 'bg-background border'
                    }`}
                  >
                    {m.content || (m.error ? '' : '（空回复）')}
                  </div>
                  {m.role === 'assistant' && (
                    <div className="px-1 text-[11px] text-muted-foreground">
                      {formatUsage(m.usage)}
                      {m.aborted && ' · 已中断'}
                    </div>
                  )}
                  {m.error && (
                    <div className="rounded-md bg-destructive/10 px-3 py-2 text-[12px] text-destructive">
                      {m.error}
                    </div>
                  )}
                </div>
              ))}
              {chat.streaming !== null && (
                <div className="whitespace-pre-wrap rounded-lg border bg-background px-3 py-2 text-[13px]">
                  {chat.streaming || '…'}
                </div>
              )}
              <div ref={bottomRef} />
            </div>

            <form onSubmit={submit} className="mt-3 flex items-end gap-2">
              <textarea
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' && !e.shiftKey) submit(e)
                }}
                rows={2}
                placeholder="Enter 发送，Shift+Enter 换行"
                className="flex-1 resize-none rounded-md border bg-background px-3 py-2 text-[13px]"
              />
              {chat.streaming !== null ? (
                <Button type="button" variant="outline" onClick={chat.stop}>
                  <Square className="h-3.5 w-3.5" />
                  停止
                </Button>
              ) : (
                <Button type="submit" disabled={!draft.trim()}>
                  <Send className="h-3.5 w-3.5" />
                  发送
                </Button>
              )}
            </form>
          </>
        )}
      </section>
    </div>
  )
}
