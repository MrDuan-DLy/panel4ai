import type { UsageReport, UsageSnapshot } from './types'

export type DisplayStatus = 'ok' | 'warning' | 'danger' | 'error' | 'cached'

export type ProviderGroup = {
  key: string
  name: string
  plan: string | null
  windows: UsageSnapshot[]
  error: string | null
  stale: boolean
  lastPushAt: number
}

const PROVIDER_NAMES: Record<string, string> = {
  openai: 'Codex',
  claude: 'Claude',
  grok: 'Grok',
  antigravity: 'Antigravity',
}
const PROVIDER_ORDER = ['claude', 'openai', 'grok', 'antigravity']
const WINDOW_NAMES: Record<string, string> = {
  hourly5: '5h',
  weekly: 'Weekly',
  daily: 'Daily',
  monthly: 'Monthly',
}
const SCOPE_NAMES: Record<string, string> = { claudegpt: 'Claude+GPT' }

function capitalize(value: string): string {
  return value ? value[0].toUpperCase() + value.slice(1) : value
}

// "openai-app-server (prolite)" -> "openai"
export function providerKey(source: string): string {
  return source.split(/[-\s(]/, 1)[0]
}

export function providerName(key: string): string {
  return PROVIDER_NAMES[key] ?? capitalize(key)
}

// "claude-oauth (max, default_claude_max_5x)" -> "max"
export function planOf(source: string): string | null {
  const match = /\(([^,)]+)/.exec(source)
  return match ? match[1].trim() : null
}

// "weekly_gemini" -> "Weekly · Gemini"; an unknown kind stays as the server reported it.
export function windowLabel(windowType: string): string {
  if (windowType === 'code_review_weekly') return 'Weekly · Code review'
  const match = /^(hourly5|weekly|daily|monthly)(?:[_-](.+))?$/.exec(windowType)
  if (!match) return windowType
  const base = WINDOW_NAMES[match[1]]
  if (!match[2]) return base
  return `${base} · ${SCOPE_NAMES[match[2]] ?? capitalize(match[2])}`
}

// Plain windows first, then per-model ones, with each kind in a fixed order.
function windowRank(windowType: string): number {
  const base = Object.keys(WINDOW_NAMES).findIndex((name) => windowType.startsWith(name))
  const scoped = windowType in WINDOW_NAMES ? 0 : 10
  return scoped + (base < 0 ? 9 : base)
}

export function groupReport(report: UsageReport): ProviderGroup[] {
  const groups = new Map<string, ProviderGroup>()
  const groupFor = (key: string): ProviderGroup => {
    let group = groups.get(key)
    if (!group) {
      group = {
        key,
        name: providerName(key),
        plan: null,
        windows: [],
        error: null,
        stale: false,
        lastPushAt: 0,
      }
      groups.set(key, group)
    }
    return group
  }
  for (const snapshot of report.snapshots) {
    const group = groupFor(providerKey(snapshot.provider))
    group.plan = planOf(snapshot.provider)
    group.windows.push(snapshot)
  }
  for (const [key, message] of Object.entries(report.providerErrors ?? {})) {
    groupFor(key).error = message
  }
  for (const [key, lastPushAt] of Object.entries(report.staleProviders ?? {})) {
    const group = groupFor(key)
    group.stale = true
    group.lastPushAt = lastPushAt
  }
  const rank = (key: string) => {
    const index = PROVIDER_ORDER.indexOf(key)
    return index < 0 ? PROVIDER_ORDER.length : index
  }
  const ordered = [...groups.values()].sort(
    (left, right) => rank(left.key) - rank(right.key) || left.name.localeCompare(right.name),
  )
  for (const group of ordered) {
    group.windows.sort(
      (left, right) =>
        windowRank(left.windowType) - windowRank(right.windowType) ||
        left.windowType.localeCompare(right.windowType),
    )
  }
  return ordered
}

export function worstStatus(groups: ProviderGroup[]): DisplayStatus {
  const statuses = groups.flatMap((group) => group.windows.map((window) => window.status))
  if (statuses.includes('danger')) return 'danger'
  if (groups.some((group) => group.error)) return 'error'
  if (statuses.includes('warning')) return 'warning'
  if (groups.some((group) => group.stale)) return 'cached'
  return 'ok'
}

export function trayStatus(status: DisplayStatus): 'ok' | 'warning' | 'danger' {
  if (status === 'danger') return 'danger'
  return status === 'ok' ? 'ok' : 'warning'
}

export function mostUsedWindow(groups: ProviderGroup[]): UsageSnapshot | null {
  const windows = groups.flatMap((group) => group.windows)
  if (windows.length === 0) return null
  return windows.reduce((worst, window) =>
    window.remainingPercent < worst.remainingPercent ? window : worst,
  )
}

export function formatDuration(seconds: number): string {
  const minutes = Math.max(1, Math.round(seconds / 60))
  if (minutes < 60) return `${minutes}m`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}h ${minutes % 60}m`
  return `${Math.floor(hours / 24)}d ${hours % 24}h`
}

// An unused window has no real reset: its reported time just keeps sliding ahead.
export function resetText(window: UsageSnapshot, nowSec: number): string {
  if (window.resetAt <= 0 || window.used <= 0) return 'Not started'
  if (window.resetAt <= nowSec) return 'Resetting'
  return `Resets in ${formatDuration(window.resetAt - nowSec)}`
}

export function panelHeightFor(groups: ProviderGroup[]): number {
  const content = groups.reduce(
    (height, group) => height + 36 + group.windows.length * 42 + (group.error ? 22 : 0),
    0,
  )
  return Math.min(720, Math.max(200, 58 + content + groups.length * 8 + 34))
}
