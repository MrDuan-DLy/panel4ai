export type UsageSnapshot = {
  provider: string
  windowType: string
  windowStart: number
  windowEnd: number
  used: number
  limit: number
  remainingPercent: number
  resetAt: number
  updatedAt: number
  status: 'ok' | 'warning' | 'danger' | 'error' | string
  message?: string | null
}

export type UsageReport = {
  serverTime: number
  lastPollAt: number | null
  snapshots: UsageSnapshot[]
  providerErrors?: Record<string, string>
  staleProviders?: Record<string, number>
}

export type AppSettings = {
  refreshIntervalSec: number
  alertThresholdPercent: number
  collapseDelayMs: number
  autoStart: boolean
  dockPosition: string
  serverUrl: string
  serverApiToken: string
}
