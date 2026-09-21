import type { TurnUsage, StreamedTurn } from '@/types/chat'

/**
 * Anthropic SSE 的增量解析。
 *
 * 纯函数、不碰 DOM、不碰网络——为的是能把「用量缺失就是未知」这条契约
 * 钉死。它在组件里写就只能靠肉眼，而肉眼分不出 `?? 0` 和 `?? null`。
 */

export type Accumulator = {
  buffer: string
  text: string
  usage: TurnUsage
  stopReason: string | null
  error: string | null
}

const unknownUsage = (): TurnUsage => ({
  inputTokens: null,
  outputTokens: null,
  cacheCreationTokens: null,
  cacheReadTokens: null,
  credits: null,
  creditUnit: null,
  creditUnitPlural: null,
})

export const createAccumulator = (): Accumulator => ({
  buffer: '',
  text: '',
  usage: unknownUsage(),
  stopReason: null,
  error: null,
})

/**
 * 把一帧 usage 合并进累积值。
 *
 * **只在字段真的出现时才写**。缺席保持 null——那是「未知」，不是 0。
 * 也因此 `message_delta`（输出侧）不会把 `message_start`（输入侧）
 * 已经报过的值抹掉。
 */
function mergeUsage(raw: Record<string, unknown> | undefined, into: TurnUsage): void {
  if (!raw || typeof raw !== 'object') return

  const num = (k: string) => (typeof raw[k] === 'number' ? (raw[k] as number) : undefined)
  const str = (k: string) => (typeof raw[k] === 'string' ? (raw[k] as string) : undefined)
  const put = <K extends keyof TurnUsage>(k: K, v: TurnUsage[K] | undefined) => {
    if (v !== undefined) into[k] = v
  }

  put('inputTokens', num('input_tokens'))
  put('outputTokens', num('output_tokens'))
  put('cacheCreationTokens', num('cache_creation_input_tokens'))
  put('cacheReadTokens', num('cache_read_input_tokens'))
  put('credits', num('credit_usage'))
  put('creditUnit', str('credit_unit'))
  put('creditUnitPlural', str('credit_unit_plural'))
}

/** 喂一段字节。可以是任意切分——事件边界由内部缓冲负责。 */
export function feed(acc: Accumulator, chunk: string): void {
  acc.buffer += chunk

  let sep: number
  while ((sep = acc.buffer.indexOf('\n\n')) !== -1) {
    const block = acc.buffer.slice(0, sep)
    acc.buffer = acc.buffer.slice(sep + 2)

    const line = block.split('\n').find((l) => l.startsWith('data:'))
    if (!line) continue // 心跳、注释行

    let event: Record<string, any>
    try {
      event = JSON.parse(line.slice(5).trim())
    } catch {
      // 坏帧跳过。上游偶尔夹带心跳或半个 JSON，整轮不该因此崩掉。
      continue
    }

    switch (event.type) {
      case 'content_block_delta':
        // 只收 text_delta。thinking 一期不展示，但更要紧的是它绝不能
        // 当成回答的一部分混进正文。
        if (event.delta?.type === 'text_delta' && typeof event.delta.text === 'string') {
          acc.text += event.delta.text
        }
        break
      case 'message_start':
        mergeUsage(event.message?.usage, acc.usage)
        break
      case 'message_delta':
        mergeUsage(event.usage, acc.usage)
        if (typeof event.delta?.stop_reason === 'string') {
          acc.stopReason = event.delta.stop_reason
        }
        break
      case 'error':
        // 上游报错不能静静收场——那会让人以为模型就回了这么点东西。
        acc.error = event.error?.message ?? '上游返回了一个错误'
        break
    }
  }
}

export function finish(acc: Accumulator, opts?: { aborted?: boolean }): StreamedTurn {
  return {
    text: acc.text,
    usage: acc.usage,
    stopReason: acc.stopReason,
    aborted: opts?.aborted ?? false,
    error: acc.error,
  }
}
