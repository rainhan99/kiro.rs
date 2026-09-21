/**
 * 对话页的类型。
 *
 * 用量字段一律 `number | null`，**`null` 表示「不知道」**。
 *
 * 这不是风格问题。后端只有收到上游的 `meteringEvent` 才会写
 * `credit_usage`（`stream.rs:1306-1308`），没有就是整个字段缺席。
 * 把缺席写成 0 就等于宣称「这轮没花钱」——那是一句谎话，而且是关于钱的。
 */
export type TurnUsage = {
  inputTokens: number | null
  outputTokens: number | null
  cacheCreationTokens: number | null
  cacheReadTokens: number | null
  /** 上游报的计费量。`null` = 未报，`0` = 明确报了零。两者不能混。 */
  credits: number | null
  creditUnit: string | null
  creditUnitPlural: string | null
}

export type StreamedTurn = {
  text: string
  usage: TurnUsage
  stopReason: string | null
  /** 用户中断了。已收到的文本与用量照常保留——那一轮可能已经花了钱。 */
  aborted: boolean
  /** 上游报的错误。`null` 表示这一轮正常收场。 */
  error: string | null
}

export type ChatMessage = {
  role: 'user' | 'assistant'
  content: string
  /** 仅 assistant 消息有。 */
  usage?: TurnUsage
  aborted?: boolean
  error?: string | null
}

export type ChatSession = {
  id: string
  title: string
  model: string
  messages: ChatMessage[]
  updatedAt: number
}
