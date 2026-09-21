import { describe, expect, test } from 'bun:test'
import { createAccumulator, feed, finish } from './chat-stream'

const acc = () => createAccumulator()

describe('每轮用量：拿不到就是未知', () => {
  /// 后端只有收到 meteringEvent 才会写 credit_usage（stream.rs:1306-1308）。
  /// 没有就是整个字段缺席。前端一个 `?? 0` 就能把这条契约毁掉，
  /// 而且看起来毫无问题——「花了 0 credit」和「不知道花了多少」在界面上
  /// 长得一样，但意思完全相反。
  test('message_delta 没带 credit_usage 时，计费是 null 而不是 0', () => {
    const a = acc()
    feed(a, 'event: message_delta\ndata: {"type":"message_delta","usage":{"output_tokens":12}}\n\n')
    const turn = finish(a)
    expect(turn.usage.outputTokens).toBe(12)
    expect(turn.usage.credits).toBeNull()
    expect(turn.usage.credits).not.toBe(0)
  })

  /// 上游真的报了 0，那是「确实没花钱」，与「不知道」是两回事，必须分得开。
  test('上游明确报 0 时就是 0，不是未知', () => {
    const a = acc()
    feed(a, 'event: message_delta\ndata: {"type":"message_delta","usage":{"credit_usage":0}}\n\n')
    expect(finish(a).usage.credits).toBe(0)
  })

  test('带了 credit_usage 就如实读出来，不经重算', () => {
    const a = acc()
    feed(a, 'event: message_delta\ndata: {"type":"message_delta","usage":' +
      '{"output_tokens":3,"credit_usage":0.0169543708291874,"credit_unit":"credit","credit_unit_plural":"credits"}}\n\n')
    const turn = finish(a)
    expect(turn.usage.credits).toBe(0.0169543708291874)
    expect(turn.usage.creditUnit).toBe('credit')
  })

  test('一个 message_delta 都没收到时，token 与计费都是未知', () => {
    const a = acc()
    feed(a, 'event: content_block_delta\ndata: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}\n\n')
    const turn = finish(a)
    expect(turn.text).toBe('hi')
    expect(turn.usage.outputTokens).toBeNull()
    expect(turn.usage.credits).toBeNull()
  })

  /// message_start 带的是输入侧用量，message_delta 带输出侧。两者都要收，
  /// 后者不该把前者覆盖成 null。
  test('message_start 的输入用量不被后续 delta 抹掉', () => {
    const a = acc()
    feed(a, 'event: message_start\ndata: {"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":40}}}\n\n')
    feed(a, 'event: message_delta\ndata: {"type":"message_delta","usage":{"output_tokens":7}}\n\n')
    const u = finish(a).usage
    expect(u.inputTokens).toBe(100)
    expect(u.cacheReadTokens).toBe(40)
    expect(u.outputTokens).toBe(7)
  })
})

describe('分片与增量', () => {
  test('一个 SSE 事件被拆在两次 feed 之间也能正确解析', () => {
    const a = acc()
    feed(a, 'event: content_block_delta\ndata: {"type":"content_block_del')
    feed(a, 'ta","delta":{"type":"text_delta","text":"你好"}}\n\n')
    expect(finish(a).text).toBe('你好')
  })

  test('一次 feed 里有多个事件', () => {
    const a = acc()
    feed(a,
      'event: content_block_delta\ndata: {"type":"content_block_delta","delta":{"type":"text_delta","text":"一"}}\n\n' +
      'event: content_block_delta\ndata: {"type":"content_block_delta","delta":{"type":"text_delta","text":"二"}}\n\n')
    expect(finish(a).text).toBe('一二')
  })

  /// 坏帧不能让整轮崩掉。上游偶尔会夹带心跳、注释行或半个 JSON。
  test('坏帧被跳过，好帧照常', () => {
    const a = acc()
    feed(a, ': keep-alive\n\n')
    feed(a, 'event: x\ndata: {不是 JSON}\n\n')
    feed(a, 'event: content_block_delta\ndata: {"type":"content_block_delta","delta":{"type":"text_delta","text":"活着"}}\n\n')
    expect(finish(a).text).toBe('活着')
  })

  /// thinking 块不该混进正文。一期不展示它，但也绝不能当成回答的一部分。
  test('thinking 增量不进正文', () => {
    const a = acc()
    feed(a, 'event: content_block_delta\ndata: {"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"内心戏"}}\n\n')
    feed(a, 'event: content_block_delta\ndata: {"type":"content_block_delta","delta":{"type":"text_delta","text":"答案"}}\n\n')
    expect(finish(a).text).toBe('答案')
  })
})

describe('中断与收尾', () => {
  /// 中断后那一轮**可能已经花了钱**。把它表现成「不算」会让界面和账单
  /// 对不上。
  test('中断保留已收到的文本并标注，不丢弃', () => {
    const a = acc()
    feed(a, 'event: content_block_delta\ndata: {"type":"content_block_delta","delta":{"type":"text_delta","text":"写了一半"}}\n\n')
    const turn = finish(a, { aborted: true })
    expect(turn.text).toBe('写了一半')
    expect(turn.aborted).toBe(true)
    expect(turn.usage.credits).toBeNull()
  })

  test('stop_reason 被记下来', () => {
    const a = acc()
    feed(a, 'event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{}}\n\n')
    expect(finish(a).stopReason).toBe('max_tokens')
  })

  /// 上游报错时不能静静地收场——那会让人以为模型就回了这么点东西。
  test('error 事件被记下来', () => {
    const a = acc()
    feed(a, 'event: error\ndata: {"type":"error","error":{"type":"overloaded_error","message":"上游过载"}}\n\n')
    const turn = finish(a)
    expect(turn.error).toContain('上游过载')
  })
})
