import { useState } from 'react'
import { isAxiosError } from 'axios'
import { AlertTriangle, Loader2, RotateCcw, Save } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { SettingGroup, SettingReadout, SettingRow } from '@/components/console/setting-row'
import { useAdjustBudget, useBudgets, useLedgerAudit, useNewCycle, useSaveBudget } from '@/hooks/use-gateway'
import type { BillingUnit, Budget, BudgetEnforcement } from '@/types/gateway'

const UNIT_LABEL: Record<BillingUnit, string> = {
  kiroCredit: '原生积分',
  CNY: '人民币',
  USD: '美元',
}

/** 每个币种单独一行。把积分和钱并在一起显示，会让人把两种东西当成同一种。 */
function BudgetRow({ budget }: { budget: Budget }) {
  const unlimited = budget.limit === null
  const exhausted = !unlimited && budget.available === '0'
  return (
    <div className="rounded-lg border p-3 space-y-1 text-xs">
      <div className="flex items-center gap-2">
        <Badge variant="outline">{UNIT_LABEL[budget.unit]}</Badge>
        <Badge variant={budget.enforcement === 'hard' ? 'default' : 'outline'}>
          {budget.enforcement === 'hard' ? '硬额度' : '软额度'}
        </Badge>
        <span className="text-muted-foreground">周期 {budget.cycle}</span>
        {exhausted && <Badge variant="destructive">已用尽</Badge>}
      </div>
      <div className="grid grid-cols-2 gap-x-4 gap-y-0.5 font-mono sm:grid-cols-4">
        <span className="text-muted-foreground">上限</span>
        <span>{unlimited ? '不限' : budget.limit}</span>
        <span className="text-muted-foreground">已用</span>
        <span>{budget.used}</span>
        <span className="text-muted-foreground">已冻结</span>
        <span>{budget.reserved}</span>
        <span className="text-muted-foreground">剩余</span>
        <span>{unlimited ? '不限' : budget.available}</span>
        <span className="text-muted-foreground">在飞</span>
        <span>{budget.inFlight}</span>
        <span className="text-muted-foreground">待结算</span>
        <span>{budget.customerPending}</span>
      </div>
      {budget.enforcement === 'soft' && !unlimited && exhausted && (
        <p className="flex items-center gap-1.5 text-amber-600">
          <AlertTriangle className="h-3.5 w-3.5" />
          软额度已用尽：新请求会被拒绝，但已在飞的请求不会被中断。
        </p>
      )}
      {budget.customerPending > 0 && (
        <p className="text-muted-foreground">
          有 {budget.customerPending} 笔待结算——上游还没给出可确认的用量。它们既没有被计入已用，也没有被当作没发生。
        </p>
      )}
    </div>
  )
}

function EditBudget({ keyId, existing }: { keyId: number; existing: Budget[] }) {
  const save = useSaveBudget(keyId)
  const [unit, setUnit] = useState<BillingUnit>('CNY')
  const [limit, setLimit] = useState('')
  const [unlimited, setUnlimited] = useState(false)
  const [enforcement, setEnforcement] = useState<BudgetEnforcement>('soft')
  const [maxInFlight, setMaxInFlight] = useState('8')
  const [maxPending, setMaxPending] = useState('16')
  const [error, setError] = useState<string | null>(null)

  const current = existing.find((b) => b.unit === unit)

  return (
    <SettingGroup
      title="设置额度"
      description="只设政策，不动已用量。要改已用量请用下面的「调整」，那是有审计记录的操作。"
    >
      <SettingRow label="币种">
        <select
          value={unit}
          onChange={(e) => {
            const next = e.target.value as BillingUnit
            setUnit(next)
            const found = existing.find((b) => b.unit === next)
            setUnlimited(found ? found.limit === null : false)
            setLimit(found?.limit ?? '')
            setEnforcement(found?.enforcement ?? 'soft')
            setMaxInFlight(String(found?.maxInFlight ?? 8))
            setMaxPending(String(found?.maxPending ?? 16))
          }}
          className="h-9 w-full rounded-md border border-input bg-background px-3 text-sm"
        >
          {(['kiroCredit', 'CNY', 'USD'] as BillingUnit[]).map((u) => (
            <option key={u} value={u}>
              {UNIT_LABEL[u]}
            </option>
          ))}
        </select>
      </SettingRow>
      <SettingRow
        label="不限额"
        hint="「不限额」与「上限 0」是两回事：前者随便花，后者一分都不能花。"
      >
        <input type="checkbox" checked={unlimited} onChange={(e) => setUnlimited(e.target.checked)} />
      </SettingRow>
      {!unlimited && (
        <SettingRow label="上限" hint="十进制填写，不经过浮点。">
          <Input value={limit} onChange={(e) => setLimit(e.target.value)} placeholder="例如 100" />
        </SettingRow>
      )}
      <SettingRow
        label="强制方式"
        hint="硬额度要求每条路都能算出保证不被突破的上界；算不出就拒绝该路，而不是用估算冒充保证。软额度**允许**没有上界的预留——原生积分本来就没有可证的调用前上界——所以带上限的软额度是会被突破的，它是警戒线不是闸门。"
      >
        <select
          value={enforcement}
          onChange={(e) => setEnforcement(e.target.value as BudgetEnforcement)}
          className="h-9 w-full rounded-md border border-input bg-background px-3 text-sm"
        >
          <option value="soft">软额度</option>
          <option value="hard">硬额度</option>
        </select>
      </SettingRow>
      <SettingRow label="并发上限">
        <Input value={maxInFlight} onChange={(e) => setMaxInFlight(e.target.value)} />
      </SettingRow>
      <SettingRow label="待结算上限">
        <Input value={maxPending} onChange={(e) => setMaxPending(e.target.value)} />
      </SettingRow>
      {current && (
        <p className="text-xs text-muted-foreground">
          当前已用 <span className="font-mono">{current.used}</span>，改上限不会改变它。
        </p>
      )}
      {error && <p className="text-xs text-destructive">{error}</p>}
      <Button
        size="sm"
        disabled={save.isPending}
        onClick={() => {
          setError(null)
          save.mutate(
            {
              unit,
              limit: unlimited ? null : limit,
              enforcement,
              maxInFlight: Number(maxInFlight) || 0,
              maxPending: Number(maxPending) || 0,
            },
            { onError: (e) => setError(message(e)) },
          )
        }}
      >
        {save.isPending ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Save className="h-3.5 w-3.5" />}
        保存额度
      </Button>
    </SettingGroup>
  )
}

function Adjust({ keyId, units }: { keyId: number; units: BillingUnit[] }) {
  const adjust = useAdjustBudget(keyId)
  const cycle = useNewCycle(keyId)
  const [unit, setUnit] = useState<BillingUnit>(units[0] ?? 'CNY')
  const [amount, setAmount] = useState('')
  const [reason, setReason] = useState('')
  const [direction, setDirection] = useState<'debit' | 'credit'>('credit')
  const [error, setError] = useState<string | null>(null)

  return (
    <SettingGroup
      title="调整与开新周期"
      description="都会留下审计记录，且按提交的标识幂等——网络重试不会重复扣款。"
    >
      <SettingRow label="币种">
        <select
          value={unit}
          onChange={(e) => setUnit(e.target.value as BillingUnit)}
          className="h-9 w-full rounded-md border border-input bg-background px-3 text-sm"
        >
          {units.map((u) => (
            <option key={u} value={u}>
              {UNIT_LABEL[u]}
            </option>
          ))}
        </select>
      </SettingRow>
      <SettingRow label="方向" hint="扣减增加已用量，返还减少已用量。">
        <select
          value={direction}
          onChange={(e) => setDirection(e.target.value as 'debit' | 'credit')}
          className="h-9 w-full rounded-md border border-input bg-background px-3 text-sm"
        >
          <option value="credit">返还</option>
          <option value="debit">扣减</option>
        </select>
      </SettingRow>
      <SettingRow label="金额">
        <Input value={amount} onChange={(e) => setAmount(e.target.value)} />
      </SettingRow>
      <SettingRow label="原因" hint="会写进审计记录。">
        <Input value={reason} onChange={(e) => setReason(e.target.value)} />
      </SettingRow>
      {error && <p className="text-xs text-destructive">{error}</p>}
      <div className="flex gap-2">
        <Button
          size="sm"
          variant="outline"
          disabled={adjust.isPending || !amount || !reason}
          onClick={() => {
            setError(null)
            adjust.mutate(
              { adjustmentId: crypto.randomUUID(), unit, direction, amount, reason },
              { onError: (e) => setError(message(e)) },
            )
          }}
        >
          提交调整
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={cycle.isPending || !reason}
          onClick={() => {
            setError(null)
            cycle.mutate(
              { operationId: crypto.randomUUID(), unit, reason },
              { onError: (e) => setError(message(e)) },
            )
          }}
        >
          <RotateCcw className="h-3.5 w-3.5" />
          开新周期
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">
        开新周期把当前账户存档后把已用量归零。**存档不删**，历史随时可查；在飞与待结算不为零时不允许开新周期。
      </p>
    </SettingGroup>
  )
}

/** 只读审计。这里展示的是账本自己的记录，不是统计视图。 */
function Audit({ keyId }: { keyId: number }) {
  const { data } = useLedgerAudit(keyId)
  const entries = (data as { entries?: { kind: string; reason: string; operationId: string }[] } | undefined)?.entries
  if (!entries || entries.length === 0) {
    return <p className="text-xs text-muted-foreground">暂无审计记录。</p>
  }
  return (
    <SettingGroup title="账本审计" description="只读。账本记录与「重置统计」无关——重置统计只清分析视图，动不了这里。">
      {entries.slice(0, 20).map((e) => (
        <div key={e.operationId} className="flex gap-2 text-xs">
          <Badge variant="outline">{e.kind}</Badge>
          <span className="text-muted-foreground">{e.reason}</span>
        </div>
      ))}
    </SettingGroup>
  )
}

export function ClientKeyBudgets({ keyId }: { keyId: number }) {
  const { data, isLoading, error } = useBudgets(keyId)

  if (isLoading) return <Loader2 className="h-4 w-4 animate-spin" />
  if (error) {
    const notConfigured = isAxiosError(error) && error.response?.status === 404
    return (
      <p className="text-xs text-muted-foreground">
        {notConfigured
          ? '多上游网关未配置，这个 Key 没有账本额度。它的积分上限仍由「累计积分上限」管着。'
          : '读取额度失败。'}
      </p>
    )
  }
  const budgets = data?.budgets ?? []

  return (
    <div className="space-y-3">
      <SettingReadout label="账本额度" hint="按币种分开。原生积分与钱是两种东西，不会合并成一个数字。">
        {budgets.length === 0 ? (
          <span className="text-muted-foreground">尚未设置</span>
        ) : (
          <span className="font-mono">{budgets.map((b) => UNIT_LABEL[b.unit]).join('、')}</span>
        )}
      </SettingReadout>
      {budgets.map((b) => (
        <BudgetRow key={b.unit} budget={b} />
      ))}
      <EditBudget keyId={keyId} existing={budgets} />
      {budgets.length > 0 && <Adjust keyId={keyId} units={budgets.map((b) => b.unit)} />}
      <Audit keyId={keyId} />
    </div>
  )
}

function message(e: unknown): string {
  if (isAxiosError(e)) {
    const body = e.response?.data as { error?: { code?: string; message?: string } } | undefined
    if (body?.error?.code === 'quota_exceeded') return `额度不足：${body.error.message ?? ''}`
    if (body?.error?.message) return body.error.message
  }
  return String(e)
}
