import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import type { AppSettings, UsageReport, UsageSnapshot } from './types'

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}))

vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => ({
    onFocusChanged: vi.fn(() => Promise.resolve(() => {})),
  }),
}))

import { invoke } from '@tauri-apps/api/core'
import App from './App'
import {
  formatDuration,
  groupReport,
  mostUsedWindow,
  planOf,
  providerKey,
  resetText,
  trayStatus,
  windowLabel,
  worstStatus,
} from './usage'

const mockedInvoke = vi.mocked(invoke)
const NOW = 1_800_000_000

const defaultSettings: AppSettings = {
  refreshIntervalSec: 60,
  alertThresholdPercent: 20,
  collapseDelayMs: 0,
  autoStart: true,
  dockPosition: 'top-right',
  serverUrl: 'http://100.1.2.3:8787',
  serverApiToken: 'test-token',
}

function snapshot(provider: string, windowType: string, used: number, status = 'ok'): UsageSnapshot {
  return {
    provider,
    windowType,
    windowStart: NOW - 3600,
    windowEnd: NOW + 7200,
    used,
    limit: 100,
    remainingPercent: 100 - used,
    resetAt: NOW + 7200,
    updatedAt: NOW,
    status,
    message: null,
  }
}

const report: UsageReport = {
  serverTime: NOW,
  lastPollAt: NOW,
  snapshots: [
    snapshot('antigravity-push', 'weekly_argon', 74),
    snapshot('antigravity-push', 'hourly5_gemini', 1),
    snapshot('grok-cli', 'weekly', 36),
    snapshot('openai-app-server (prolite)', 'weekly', 54),
    snapshot('claude-oauth (max, default_claude_max_5x)', 'weekly', 45),
    snapshot('claude-oauth (max, default_claude_max_5x)', 'hourly5', 36),
  ],
}

function setupMocks(overrides?: {
  settings?: AppSettings
  report?: UsageReport
  reportError?: string
}) {
  const settings = overrides?.settings ?? defaultSettings
  mockedInvoke.mockImplementation(async (cmd: string) => {
    switch (cmd) {
      case 'get_settings':
        return settings
      case 'get_usage_report':
        if (overrides?.reportError) throw new Error(overrides.reportError)
        return overrides?.report ?? report
      case 'notify_if_needed':
        return false
      default:
        return undefined
    }
  })
}

describe('usage helpers', () => {
  it('derives the provider key and plan from the source string', () => {
    expect(providerKey('openai-app-server (prolite)')).toBe('openai')
    expect(providerKey('claude-oauth (max, default_claude_max_5x)')).toBe('claude')
    expect(providerKey('grok-cli')).toBe('grok')
    expect(providerKey('antigravity-push')).toBe('antigravity')
    expect(planOf('claude-oauth (max, default_claude_max_5x)')).toBe('max')
    expect(planOf('openai-app-server (prolite)')).toBe('prolite')
    expect(planOf('grok-cli')).toBeNull()
  })

  it('labels windows without knowing every provider', () => {
    expect(windowLabel('hourly5')).toBe('5h')
    expect(windowLabel('weekly')).toBe('Weekly')
    expect(windowLabel('weekly_gemini')).toBe('Weekly · Gemini')
    expect(windowLabel('hourly5_claudegpt')).toBe('5h · Claude+GPT')
    expect(windowLabel('code_review_weekly')).toBe('Weekly · Code review')
    expect(windowLabel('something_new')).toBe('something_new')
  })

  it('groups and orders providers and their windows', () => {
    const groups = groupReport(report)
    expect(groups.map((group) => group.name)).toEqual(['Claude', 'Codex', 'Grok', 'Antigravity'])
    expect(groups[0].windows.map((window) => window.windowType)).toEqual(['hourly5', 'weekly'])
    expect(groups[3].windows.map((window) => window.windowType)).toEqual([
      'hourly5_gemini',
      'weekly_argon',
    ])
  })

  it('shows an unknown provider without code changes', () => {
    const groups = groupReport({
      ...report,
      snapshots: [snapshot('newvendor-push', 'monthly', 5)],
    })
    expect(groups.map((group) => group.name)).toEqual(['Newvendor'])
  })

  it('keeps a provider card for errors and stale pushes with no windows', () => {
    const groups = groupReport({
      serverTime: NOW,
      lastPollAt: NOW,
      snapshots: [],
      providerErrors: { grok: 'login expired' },
      staleProviders: { antigravity: NOW - 7200 },
    })
    expect(groups.find((group) => group.key === 'grok')?.error).toBe('login expired')
    const antigravity = groups.find((group) => group.key === 'antigravity')
    expect(antigravity?.stale).toBe(true)
    expect(antigravity?.lastPushAt).toBe(NOW - 7200)
  })

  it('rolls status up with danger first and stale data as a mild warning', () => {
    const danger = groupReport({ ...report, snapshots: [snapshot('grok-cli', 'weekly', 95, 'danger')] })
    expect(worstStatus(danger)).toBe('danger')
    expect(trayStatus(worstStatus(danger))).toBe('danger')
    const failed = groupReport({ ...report, snapshots: [], providerErrors: { grok: 'x' } })
    expect(worstStatus(failed)).toBe('error')
    expect(trayStatus(worstStatus(failed))).toBe('warning')
    const stale = groupReport({ ...report, staleProviders: { antigravity: 1 } })
    expect(worstStatus(stale)).toBe('cached')
    expect(worstStatus(groupReport(report))).toBe('ok')
  })

  it('finds the window closest to its limit', () => {
    expect(mostUsedWindow(groupReport(report))?.windowType).toBe('weekly_argon')
    expect(mostUsedWindow([])).toBeNull()
  })

  it('does not count down on a window that has not started', () => {
    expect(resetText(snapshot('grok-cli', 'weekly', 0), NOW)).toBe('Not started')
    expect(resetText(snapshot('grok-cli', 'weekly', 10), NOW)).toBe('Resets in 2h 0m')
    expect(resetText({ ...snapshot('grok-cli', 'weekly', 10), resetAt: NOW - 1 }, NOW)).toBe('Resetting')
    expect(formatDuration(90)).toBe('2m')
    expect(formatDuration(3 * 86_400 + 3600)).toBe('3d 1h')
  })
})

describe('App', () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  it('renders every provider window from the server report', async () => {
    setupMocks()
    render(<App />)
    await waitFor(() => expect(screen.getByText('Antigravity')).toBeInTheDocument())
    expect(screen.getByText('Claude')).toBeInTheDocument()
    expect(screen.getByText('Codex')).toBeInTheDocument()
    expect(screen.getByText('Grok')).toBeInTheDocument()
    expect(screen.getByText('Weekly · Argon')).toBeInTheDocument()
    expect(screen.getByText('74% used')).toBeInTheDocument()
    expect(mockedInvoke).toHaveBeenCalledWith('update_tray_status', { status: 'ok' })
  })

  it('warns about the closest window through the native notification check', async () => {
    setupMocks()
    render(<App />)
    await waitFor(() =>
      expect(mockedInvoke).toHaveBeenCalledWith(
        'notify_if_needed',
        expect.objectContaining({ snapshot: expect.objectContaining({ windowType: 'weekly_argon' }) }),
      ),
    )
  })

  it('asks for server settings instead of fetching when unconfigured', async () => {
    setupMocks({ settings: { ...defaultSettings, serverApiToken: '' } })
    render(<App />)
    await waitFor(() => expect(screen.getByText('Server is not configured.')).toBeInTheDocument())
    expect(mockedInvoke).not.toHaveBeenCalledWith('get_usage_report')
  })

  it('keeps showing an error when the server cannot be reached', async () => {
    setupMocks({ reportError: 'Server request failed' })
    render(<App />)
    await waitFor(() => expect(screen.getByText(/Server request failed/)).toBeInTheDocument())
    expect(mockedInvoke).toHaveBeenCalledWith('update_tray_status', { status: 'warning' })
  })

  it('shows a provider error on its card', async () => {
    setupMocks({ report: { ...report, providerErrors: { grok: 'login expired' } } })
    render(<App />)
    await waitFor(() => expect(screen.getByText('login expired')).toBeInTheDocument())
  })
})
