import { describe, expect, test } from 'bun:test'
import { parseModels } from './chat'

describe('模型列表', () => {
  test('读 data 数组，取 id', () => {
    expect(parseModels({ data: [{ id: 'claude-sonnet-4' }, { id: 'claude-opus-4' }] }))
      .toEqual(['claude-sonnet-4', 'claude-opus-4'])
  })

  /// 列表是凭据可见模型的并集，可能为空（一个凭据都没配时）。
  /// 空不是错误——界面要说「没有可用模型，先去添加凭据」，而不是一直转圈。
  test('空列表是合法状态，不抛异常', () => {
    expect(parseModels({ data: [] })).toEqual([])
  })

  test('形状意外时退化成空而不是崩溃', () => {
    expect(parseModels(null)).toEqual([])
    expect(parseModels(undefined)).toEqual([])
    expect(parseModels({ data: 'nope' })).toEqual([])
    expect(parseModels({ data: [{ noId: 1 }, { id: 'ok' }] })).toEqual(['ok'])
  })
})
