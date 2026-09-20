import {
  Cpu,
  Gauge,
  Globe,
  ScrollText,
  PackageOpen,
  ShieldCheck,
  Tags,
  GitBranch,
  Waypoints,
} from 'lucide-react'
import { PageHeader } from '@/components/console/page-header'
import { Badge } from '@/components/ui/badge'
import { Card, CardContent } from '@/components/ui/card'
import { useUrlState } from '@/hooks/use-url-state'
import { cn } from '@/lib/utils'
import { DispatchSection } from '@/components/settings/dispatch-section'
import { NetworkSection } from '@/components/settings/network-section'
import { LogSection } from '@/components/settings/log-section'
import { SystemSection } from '@/components/settings/system-section'
import { SecuritySection } from '@/components/settings/security-section'
import { MetadataSection } from '@/components/settings/metadata-section'
import { ModelsSection } from '@/components/settings/models-section'
import { RequestPipelineSection } from '@/components/settings/request-pipeline-section'
import { GatewaySection } from '@/components/settings/gateway-section'

/**
 * 设置页 —— 把此前散在三处的 7 个配置端点收拢到一处。
 *
 * 改造前它们分别住在：顶栏按钮（负载均衡）、顶栏两个下拉（风控故障转移、自愈）、
 * 顶栏设置菜单（登录密钥）、日志页下拉（日志治理）、代理池弹窗内（全局代理）、
 * 镜像更新弹窗内（更新配置）。同一类东西分在六个地方，找一个配置得先记住它藏在哪。
 *
 * 顶栏**保留**三个快捷开关（负载均衡 / 故障转移 / 自愈），因为它们是运维高频动作，
 * 一次点击就该切换完；但参数（冷却时长、连续上限、保留天数这些）全部移到这里 ——
 * 下拉菜单里塞数字输入框本来就不是它该干的事。
 */
type SectionKey =
  | 'dispatch'
  | 'metadata'
  | 'network'
  | 'log'
  | 'models'
  | 'system'
  | 'security'
  | 'pipeline'
  | 'gateway'

/// 设置页的分区列表（窄屏顶部导航用）。
///
/// 桌面端的入口在 `app-layout.tsx` 的 `TABS` 里，是**另一份**列表。两份必须一一
/// 对应，否则新加的分区只在一种屏宽下点得到——多上游网关就这么漏过一次：设置页
/// 加了，侧边栏没加，桌面端根本进不去。`settings-sections.test.js` 钉住这件事。
export const SECTIONS: {
  key: SectionKey
  label: string
  icon: React.ReactNode
}[] = [
  { key: 'pipeline', label: '请求管线', icon: <GitBranch className="h-4 w-4" /> },
  { key: 'gateway', label: '多上游网关', icon: <Waypoints className="h-4 w-4" /> },
  {
    key: 'dispatch',
    label: '调度',
    icon: <Gauge className="h-4 w-4" />,
  },
  {
    key: 'metadata',
    label: '凭据字段',
    icon: <Tags className="h-4 w-4" />,
  },
  {
    key: 'network',
    label: '网络',
    icon: <Globe className="h-4 w-4" />,
  },
  {
    key: 'log',
    label: '日志',
    icon: <ScrollText className="h-4 w-4" />,
  },
  {
    key: 'models',
    label: '模型',
    icon: <Cpu className="h-4 w-4" />,
  },
  {
    key: 'system',
    label: '系统',
    icon: <PackageOpen className="h-4 w-4" />,
  },
  {
    key: 'security',
    label: '安全',
    icon: <ShieldCheck className="h-4 w-4" />,
  },
]

export function SettingsPage() {
  const [urlState, patchUrl] = useUrlState('settings', { s: 'dispatch' })
  const active = (SECTIONS.some((x) => x.key === urlState.s)
    ? urlState.s
    : 'dispatch') as SectionKey

  const activeMeta = SECTIONS.find((s) => s.key === active) ?? SECTIONS[0]

  return (
    <div className="console-scope space-y-4">
      <PageHeader
        breadcrumbs={[
          { label: '控制台' },
          { label: '系统设置', onClick: () => patchUrl({ s: 'dispatch' }) },
          { label: activeMeta.label, active: true },
        ]}
        icon={activeMeta.icon}
        title={`设置 · ${activeMeta.label}`}
        description={
          active === 'pipeline'
            ? '请求管线配置保存到文件，服务重启后生效。当前运行值与已保存值分别展示。'
            : active === 'gateway'
              ? '网关配置保存即生效，但只影响此后的新请求——已在飞的请求按它开始时的那一份快照走完。'
              : '本分区改动即时生效并写入配置文件，无需重启服务进程。'
        }
        badge={
          <Badge variant="outline" className="font-mono text-xs">
            {active === 'pipeline' ? '重启生效' : active === 'gateway' ? '保存即生效' : '热重载就绪'}
          </Badge>
        }
      />

      {/* 窄屏设备横向滚动的子菜单导航栏（桌面端由左侧全局侧边栏接管） */}
      <nav
        className="flex lg:hidden shrink-0 gap-1 overflow-x-auto px-1 py-1 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
        aria-label="设置分区"
      >
        {SECTIONS.map((s) => (
          <button
            key={s.key}
            type="button"
            onClick={() => patchUrl({ s: s.key })}
            aria-current={active === s.key ? 'page' : undefined}
            className={cn(
              'inline-flex shrink-0 items-center gap-1.5 rounded-md px-2.5 py-1.5 text-xs font-medium transition-colors',
              'focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring',
              active === s.key
                ? 'bg-primary/10 text-primary font-semibold'
                : 'text-muted-foreground hover:bg-accent hover:text-foreground',
            )}
          >
            {s.icon}
            <span className="whitespace-nowrap">{s.label}</span>
          </button>
        ))}
      </nav>

      <div className="flex flex-col gap-4">

        <Card className="min-w-0 flex-1">
          <CardContent className="p-4 sm:p-5">
            {active === 'dispatch' && <DispatchSection />}
            {active === 'pipeline' && <RequestPipelineSection />}
            {active === 'gateway' && <GatewaySection />}
            {active === 'metadata' && <MetadataSection />}
            {active === 'models' && <ModelsSection />}
            {active === 'network' && <NetworkSection />}
            {active === 'log' && <LogSection />}
            {active === 'system' && <SystemSection />}
            {active === 'security' && <SecuritySection />}
          </CardContent>
        </Card>
      </div>
    </div>
  )
}
