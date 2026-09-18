import { useEffect, useMemo, useState } from 'react'
import { isAxiosError } from 'axios'
import { AlertTriangle, Loader2, Plus, RefreshCw, Save, Trash2, Waypoints } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { SettingGroup, SettingReadout, SettingRow, SettingSwitch } from '@/components/console/setting-row'
import { useGatewayConfig, usePreviewRoute, useSaveGatewayConfig } from '@/hooks/use-gateway'
import type { BillingUnit, RoutingMode, UpstreamKind } from '@/types/gateway'
import {
  createEditor,
  emptyBinding,
  emptyUpstream,
  receiveEditor,
  validateDraft,
  type BindingDraft,
  type GatewayEditor,
  type PricesDraft,
  type UpstreamDraft,
} from './gateway-form'

const KINDS: { value: UpstreamKind; label: string }[] = [
  { value: 'kiro', label: 'Kiro（既有凭据池）' },
  { value: 'anthropic', label: 'Anthropic' },
  { value: 'openai_chat', label: 'OpenAI Chat Completions' },
  { value: 'openai_responses', label: 'OpenAI Responses' },
]
const UNITS: { value: BillingUnit; label: string }[] = [
  { value: 'kiroCredit', label: '原生积分' },
  { value: 'CNY', label: '人民币' },
  { value: 'USD', label: '美元' },
]

function Field({
  label,
  value,
  onChange,
  error,
  placeholder,
}: {
  label: string
  value: string
  onChange: (next: string) => void
  error?: string
  placeholder?: string
}) {
  return (
    <SettingRow label={label} hint={error ? <span className="text-destructive">{error}</span> : undefined}>
      <Input
        value={value}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        className={error ? 'border-destructive' : undefined}
      />
    </SettingRow>
  )
}

function Choice<T extends string>({
  label,
  value,
  options,
  onChange,
  hint,
}: {
  label: string
  value: T
  options: { value: T; label: string }[]
  onChange: (next: T) => void
  hint?: string
}) {
  return (
    <SettingRow label={label} hint={hint}>
      <select
        value={value}
        onChange={(e) => onChange(e.target.value as T)}
        className="h-9 w-full rounded-md border border-input bg-background px-3 text-sm"
      >
        {options.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </select>
    </SettingRow>
  )
}

/** 价目表。**每一项都是十进制字符串**，从不经过浮点。 */
function PriceTable({
  title,
  prices,
  path,
  errors,
  onChange,
}: {
  title: string
  prices: PricesDraft
  path: string
  errors: Record<string, string>
  onChange: (next: PricesDraft) => void
}) {
  const set = (key: keyof PricesDraft) => (value: string) => onChange({ ...prices, [key]: value })
  return (
    <SettingGroup title={title} description="每百万 token 的价格，十进制填写；不接受科学计数法。">
      <Field label="输入" value={prices.input} onChange={set('input')} error={errors[`${path}.input`]} />
      <Field label="输出" value={prices.output} onChange={set('output')} error={errors[`${path}.output`]} />
      <Field label="缓存读取" value={prices.cacheRead} onChange={set('cacheRead')} error={errors[`${path}.cacheRead`]} />
      <Field label="缓存写入" value={prices.cacheWrite} onChange={set('cacheWrite')} error={errors[`${path}.cacheWrite`]} />
      <Field
        label="1 小时缓存写入"
        value={prices.cacheWrite1h}
        onChange={set('cacheWrite1h')}
        error={errors[`${path}.cacheWrite1h`]}
        placeholder="留空表示上游不提供这一档"
      />
      {errors[`${path}.currency`] && (
        <p className="text-xs text-destructive">{errors[`${path}.currency`]}</p>
      )}
    </SettingGroup>
  )
}

function UpstreamCard({
  upstream,
  index,
  errors,
  onChange,
  onRemove,
}: {
  upstream: UpstreamDraft
  index: number
  errors: Record<string, string>
  onChange: (next: UpstreamDraft) => void
  onRemove: () => void
}) {
  const path = `upstreams.${index}`
  const set = <K extends keyof UpstreamDraft>(key: K) => (value: UpstreamDraft[K]) =>
    onChange({ ...upstream, [key]: value })
  return (
    <div className="rounded-lg border p-3 space-y-1">
      <div className="flex items-center justify-between">
        <span className="font-mono text-xs text-muted-foreground">上游 #{index + 1}</span>
        <Button variant="ghost" size="sm" onClick={onRemove}>
          <Trash2 className="h-3.5 w-3.5" />
        </Button>
      </div>
      <Field label="id" value={upstream.id} onChange={set('id')} error={errors[`${path}.id`]} />
      <Field label="名称" value={upstream.name} onChange={set('name')} />
      <Choice label="类型" value={upstream.kind} options={KINDS} onChange={set('kind')} />
      <SettingSwitch label="启用" checked={upstream.enabled} onChange={set('enabled')} />
      <Field label="权重" value={upstream.weight} onChange={set('weight')} error={errors[`${path}.weight`]} />
      {upstream.kind === 'kiro' ? (
        <Field
          label="凭据分组"
          value={upstream.kiroGroup}
          onChange={set('kiroGroup')}
          error={errors[`${path}.kiroGroup`]}
          placeholder="留空表示不限"
        />
      ) : (
        <>
          <Field label="baseUrl" value={upstream.baseUrl} onChange={set('baseUrl')} error={errors[`${path}.baseUrl`]} />
          <SettingRow
            label="API 密钥"
            hint={
              upstream.hasApiKey && !upstream.apiKey && !upstream.clearApiKey
                ? '已配置。留空表示沿用原值——界面从不显示也从不回传真实密钥。'
                : '填写以替换'
            }
          >
            <Input
              type="password"
              value={upstream.apiKey ?? ''}
              placeholder={upstream.hasApiKey ? '已配置（留空沿用）' : '未配置'}
              onChange={(e) => onChange({ ...upstream, apiKey: e.target.value, clearApiKey: false })}
            />
          </SettingRow>
          {upstream.hasApiKey && (
            <SettingSwitch
              label="清除已配置的密钥"
              hint="打开后保存会把这个上游的密钥清空"
              checked={upstream.clearApiKey ?? false}
              onChange={(v) => onChange({ ...upstream, clearApiKey: v, apiKey: undefined })}
            />
          )}
          <SettingSwitch
            label="允许私网地址"
            hint="默认拒绝私网、回环与元数据地址。只有确实要连内网上游时才打开。"
            checked={upstream.allowPrivateNetwork}
            onChange={set('allowPrivateNetwork')}
          />
        </>
      )}
      {errors[`${path}.apiKey`] && <p className="text-xs text-destructive">{errors[`${path}.apiKey`]}</p>}
    </div>
  )
}

function BindingCard({
  binding,
  path,
  errors,
  onChange,
  onRemove,
}: {
  binding: BindingDraft
  path: string
  errors: Record<string, string>
  onChange: (next: BindingDraft) => void
  onRemove: () => void
}) {
  const set = <K extends keyof BindingDraft>(key: K) => (value: BindingDraft[K]) =>
    onChange({ ...binding, [key]: value })
  const monetary = binding.billingUnit !== 'kiroCredit'
  return (
    <div className="rounded-lg border p-3 space-y-1">
      <div className="flex items-center justify-between">
        <span className="font-mono text-xs text-muted-foreground">绑定 {binding.id || '（未命名）'}</span>
        <Button variant="ghost" size="sm" onClick={onRemove}>
          <Trash2 className="h-3.5 w-3.5" />
        </Button>
      </div>
      <Field label="id" value={binding.id} onChange={set('id')} error={errors[`${path}.id`]} />
      <Field label="上游 id" value={binding.upstreamId} onChange={set('upstreamId')} error={errors[`${path}.upstreamId`]} />
      <Field
        label="上游真实模型"
        value={binding.upstreamModel}
        onChange={set('upstreamModel')}
        error={errors[`${path}.upstreamModel`]}
      />
      <SettingSwitch label="启用" checked={binding.enabled} onChange={set('enabled')} />
      <Field
        label="优先级层"
        value={binding.priorityTier}
        onChange={set('priorityTier')}
        error={errors[`${path}.priorityTier`]}
      />
      <Field label="权重" value={binding.weight} onChange={set('weight')} error={errors[`${path}.weight`]} />
      <Field
        label="上下文窗口"
        value={binding.contextWindow}
        onChange={set('contextWindow')}
        error={errors[`${path}.contextWindow`]}
      />
      <Field
        label="输出上限"
        value={binding.maxOutputTokens}
        onChange={set('maxOutputTokens')}
        error={errors[`${path}.maxOutputTokens`]}
      />
      <SettingSwitch label="支持工具" checked={binding.supportsTools} onChange={set('supportsTools')} />
      <SettingSwitch label="支持图片" checked={binding.supportsImages} onChange={set('supportsImages')} />
      <SettingSwitch label="支持推理" checked={binding.supportsReasoning} onChange={set('supportsReasoning')} />
      <Choice
        label="计费单位"
        value={binding.billingUnit}
        options={UNITS}
        hint={
          monetary
            ? '按钱计费必须填写售价；成本价可选，用于核算利润。'
            : '原生积分的成本以上游下发的计量为准，不填价目表。'
        }
        onChange={(unit) =>
          onChange({
            ...binding,
            billingUnit: unit,
            costPrices: unit === 'kiroCredit' ? null : binding.costPrices,
            sellPrices: unit === 'kiroCredit' ? null : binding.sellPrices,
          })
        }
      />
      {errors[`${path}.billingUnit`] && <p className="text-xs text-destructive">{errors[`${path}.billingUnit`]}</p>}
      {monetary && binding.costPrices && (
        <PriceTable
          title="成本价（付给上游）"
          prices={binding.costPrices}
          path={`${path}.costPrices`}
          errors={errors}
          onChange={set('costPrices')}
        />
      )}
      {monetary && binding.sellPrices && (
        <PriceTable
          title="售价（向客户收取）"
          prices={binding.sellPrices}
          path={`${path}.sellPrices`}
          errors={errors}
          onChange={set('sellPrices')}
        />
      )}
      {errors[`${path}.sellPrices`] && <p className="text-xs text-destructive">{errors[`${path}.sellPrices`]}</p>}
    </div>
  )
}

/** 路由预览：只问「会走哪条路」，不预留、不发请求。 */
function RoutePreview({ models }: { models: string[] }) {
  const [keyId, setKeyId] = useState('1')
  const [model, setModel] = useState(models[0] ?? '')
  const preview = usePreviewRoute()
  return (
    <SettingGroup
      title="路由预览"
      description="按当前已保存的配置推演：这个 Key 请求这个别名会落到哪条路。只做判定，不预留额度，也不向上游发任何请求。"
    >
      <SettingRow label="Key id">
        <Input value={keyId} onChange={(e) => setKeyId(e.target.value)} />
      </SettingRow>
      <SettingRow label="模型别名">
        <Input value={model} onChange={(e) => setModel(e.target.value)} />
      </SettingRow>
      <Button
        size="sm"
        variant="outline"
        disabled={preview.isPending}
        onClick={() => preview.mutate({ keyId: Number(keyId) || 0, model })}
      >
        {preview.isPending ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Waypoints className="h-3.5 w-3.5" />}
        推演
      </Button>
      {preview.data && (
        <div className="space-y-1 text-xs">
          {!preview.data.managed ? (
            <p className="text-muted-foreground">网关不接管这个别名，请求会原样走既有 Kiro 路径。</p>
          ) : (
            <>
              <p>
                模式 <span className="font-mono">{preview.data.mode}</span>，
                {preview.data.selected ? (
                  <>
                    会选中 <span className="font-mono text-primary">{preview.data.selected}</span>
                  </>
                ) : (
                  <span className="text-destructive">没有可用路线</span>
                )}
              </p>
              {preview.data.candidates.map((c) => (
                <div key={c.bindingId} className="flex items-center gap-2 font-mono">
                  <Badge variant={c.eligible ? 'default' : 'outline'}>{c.bindingId}</Badge>
                  <span className="text-muted-foreground">
                    {c.upstreamId} / {c.upstreamModel} / {c.unit}
                  </span>
                  {c.refusal && <span className="text-destructive">{c.refusal}</span>}
                </div>
              ))}
            </>
          )}
        </div>
      )}
    </SettingGroup>
  )
}

export function GatewaySection() {
  const { data, isLoading, error, refetch } = useGatewayConfig()
  const save = useSaveGatewayConfig()
  const [editor, setEditor] = useState<GatewayEditor | null>(null)
  const [saveError, setSaveError] = useState<string | null>(null)

  useEffect(() => {
    if (data) setEditor((current) => receiveEditor(current, data))
  }, [data])

  const validated = useMemo(() => (editor ? validateDraft(editor.draft) : null), [editor])
  const errors = validated?.errors ?? {}

  if (isLoading) return <Loader2 className="h-4 w-4 animate-spin" />
  if (error) {
    const notConfigured = isAxiosError(error) && error.response?.status === 404
    return (
      <SettingGroup
        title="多上游网关"
        description={
          notConfigured
            ? '未配置。在缓存目录下创建 gateway.json 并重启即可启用；未配置时所有请求原样走既有 Kiro 路径，行为与从前完全一致。'
            : '读取配置失败。'
        }
      >
        <Button size="sm" variant="outline" onClick={() => refetch()}>
          <RefreshCw className="h-3.5 w-3.5" />
          重试
        </Button>
      </SettingGroup>
    )
  }
  if (!editor || !data) return null

  const patch = (next: Partial<GatewayEditor['draft']>) =>
    setEditor({ ...editor, draft: { ...editor.draft, ...next } })

  const submit = () => {
    setSaveError(null)
    if (!validated?.config) return
    save.mutate(
      { revision: editor.revision, config: validated.config },
      {
        onError: (e) => {
          const code = isAxiosError(e) ? (e.response?.data as { error?: { code?: string } })?.error?.code : undefined
          setSaveError(
            code === 'configuration_conflict'
              ? '配置在你编辑期间被别人改过。点「放弃本地改动」重新载入，或复制你的改动后再试。'
              : extractError(e),
          )
        },
      },
    )
  }

  return (
    <div className="space-y-4">
      <SettingGroup
        title="多上游网关"
        description="为公开别名配置多条上游路线，并按账本计费。未在这里定义的模型一律原样走既有 Kiro 路径。"
      >
        <SettingReadout label="已保存版本">
          <span className="font-mono">{editor.revision}</span>
        </SettingReadout>
        <SettingReadout label="当前接管的别名">
          {editor.managedModels.length === 0 ? (
            <span className="text-muted-foreground">无（定义了但全部停用的模型不算接管）</span>
          ) : (
            <span className="font-mono">{editor.managedModels.join('、')}</span>
          )}
        </SettingReadout>
        {editor.stale && (
          <p className="flex items-center gap-1.5 text-xs text-amber-600">
            <AlertTriangle className="h-3.5 w-3.5" />
            服务端配置已被改动，而你这里还有未保存的编辑。保存会被拒绝；请先放弃本地改动重新载入。
          </p>
        )}
        <Choice
          label="默认路由模式"
          value={editor.draft.defaultRoutingMode}
          options={[
            { value: 'sticky', label: '会话粘性' },
            { value: 'weighted_random', label: '按权重随机' },
          ]}
          hint="按权重随机会同时关掉 Kiro 自身的会话粘性，否则第一次选中的凭据会接管整个会话。"
          onChange={(v: RoutingMode) => patch({ defaultRoutingMode: v })}
        />
        <Field
          label="会话保持时长（秒）"
          value={editor.draft.affinityTtlSecs}
          onChange={(v) => patch({ affinityTtlSecs: v })}
          error={errors.affinityTtlSecs}
        />
        <Field
          label="最大尝试次数"
          value={editor.draft.maxAttempts}
          onChange={(v) => patch({ maxAttempts: v })}
          error={errors.maxAttempts}
        />
        <Field
          label="请求超时（秒）"
          value={editor.draft.requestTimeoutSecs}
          onChange={(v) => patch({ requestTimeoutSecs: v })}
          error={errors.requestTimeoutSecs}
        />
      </SettingGroup>

      <SettingGroup title="上游" description="Kiro 走既有凭据池，不在这里配地址与密钥；直连上游只接受 HTTPS。">
        {editor.draft.upstreams.map((u, i) => (
          <UpstreamCard
            key={i}
            upstream={u}
            index={i}
            errors={errors}
            onChange={(next) => {
              const upstreams = [...editor.draft.upstreams]
              upstreams[i] = next
              patch({ upstreams })
            }}
            onRemove={() => patch({ upstreams: editor.draft.upstreams.filter((_, j) => j !== i) })}
          />
        ))}
        <Button
          size="sm"
          variant="outline"
          onClick={() =>
            patch({ upstreams: [...editor.draft.upstreams, emptyUpstream(`u${editor.draft.upstreams.length + 1}`)] })
          }
        >
          <Plus className="h-3.5 w-3.5" />
          添加上游
        </Button>
      </SettingGroup>

      <SettingGroup title="公开别名" description="客户端请求的模型名。每个别名下的绑定按优先级层与权重选路。">
        {editor.draft.models.map((m, mi) => (
          <div key={mi} className="rounded-lg border p-3 space-y-1">
            <div className="flex items-center justify-between">
              <span className="font-mono text-xs text-muted-foreground">别名 {m.id || '（未命名）'}</span>
              <Button
                variant="ghost"
                size="sm"
                onClick={() => patch({ models: editor.draft.models.filter((_, j) => j !== mi) })}
              >
                <Trash2 className="h-3.5 w-3.5" />
              </Button>
            </div>
            <Field
              label="别名"
              value={m.id}
              error={errors[`models.${mi}.id`]}
              onChange={(v) => {
                const models = [...editor.draft.models]
                models[mi] = { ...m, id: v }
                patch({ models })
              }}
            />
            <Choice
              label="路由模式"
              value={m.routingMode}
              options={[
                { value: '', label: '跟随全局默认' },
                { value: 'sticky', label: '会话粘性' },
                { value: 'weighted_random', label: '按权重随机' },
              ]}
              onChange={(v) => {
                const models = [...editor.draft.models]
                models[mi] = { ...m, routingMode: v }
                patch({ models })
              }}
            />
            {m.bindings.map((b, bi) => (
              <BindingCard
                key={bi}
                binding={b}
                path={`models.${mi}.bindings.${bi}`}
                errors={errors}
                onChange={(next) => {
                  const models = [...editor.draft.models]
                  const bindings = [...m.bindings]
                  bindings[bi] = next
                  models[mi] = { ...m, bindings }
                  patch({ models })
                }}
                onRemove={() => {
                  const models = [...editor.draft.models]
                  models[mi] = { ...m, bindings: m.bindings.filter((_, j) => j !== bi) }
                  patch({ models })
                }}
              />
            ))}
            <Button
              size="sm"
              variant="outline"
              onClick={() => {
                const models = [...editor.draft.models]
                models[mi] = {
                  ...m,
                  bindings: [
                    ...m.bindings,
                    emptyBinding(`b${m.bindings.length + 1}`, editor.draft.upstreams[0]?.id ?? ''),
                  ],
                }
                patch({ models })
              }}
            >
              <Plus className="h-3.5 w-3.5" />
              添加绑定
            </Button>
          </div>
        ))}
        <Button
          size="sm"
          variant="outline"
          onClick={() =>
            patch({
              models: [
                ...editor.draft.models,
                { id: '', displayName: '', routingMode: '', affinityTtlSecs: '', bindings: [] },
              ],
            })
          }
        >
          <Plus className="h-3.5 w-3.5" />
          添加别名
        </Button>
      </SettingGroup>

      <RoutePreview models={editor.managedModels} />

      {saveError && (
        <p className="flex items-center gap-1.5 text-xs text-destructive">
          <AlertTriangle className="h-3.5 w-3.5" />
          {saveError}
        </p>
      )}
      <div className="flex gap-2">
        <Button size="sm" disabled={!validated?.config || save.isPending} onClick={submit}>
          {save.isPending ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Save className="h-3.5 w-3.5" />}
          保存
        </Button>
        <Button size="sm" variant="outline" onClick={() => setEditor(createEditor(data))}>
          <RefreshCw className="h-3.5 w-3.5" />
          放弃本地改动
        </Button>
      </div>
      {Object.keys(errors).length > 0 && (
        <p className="text-xs text-destructive">还有 {Object.keys(errors).length} 处需要修正，保存已禁用。</p>
      )}
    </div>
  )
}

function extractError(e: unknown): string {
  if (isAxiosError(e)) {
    const message = (e.response?.data as { error?: { message?: string } })?.error?.message
    if (message) return message
  }
  return String(e)
}
