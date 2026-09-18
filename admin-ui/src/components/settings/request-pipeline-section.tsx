import { useEffect, useState } from 'react'
import { isAxiosError } from 'axios'
import { AlertTriangle, GitBranch, Loader2, RefreshCw, Save } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { useConfirm } from '@/components/ui/confirm-dialog'
import { SettingGroup, SettingReadout, SettingRow, SettingSwitch } from '@/components/console/setting-row'
import { useContextCalibration, useRequestPipeline, useSaveRequestPipeline } from '@/hooks/use-request-pipeline'
import { extractErrorMessage } from '@/lib/utils'
import type { PipelineConfig } from '@/types/request-pipeline'
import { createEditor, flattenConfig, numericFields, receiveEditor, validateDraft } from './request-pipeline-form'
import { calibrationRows } from './calibration-rows'

/// 被动观测只读面板。
function CalibrationObservations() {
  const { data } = useContextCalibration()
  const rows = calibrationRows(data?.observations)
  return (
    <SettingGroup
      title="上下文分母被动观测"
      description="仅由正常业务流量得出：原生 tokenUsage 与上下文百分比在同一次响应里同时到达时，两者反推出上游实际使用的分母。没有发送任何探测请求，也不做阈值二分。这是在已见样本上对上游算术的观察，不是实测或公布的上游上限，也不参与准入判定。"
    >
      {rows.length === 0
        ? <p className="text-xs text-muted-foreground">暂无观测。样本只在响应同时给出完整原生用量和可用百分比时产生；100% 可能是钳位值，一律丢弃。</p>
        : (
          <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
            {rows.map((row) => (
              <div key={`${row.model}/${row.endpoint}`} className="contents">
                <dt className="text-muted-foreground">{row.model} · {row.endpoint}</dt>
                <dd className="font-mono">
                  {row.window}
                  <span className="ml-2 font-sans text-muted-foreground">{row.samples} 个样本</span>
                  {row.unstable && <span className="ml-2 font-sans text-destructive">跨度大，均值不可当作窗口</span>}
                  {row.thin && <span className="ml-2 font-sans text-muted-foreground">样本偏少，仅供参考</span>}
                </dd>
              </div>
            ))}
          </dl>
        )}
    </SettingGroup>
  )
}
import type { PipelineEditor } from './request-pipeline-form'

const choices: Record<string, { value: string; label: string }[]> = {
  mode: [{ value: 'off', label: '关闭' }, { value: 'audit', label: '审计' }, { value: 'enforce', label: '强制执行' }],
  cacheStrategy: [{ value: 'off', label: '关闭' }, { value: 'static-prefix', label: '静态前缀（实验性）' }],
  agentMode: [{ value: 'vibe', label: 'Vibe' }, { value: 'spec', label: 'Spec' }],
  'images.strategy': [{ value: 'preserve', label: '保留原图' }, { value: 'lossless-tiles', label: '无损切片' }],
  'toolResults.strategy': [{ value: 'join', label: '合并为单条目（默认）' }, { value: 'lossless-chunks', label: '无损分片（接受性未验证）' }],
  admission: [{ value: 'off', label: '不拦截（默认）' }, { value: 'declared-ceiling', label: '按上游声明上限拦截' }],
  recovery: [{ value: 'off', label: '不重试（默认）' }, { value: 'lossless-retry', label: '无损修正后重发一次' }],
}
const labels: Record<string, string> = {
  mode: '执行模式', stripBillingHeader: '移除计费标记头', cacheStrategy: '缓存策略', agentMode: '代理模式',
  'artifacts.enabled': '启用原文分页读取', 'images.strategy': '图片策略',
  'toolResults.strategy': '工具结果线上形状', admission: '发送前 token 准入',
  recovery: '长度拒绝后的恢复', auditEnabled: '记录管线审计',
  kiroOnly: '仅使用 Kiro', allowSimulatedCache: '允许模拟缓存',
  ...Object.fromEntries(numericFields.map((field) => [field.key, field.label])),
}

function displayValue(key: string, value: string | boolean | undefined) {
  if (value === undefined) return '不可用'
  if (value === '') return '未设上限'
  if (typeof value === 'boolean') return value ? '开启' : '关闭'
  const option = choices[key]?.find((choice) => choice.value === value)
  if (option) return option.label
  const field = numericFields.find((field) => field.key === key)
  return field ? `${Number(value).toLocaleString()} ${field.unit}` : value
}

function ConfigComparison({ effective, saved }: { effective: PipelineConfig; saved: PipelineConfig | null }) {
  const current = flattenConfig(effective)
  const stored = saved ? flattenConfig(saved) : null
  return (
    <details className="rounded-lg border p-3">
      <summary className="cursor-pointer text-sm font-medium">查看当前生效 / 已保存配置对照</summary>
      <div className="mt-3 overflow-x-auto">
        <table className="w-full min-w-[420px] text-left text-xs">
          <thead><tr className="border-b"><th className="py-2 pr-3">配置项</th><th className="py-2 pr-3">当前生效（启动时）</th><th className="py-2">已保存（磁盘）</th></tr></thead>
          <tbody>{Object.entries(labels).map(([key, label]) => (
            <tr key={key} className="border-b border-border/40 last:border-0">
              <th className="py-2 pr-3 font-normal">{label}</th>
              <td className="py-2 pr-3 tabular-nums">{displayValue(key, current[key])}</td>
              <td className={`py-2 tabular-nums ${stored && stored[key] !== current[key] ? 'text-amber-600 dark:text-amber-400' : ''}`}>{stored ? displayValue(key, stored[key]) : '不可用'}</td>
            </tr>
          ))}</tbody>
        </table>
      </div>
    </details>
  )
}

export function RequestPipelineSection() {
  const query = useRequestPipeline()
  const save = useSaveRequestPipeline()
  const confirm = useConfirm()
  const [editor, setEditor] = useState<PipelineEditor | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [savedMessage, setSavedMessage] = useState<string | null>(null)
  const [reloading, setReloading] = useState(false)

  useEffect(() => {
    if (query.data) setEditor((previous) => receiveEditor(previous, query.data!))
  }, [query.data])

  const data = query.data
  const dirty = !!editor && (editor.safetyAdjusted || JSON.stringify(editor.draft) !== JSON.stringify(editor.baseline))
  const validation = editor ? validateDraft(editor.draft) : { errors: {} }
  const busy = save.isPending || reloading
  const editable = !!data?.editable && editor?.revision != null
  const changedOnDisk = !!data && !!editor && data.revision !== editor.revision

  const change = (key: string, value: string | boolean) => {
    setEditor((previous) => previous && ({ ...previous, draft: { ...previous.draft, [key]: value } }))
    setSavedMessage(null)
  }

  const reload = async () => {
    if (dirty && !await confirm({ title: '放弃未保存的修改？', description: '重新读取已保存配置会替换此页草稿。当前运行中的配置不会改变。', confirmText: '放弃并重新读取' })) return
    setReloading(true)
    const result = await query.refetch()
    if (result.isError || !result.data) setError(`读取失败，草稿已保留：${extractErrorMessage(result.error)}`)
    else {
      setEditor((previous) => receiveEditor(previous, result.data!, true))
      setError(null)
      setSavedMessage(null)
    }
    setReloading(false)
  }

  const submit = async () => {
    if (!editor?.revision || !validation.config || !editable || busy) return
    setSavedMessage(null)
    try {
      const next = await save.mutateAsync({ config: validation.config, revision: editor.revision })
      setEditor(createEditor(next))
      setError(null)
      setSavedMessage(next.restartRequired ? '已保存到配置文件。重启服务后生效；当前运行配置未变。' : '已保存；配置与当前运行值一致，无需重启。')
    } catch (cause) {
      setError(isAxiosError(cause) && cause.response?.status === 409
        ? '保存冲突：配置已被其他操作修改。草稿已保留，请先记录需要保留的改动，再重新读取已保存配置。'
        : `保存失败，草稿已保留：${extractErrorMessage(cause)}`)
    }
  }

  if (!data || !editor) return (
    <div className="space-y-3 py-6 text-sm" role="status">
      {query.isLoading ? <span className="flex items-center gap-2"><Loader2 className="size-4 animate-spin" />正在读取请求管线配置…</span> : <><p role="alert">读取失败：{extractErrorMessage(query.error)}</p><Button variant="outline" onClick={() => void query.refetch()}>重试</Button></>}
    </div>
  )

  const select = (key: string, hint: string) => (
    <SettingRow label={labels[key]} hint={hint}>
      <select aria-label={labels[key]} value={String(editor.draft[key])} onChange={(event) => change(key, event.target.value)} className="h-9 max-w-full rounded-md border border-input bg-background px-3 text-sm">
        {choices[key].map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}
      </select>
    </SettingRow>
  )
  const toggle = (key: string, hint: string) => <SettingSwitch label={labels[key]} hint={hint} checked={editor.draft[key] === true} onChange={(value) => change(key, value)} disabled={!editable || busy} />
  const numbers = (keys: string[]) => numericFields.filter((field) => keys.includes(field.key)).map((field) => {
    const issue = validation.errors[field.key]
    const id = `pipeline-${field.key.replace(/\./g, '-')}`
    return (
      <SettingRow key={field.key} label={field.label} hint={`${field.min.toLocaleString()}–${field.max.toLocaleString()} ${field.unit}${'optional' in field ? '；留空停用此检查，0 无效' : ''}`}>
        <div className="w-full space-y-1 sm:w-60">
          <div className="flex items-center gap-2">
            <Input id={id} aria-label={field.label} type="text" inputMode="numeric" value={String(editor.draft[field.key] ?? '')} onChange={(event) => change(field.key, event.target.value)} aria-invalid={!!issue} aria-describedby={issue ? `${id}-error` : undefined} placeholder={'optional' in field ? '留空停用' : undefined} className="h-9 min-w-0 text-right tabular-nums" />
            <span className="shrink-0 text-xs text-muted-foreground">{field.unit}</span>
          </div>
          {issue && <p id={`${id}-error`} className="text-xs text-destructive">{issue}</p>}
        </div>
      </SettingRow>
    )
  })

  return (
    <div className="space-y-6">
      <div className="space-y-3">
        <div className="flex flex-wrap items-center gap-2"><GitBranch className="size-4" /><h2 className="font-semibold">请求管线</h2><Badge variant="outline">重启生效</Badge>{dirty && <Badge variant="secondary">草稿未保存</Badge>}</div>
        <p className="text-xs leading-relaxed text-muted-foreground">表单修改仅保留在此页，点击保存后写入配置文件。服务启动时加载配置，本页不执行热更新、自动重启或上游探测。</p>
        <div className="grid gap-3 sm:grid-cols-2">
          <div className="rounded-lg border bg-muted/20 p-3"><p className="text-xs text-muted-foreground">当前生效 · 启动时配置</p><p className="mt-1 text-sm font-medium">执行模式：{displayValue('mode', data.effectiveConfig.mode)}</p><p className="mt-1 text-xs text-muted-foreground">保存不会改变当前进程行为</p></div>
          <div className="rounded-lg border bg-muted/20 p-3"><p className="text-xs text-muted-foreground">已保存 · 配置文件</p><p className="mt-1 text-sm font-medium">{data.savedConfig ? `执行模式：${displayValue('mode', data.savedConfig.mode)}` : '无可编辑的配置文件'}</p><p className="mt-1 text-xs text-muted-foreground">{data.restartRequired ? '已保存值与当前生效值不同，等待重启' : data.savedConfig ? '与当前生效值一致' : '仅展示当前启动配置'}</p></div>
        </div>
        {data.restartRequired && <p className="flex items-start gap-2 rounded-md border border-amber-500/40 bg-amber-500/5 p-3 text-sm"><AlertTriangle className="mt-0.5 size-4 shrink-0" />待重启：配置已保存，尚未在当前进程中生效。</p>}
        {!editable && <p role="status" className="rounded-md border p-3 text-sm">当前服务没有可写入的已知配置文件，本页为只读。请通过服务启动配置管理此项。</p>}
        {query.isError && <p role="alert" className="text-sm text-destructive">刷新失败；展示上次读取值，草稿已保留。{extractErrorMessage(query.error)}</p>}
        {changedOnDisk && <p role="alert" className="text-sm text-amber-600 dark:text-amber-400">配置版本已变化，草稿未被覆盖。请重新读取后再保存。</p>}
        {editor.safetyAdjusted && <p role="alert" className="rounded-md border border-amber-500/40 p-3 text-sm">读取的配置允许非 Kiro 路由或模拟缓存。本编辑器的草稿已明确调整为“仅使用 Kiro”和“禁止模拟缓存”；保存会一并写入这两项约束。实际已保存值与生效值可在下方对照中查看。</p>}
        <ConfigComparison effective={data.effectiveConfig} saved={data.savedConfig} />
      </div>

      <form onSubmit={(event) => { event.preventDefault(); void submit() }} className="space-y-6">
        <fieldset disabled={!editable || busy} className="min-w-0 space-y-7 disabled:opacity-65">
          <legend className="mb-3 text-sm font-semibold">{editable ? '编辑下次启动配置' : '配置只读预览'}</legend>
          <SettingGroup title="执行与审计" description="各开关只修改草稿，统一通过下方保存按钮提交。">
            {select('mode', '强制执行会执行管线转换并拒绝超出已配置预算的请求。审计 / 关闭保留原有请求转换，不执行计费标记清理、原文转存、图片切片或 cachePoint 标记，也不强制拒绝这些预算违规。入口上限始终适用，审计记录由独立开关控制。')}
            {select('agentMode', '上游请求使用的代理模式。')}
            {toggle('stripBillingHeader', '仅移除首个 system 文本块开头的 x-anthropic-billing-header: 行；不是任意 HTTP 请求头清理。')}
            {toggle('auditEnabled', '记录管线处理证据；原生缓存 usage 与本地估算分别展示。')}
            <SettingReadout label="仅使用 Kiro" hint="本编辑器固定启用，不切换其他提供商。">开启（固定）</SettingReadout>
            <SettingReadout label="模拟缓存" hint="本编辑器固定禁用，不把本地估算标为原生缓存 usage。">关闭（固定）</SettingReadout>
          </SettingGroup>

          <SettingGroup title="缓存策略" description="cachePoint 为实验性请求标记，不保证 Kiro 接受或实际命中。是否有原生缓存 usage，以请求日志中上游实际返回的证据为准。">
            {select('cacheStrategy', '静态前缀策略尝试插入缓存标记；启用本身不能证明原生缓存生效。')}
          </SettingGroup>

          <SettingGroup title="本地字节预算" description="这些是网关本地保护阈值，不是 Kiro 官方公布的限制。入口上限始终适用；其余检查可留空停用，且不能大于入口上限。">
            {numbers(['ingressMaxBytes', 'limits.bodyBytes', 'limits.textFieldBytes', 'limits.toolResultBytes', 'limits.imageBase64Bytes'])}
          </SettingGroup>

          <SettingGroup title="原文存储与分页读取" description="仅转存历史用户文本和工具结果文本，不转存当前指令、系统提示、工具 schema、推理或工具输入。原文保存在当前进程内存中，由内部工具分页读取；保留可检索性，不保证与一次性输入全文有相同推理效果。有效期结束、容量淘汰或进程重启后可能不可用，这不是持久化记忆。转存阈值 ≤ 单个内容上限 ≤ 内存存储总上限。">
            {toggle('artifacts.enabled', '启用后，达到阈值的历史用户文本和工具结果文本可转为引用，通过内部读取轮次获取原文。')}
            {numbers(numericFields.filter((field) => field.key.startsWith('artifacts.')).map((field) => field.key))}
          </SettingGroup>

          <CalibrationObservations />

          <SettingGroup title="发送前准入与拒绝后恢复" description="两者都默认关闭，因为它们会改变请求的实际走向。准入按上游声明的 maxInputTokens 在发送前拒绝：判据是本地启发式估算而不是上游自己的计数，因此可能拒掉上游本来会接受的请求；上游没有声明上限时不拦截——未知不是无限也不是零。写死的模型名窗口表永远不会被当作上限。恢复只在拒绝分类明确指向长度预算时触发，对 payload 施加一次无损修正后同模型重发一次，没有第三次、不换补救手段、不换账号；它不改变正常请求的形状，修正只作用于这一次重试；修正后字节毫无变化时不重发。">
            {select('admission', '按上游声明的 maxInputTokens 在发送前拒绝。判据是估算，可能误拦。')}
            {select('recovery', '长度拒绝后施加一次无损修正并重发一次，同模型。接受性未验证的形状只在上游已拒绝常规形状之后发出。')}
          </SettingGroup>

          <SettingGroup title="工具结果线上形状" description="默认把工具结果的所有片段合并成单个 text 条目，与既有行为一致。无损分片把超过分片上限的正文切成多个 text 条目：逐字节可还原、切点落在 UTF-8 字符边界、与 tool_use_id 的配对不变，全文照发，既不是转存也不是摘要。上游 content 字段本就是数组，但本项目从未向上游发送过多于一个条目的载荷，也不允许为探测而发试探流量，因此上游是否接受未经验证；开启后若出现 400，请改回合并，不会自动改形或重试。可先用离线检查验收形状。">
            {select('toolResults.strategy', '合并为单条目，或把超过分片上限的工具结果切成多个无损条目。启用前建议先用 --inspect-request 确认线上形状。')}
            {numbers(numericFields.filter((field) => field.key.startsWith('toolResults.')).map((field) => field.key))}
          </SettingGroup>

          <SettingGroup title="图片处理" description="无损切片保留解码后的像素，不保证与原压缩文件字节相同。切片、解码像素和 Base64 字节均受本地预算约束；启用切片时单块预算不能超过入口上限。">
            {select('images.strategy', '保留原图，或将支持的静态 PNG / JPEG / WebP 图片处理为无损切片；切片模式不支持动画或远程图片读取。')}
            {numbers(numericFields.filter((field) => field.key.startsWith('images.')).map((field) => field.key))}
          </SettingGroup>
        </fieldset>

        <div className="space-y-3 border-t pt-4">
          {Object.keys(validation.errors).length > 0 && <p role="alert" className="text-sm text-destructive">有 {Object.keys(validation.errors).length} 项配置无效，请修正上方标注后保存。</p>}
          {error && <p role="alert" className="rounded-md border border-destructive/40 p-3 text-sm text-destructive">{error}</p>}
          {savedMessage && <p role="status" className="rounded-md border border-emerald-500/40 p-3 text-sm">{savedMessage}</p>}
          <div className="flex flex-wrap gap-2">
            <Button type="submit" disabled={!editable || busy || !dirty || !validation.config || changedOnDisk}>{save.isPending ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}保存配置（重启生效）</Button>
            <Button type="button" variant="outline" disabled={busy} onClick={() => void reload()}><RefreshCw className={`size-4 ${reloading ? 'animate-spin' : ''}`} />{dirty ? '放弃草稿并重新读取' : '重新读取已保存配置'}</Button>
          </div>
          <p className="text-xs text-muted-foreground">离开本分区会丢弃未保存草稿。需要撤回待重启配置时，请参考生效值修改草稿并保存。</p>
        </div>
      </form>
    </div>
  )
}
