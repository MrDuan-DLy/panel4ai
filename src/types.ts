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

export type AppSettings = {
  provider: 'openai_oauth' | 'claude_oauth' | string
  refreshIntervalSec: number
  alertThresholdPercent: number
  collapseDelayMs: number
  autoStart: boolean
  dockPosition: string
  selectedWindowType: 'hourly5' | 'weekly' | 'code_review_weekly' | string
  codexAuthPath: string
  claudeAuthPath: string
  limitsByWindow: Record<string, number>
}

export type OAuthStatus = {
  available: boolean
  source: string
  message?: string | null
}
