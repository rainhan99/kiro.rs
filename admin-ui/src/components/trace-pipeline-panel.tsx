import { useTracePipelineEvidence } from '@/hooks/use-traces'
import type { NativeTokenUsage } from '@/types/api'

function objectValue(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null
}

function nativeUsage(value: unknown): NativeTokenUsage | null {
  const record = objectValue(value)
  if (!record) return null
  const counters = ['uncachedInputTokens', 'outputTokens', 'cacheReadInputTokens', 'cacheWriteInputTokens']
  return counters.every((key) => typeof record[key] === 'number'
    && Number.isInteger(record[key]) && record[key] >= 0 && record[key] <= 2_147_483_647)
    ? record as unknown as NativeTokenUsage
    : null
}

function numberLabel(value: unknown): string {
  return typeof value === 'number' && Number.isFinite(value) ? value.toLocaleString('en-US') : '不可用'
}

function WireAudit({ value, index }: { value: Record<string, unknown>; index: number }) {
  const metrics = objectValue(value.metrics)
  const fields: [string, unknown][] = [
    ['完整请求指纹', value.wireFingerprint],
    ['语义请求指纹', value.semanticFingerprint],
    ['静态前缀指纹', value.staticPrefixFingerprint],
    ['配置指纹', value.configFingerprint],
    ['上游 Profile 指纹', value.profileFingerprint],
    ['诊断分组指纹', value.scopeFingerprint],
    ['指纹周期', value.fingerprintEpoch],
  ]
  return (
    <details className="rounded-md border border-border/50 px-3 py-2">
      <summary className="cursor-pointer text-xs">
        请求构造审计 {index + 1} · 请求体 {numberLabel(metrics?.bodyBytes)} 字节
        {' · '}缓存标记 {numberLabel(metrics?.cachePointCount)} 个
      </summary>
      <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-[11px]">
        <dt className="text-muted-foreground">端点</dt>
        <dd>{typeof value.endpoint === 'string' ? value.endpoint : '不可用'}</dd>
        <dt className="text-muted-foreground">凭据 ID / 模型 / Agent Mode</dt>
        <dd>{numberLabel(value.credentialId)} / {typeof value.modelId === 'string' ? value.modelId : '不可用'} / {typeof value.agentMode === 'string' ? value.agentMode : '不可用'}</dd>
        <dt className="text-muted-foreground">阶段</dt>
        <dd>{typeof value.stage === 'string' ? value.stage : '最终 endpoint 构造，尚不证明已发送'}</dd>
        <dt className="text-muted-foreground">头部大小估算</dt>
        <dd>{numberLabel(value.headerBytes)} 字节</dd>
        <dt className="text-muted-foreground">头部名称</dt>
        <dd className="break-all font-mono">
          {Array.isArray(value.headerNames) ? value.headerNames.filter((name) => typeof name === 'string').join(', ') : '不可用'}
        </dd>
        {fields.map(([label, fingerprint]) => (
          <div key={label} className="contents">
            <dt className="text-muted-foreground">{label}</dt>
            <dd className="break-all font-mono">{typeof fingerprint === 'string' ? fingerprint : '不可用'}</dd>
          </div>
        ))}
      </dl>
      {Array.isArray(value.violations) && value.violations.length > 0
        ? <p className="mt-2 break-all text-[11px] text-destructive">本地预算超限：{value.violations.filter((item) => typeof item === 'string').join('; ')}</p>
        : null}
      <p className="mt-2 text-[11px] text-muted-foreground">指纹仅在同一周期内可比较；缓存标记和大小不能证明上游缓存命中。</p>
    </details>
  )
}

export function TracePipelinePanel({ traceId }: { traceId: string }) {
  const { data, isPending, isError } = useTracePipelineEvidence(traceId)
  const native = (data?.evidence ?? [])
    .filter((sample) => sample.kind === 'native_usage')
    .map((sample) => nativeUsage(sample.evidence))
    .filter((sample): sample is NativeTokenUsage => sample !== null)
  const audits = (data?.evidence ?? [])
    .filter((sample) => sample.kind === 'wire_audit')
    .map((sample) => objectValue(sample.evidence))
    .filter((sample): sample is Record<string, unknown> => sample !== null)

  return (
    <div className="space-y-3 rounded-lg border border-border/60 bg-card/60 p-3.5">
      <div className="text-[13px] font-semibold">上游原生用量与发送审计</div>
      {isPending ? <p className="text-xs text-muted-foreground">正在读取证据…</p>
        : isError ? <p className="text-xs text-muted-foreground">证据暂不可用，缓存效果未验证。</p>
          : <>
            {native.length === 0
              ? <p className="text-xs text-muted-foreground">没有完整的原生用量快照，缓存效果未验证。</p>
              : <>
                <p className="text-xs text-muted-foreground">
                  保留的原生快照 {data?.summary.nativeSampleCount} 份 · 缓存读取 {numberLabel(data?.summary.nativeCacheReadInputTokens)}
                  {' · '}缓存写入 {numberLabel(data?.summary.nativeCacheWriteInputTokens)} Token。完整轮次覆盖未知。
                </p>
                <div className="overflow-x-auto">
                  <table className="w-full text-left text-xs tabular-nums">
                    <thead className="text-muted-foreground">
                      <tr>{['原生快照', '未缓存输入', '输出', '缓存读取', '缓存写入'].map((label) => <th key={label} className="px-2 py-1 font-medium">{label}</th>)}</tr>
                    </thead>
                    <tbody>{native.map((usage, index) => (
                      <tr key={index} className="border-t border-border/30 font-mono">
                        {[index + 1, usage.uncachedInputTokens, usage.outputTokens, usage.cacheReadInputTokens, usage.cacheWriteInputTokens]
                          .map((value, column) => <td key={column} className="px-2 py-1">{numberLabel(value)}</td>)}
                      </tr>
                    ))}</tbody>
                  </table>
                </div>
              </>}
            {audits.length > 0 ? audits.map((audit, index) => <WireAudit key={index} value={audit} index={index} />)
              : <p className="text-xs text-muted-foreground">没有保存发送审计，请求大小与指纹不可用。</p>}
          </>}
    </div>
  )
}
