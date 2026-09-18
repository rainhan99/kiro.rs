export const THEME_STORAGE_KEY = 'adminTheme'

export type ThemeId =
  | 'system'
  | 'graphite'
  | 'ocean'
  | 'forest'
  | 'amber'

export type ThemeMode = 'light' | 'dark' | 'system'

export interface ThemeSelection {
  palette: ThemeId
  mode: ThemeMode
}

export interface ThemeMetadata {
  id: ThemeId
  name: string
}

export const DEFAULT_THEME_SELECTION: ThemeSelection = {
  palette: 'system',
  mode: 'system',
}

export const THEME_METADATA: readonly ThemeMetadata[] = [
  {
    id: 'system',
    name: '清透青',
  },
  {
    id: 'graphite',
    name: '石墨灰',
  },
  {
    id: 'ocean',
    name: '海洋蓝',
  },
  {
    id: 'forest',
    name: '松林绿',
  },
  {
    id: 'amber',
    name: '琥珀金',
  },
] as const

const THEME_IDS = new Set<ThemeId>(THEME_METADATA.map(({ id }) => id))
const THEME_MODES = new Set<ThemeMode>(['light', 'dark', 'system'])

export function isThemeId(value: unknown): value is ThemeId {
  return typeof value === 'string' && THEME_IDS.has(value as ThemeId)
}

export function isThemeMode(value: unknown): value is ThemeMode {
  return typeof value === 'string' && THEME_MODES.has(value as ThemeMode)
}

export function parseThemeSelection(value: unknown): ThemeSelection {
  if (!value || typeof value !== 'object') return { ...DEFAULT_THEME_SELECTION }

  const candidate = value as { palette?: unknown; mode?: unknown }
  if (!isThemeId(candidate.palette) || !isThemeMode(candidate.mode)) {
    return { ...DEFAULT_THEME_SELECTION }
  }

  return { palette: candidate.palette, mode: candidate.mode }
}

export function resolveSystemDarkMode(): boolean {
  if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') {
    return false
  }
  return window.matchMedia('(prefers-color-scheme: dark)').matches
}

export function resolveDarkMode(selection: ThemeSelection): boolean {
  return selection.mode === 'dark' || (selection.mode === 'system' && resolveSystemDarkMode())
}

export function applyTheme(selection: ThemeSelection, isDark = resolveDarkMode(selection)): boolean {
  if (typeof document === 'undefined') return isDark

  const root = document.documentElement
  root.dataset.theme = selection.palette
  root.classList.toggle('dark', isDark)
  return isDark
}
