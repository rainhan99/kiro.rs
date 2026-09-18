import { Check, Monitor, Moon, Palette, Sun } from 'lucide-react'
import { Button } from '@/components/ui/button'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from '@/components/ui/tooltip'
import {
  THEME_METADATA,
  type ThemeId,
  type ThemeMode,
  type ThemeSelection,
} from '@/lib/theme'

const MODE_OPTIONS: readonly {
  id: ThemeMode
  label: string
  icon: typeof Monitor
}[] = [
  { id: 'system', label: '跟随系统', icon: Monitor },
  { id: 'light', label: '浅色', icon: Sun },
  { id: 'dark', label: '深色', icon: Moon },
]

interface ThemePickerProps {
  theme: ThemeSelection
  isDarkMode: boolean
  onSelectPalette: (palette: ThemeId) => void
  onSelectMode: (mode: ThemeMode) => void
}

export function ThemePicker({
  theme,
  isDarkMode,
  onSelectPalette,
  onSelectMode,
}: ThemePickerProps) {
  const palette = THEME_METADATA.find((item) => item.id === theme.palette) ?? THEME_METADATA[0]
  const modeLabel = MODE_OPTIONS.find((item) => item.id === theme.mode)?.label ?? '跟随系统'
  const title = `主题：${palette.name} · ${modeLabel}${theme.mode === 'system' ? `（当前${isDarkMode ? '深色' : '浅色'}）` : ''}`

  return (
    <TooltipProvider delayDuration={350}>
      <DropdownMenu modal={false}>
        <Tooltip>
          <TooltipTrigger asChild>
            <DropdownMenuTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                aria-label={title}
                className="relative text-primary"
              >
                <Palette className="h-4 w-4" />
                <span className="absolute right-1 top-1 size-1.5 rounded-full bg-primary" aria-hidden="true" />
              </Button>
            </DropdownMenuTrigger>
          </TooltipTrigger>
          <TooltipContent>{title}</TooltipContent>
        </Tooltip>
        <DropdownMenuContent align="end" className="max-h-[var(--radix-dropdown-menu-content-available-height)] w-64 max-w-[calc(100vw-2rem)] overflow-y-auto p-1.5">
          <DropdownMenuLabel id="palette-label">配色主题</DropdownMenuLabel>
          <DropdownMenuGroup aria-labelledby="palette-label" className="grid gap-1">
            {THEME_METADATA.map((item) => (
              <DropdownMenuItem
                key={item.id}
                role="menuitemradio"
                aria-checked={theme.palette === item.id}
                onSelect={() => onSelectPalette(item.id)}
                data-active={theme.palette === item.id}
                className="gap-3 rounded-md px-2.5 py-2 data-[active=true]:bg-accent data-[active=true]:text-accent-foreground"
              >
                <span
                  aria-hidden="true"
                  data-theme={item.id}
                  className="theme-preview flex h-8 w-12 shrink-0 overflow-hidden rounded-sm border border-border bg-background"
                >
                  <span className="flex w-3.5 shrink-0 flex-col gap-1 bg-sidebar p-1">
                    <span className="h-1 w-full rounded-full bg-primary" />
                    <span className="h-1 w-full rounded-full bg-primary/25" />
                  </span>
                  <span className="flex flex-1 flex-col gap-1 p-1">
                    <span className="h-2 rounded-xs bg-card" />
                    <span className="h-2 rounded-xs bg-primary" />
                  </span>
                </span>
                <span className="min-w-0 flex-1 text-sm">{item.name}</span>
                {theme.palette === item.id && <Check className="size-4 text-primary" aria-hidden="true" />}
              </DropdownMenuItem>
            ))}
          </DropdownMenuGroup>
          <DropdownMenuSeparator />
          <DropdownMenuLabel id="mode-label">明暗模式</DropdownMenuLabel>
          <DropdownMenuGroup aria-labelledby="mode-label" className="grid grid-cols-3 gap-1 rounded-md bg-muted p-1">
            {MODE_OPTIONS.map(({ id, label, icon: Icon }) => (
              <DropdownMenuItem
                key={id}
                role="menuitemradio"
                aria-checked={theme.mode === id}
                onSelect={() => onSelectMode(id)}
                data-active={theme.mode === id}
                className="flex-col justify-center gap-1 rounded-sm px-1 py-2 data-[active=true]:bg-card data-[active=true]:text-primary data-[active=true]:shadow-xs"
              >
                <Icon aria-hidden="true" />
                <span>{label}</span>
              </DropdownMenuItem>
            ))}
          </DropdownMenuGroup>
        </DropdownMenuContent>
      </DropdownMenu>
    </TooltipProvider>
  )
}
