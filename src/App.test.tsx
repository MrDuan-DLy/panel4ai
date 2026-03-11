import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import type { UsageSnapshot, AppSettings, OAuthStatus } from './types'

// Mock @tauri-apps/api/core
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}))

// Mock @tauri-apps/api/window
vi.mock('@tauri-apps/api/window', () => ({
  getCurrentWindow: () => ({
    onFocusChanged: vi.fn(() => Promise.resolve(() => {})),
  }),
}))

import { invoke } from '@tauri-apps/api/core'
import App from './App'

const mockedInvoke = vi.mocked(invoke)

const defaultSettings: AppSettings = {
  provider: 'openai_oauth',
  refreshIntervalSec: 60,
  alertThresholdPercent: 20,
  collapseDelayMs: 0,
  autoStart: true,
  dockPosition: 'top-right',
  selectedWindowType: 'hourly5',
  codexAuthPath: '',
  claudeAuthPath: '',
  limitsByWindow: { hourly5: 100, weekly: 100 },
}

const defaultOAuthStatus: OAuthStatus = {
  available: true,
  source: 'OpenAI',
  message: 'OAuth token(s) loaded',
}

const okSnapshot: UsageSnapshot = {
  provider: 'openai-oauth',
  windowType: 'hourly5',
  windowStart: 1700000000,
  windowEnd: 1700018000,
  used: 30,
  limit: 100,
  remainingPercent: 70,
  resetAt: 1700018000,
  updatedAt: 1700000000,
  status: 'ok',
  message: null,
}

const warningSnapshot: UsageSnapshot = {
  ...okSnapshot,
  provider: 'claude-oauth',
  used: 85,
  remainingPercent: 15,
  status: 'warning',
}

const dangerSnapshot: UsageSnapshot = {
  ...okSnapshot,
  used: 95,
  remainingPercent: 5,
  status: 'danger',
}

const errorSnapshot: UsageSnapshot = {
  ...okSnapshot,
  provider: 'openai-oauth',
  used: 0,
  limit: 0,
  remainingPercent: 0,
  status: 'error',
  message: 'rate limited',
}

const cachedSnapshot: UsageSnapshot = {
  ...okSnapshot,
  status: 'cached',
  message: '(cached) rate limited',
}

function setupMocks(overrides?: {
  settings?: AppSettings
  oauth?: OAuthStatus
  snapshots?: UsageSnapshot[]
}) {
  const settings = overrides?.settings ?? defaultSettings
  const oauth = overrides?.oauth ?? defaultOAuthStatus
  const snapshots = overrides?.snapshots ?? []

  mockedInvoke.mockImplementation(async (cmd: string) => {
    switch (cmd) {
      case 'get_settings': return settings
      case 'get_oauth_status': return oauth
      case 'get_all_usage_snapshots': return snapshots
      case 'resize_panel': return undefined
      case 'update_tray_status': return undefined
      case 'notify_if_needed': return false
      case 'panel_mouse_enter': return undefined
      case 'panel_mouse_leave': return undefined
      default: return undefined
    }
  })
}

async function renderApp(options?: { expectRefresh?: boolean }) {
  render(<App />)

  await waitFor(() => {
    expect(mockedInvoke).toHaveBeenCalledWith('get_settings')
    expect(mockedInvoke).toHaveBeenCalledWith('get_oauth_status')
  })

  if (options?.expectRefresh !== false) {
    await waitFor(() => {
      expect(mockedInvoke).toHaveBeenCalledWith('update_tray_status', expect.any(Object))
    })
  }
}

beforeEach(() => {
  vi.clearAllMocks()
})

describe('App', () => {
  it('renders the title', async () => {
    setupMocks()
    await renderApp()
    expect(screen.getByText('AI Usage Monitor')).toBeInTheDocument()
  })

  it('renders settings button', async () => {
    setupMocks()
    await renderApp()
    expect(screen.getByText('Settings')).toBeInTheDocument()
  })

  it('shows "No usage data yet" when no snapshots', async () => {
    setupMocks({ oauth: defaultOAuthStatus, snapshots: [] })
    await renderApp()
    expect(screen.getByText('No usage data yet...')).toBeInTheDocument()
  })

  it('shows provider name for ok snapshot', async () => {
    setupMocks({ snapshots: [okSnapshot] })
    await renderApp()
    expect(screen.getByText('openai-oauth')).toBeInTheDocument()
  })

  it('shows error message for error snapshot', async () => {
    setupMocks({ snapshots: [errorSnapshot] })
    await renderApp()
    expect(screen.getByText('rate limited')).toBeInTheDocument()
  })

  it('shows warning when no oauth available', async () => {
    setupMocks({
      oauth: { available: false, source: 'none', message: 'No OAuth providers configured.' },
    })
    await renderApp({ expectRefresh: false })
    expect(screen.getByText('No OAuth providers found.')).toBeInTheDocument()
  })

  it('calls resize_panel on mount', async () => {
    setupMocks()
    await renderApp()
    await waitFor(() => {
      expect(mockedInvoke).toHaveBeenCalledWith('resize_panel', expect.any(Object))
    })
  })

  it('calls update_tray_status after fetching snapshots', async () => {
    setupMocks({ snapshots: [okSnapshot] })
    await renderApp()
    await waitFor(() => {
      expect(mockedInvoke).toHaveBeenCalledWith('update_tray_status', { status: 'ok' })
    })
  })

  it('reports danger as worst tray status', async () => {
    setupMocks({ snapshots: [okSnapshot, dangerSnapshot] })
    await renderApp()
    await waitFor(() => {
      expect(mockedInvoke).toHaveBeenCalledWith('update_tray_status', { status: 'danger' })
    })
  })

  it('reports warning as worst tray status', async () => {
    setupMocks({ snapshots: [okSnapshot, warningSnapshot] })
    await renderApp()
    await waitFor(() => {
      expect(mockedInvoke).toHaveBeenCalledWith('update_tray_status', { status: 'warning' })
    })
  })

  it('reports warning when all snapshots are errors', async () => {
    setupMocks({ snapshots: [errorSnapshot] })
    await renderApp()
    await waitFor(() => {
      expect(mockedInvoke).toHaveBeenCalledWith('update_tray_status', { status: 'warning' })
    })
  })

  it('applies error panel styling for error snapshots', async () => {
    setupMocks({ snapshots: [errorSnapshot] })
    await renderApp()
    expect(document.querySelector('.panel.error')).toBeTruthy()
  })

  it('applies cached panel styling for cached snapshots', async () => {
    setupMocks({ snapshots: [cachedSnapshot] })
    await renderApp()
    expect(document.querySelector('.panel.cached')).toBeTruthy()
  })
})

describe('types', () => {
  it('UsageSnapshot matches expected shape', () => {
    const snap: UsageSnapshot = okSnapshot
    expect(snap.provider).toBe('openai-oauth')
    expect(snap.status).toBe('ok')
    expect(snap.remainingPercent).toBe(70)
  })

  it('AppSettings has all required fields', () => {
    const s: AppSettings = defaultSettings
    expect(s.provider).toBe('openai_oauth')
    expect(s.refreshIntervalSec).toBe(60)
    expect(s.limitsByWindow.hourly5).toBe(100)
    expect(s.limitsByWindow.weekly).toBe(100)
  })
})
