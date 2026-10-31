import { useCallback, useEffect, useMemo, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import type { AppSettings, UsageReport } from './types'
import {
  formatDuration,
  groupReport,
  mostUsedWindow,
  panelHeightFor,
  resetText,
  trayStatus,
  windowLabel,
  worstStatus,
} from './usage'
import './App.css'

const defaultSettings: AppSettings = {
  refreshIntervalSec: 120,
  alertThresholdPercent: 20,
  collapseDelayMs: 0,
  autoStart: true,
  dockPosition: 'top-right',
  serverUrl: 'http://127.0.0.1:8787',
  serverApiToken: '',
}

const emptyReport: UsageReport = { serverTime: 0, lastPollAt: null, snapshots: [] }

function fmtTime(epochSec: number | null): string {
  if (!epochSec || epochSec <= 0) return '--'
  return new Date(epochSec * 1000).toLocaleTimeString()
}

function isConfigured(settings: AppSettings): boolean {
  return settings.serverUrl.trim() !== '' && settings.serverApiToken.trim() !== ''
}

function App() {
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [settings, setSettings] = useState<AppSettings>(defaultSettings)
  const [savedSettings, setSavedSettings] = useState<AppSettings | null>(null)
  const [report, setReport] = useState<UsageReport>(emptyReport)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState('')
  const [testEmailStatus, setTestEmailStatus] = useState('')

  const groups = useMemo(() => groupReport(report), [report])
  const status = useMemo(() => worstStatus(groups), [groups])
  const configured = savedSettings !== null && isConfigured(savedSettings)
  // Countdowns use the report's own clock, so a Mac with a wrong clock still reads true.
  const nowSec = report.serverTime

  const panelHeight = useMemo(() => {
    if (settingsOpen) return 420
    return panelHeightFor(groups) + (error ? 45 : 0) + (configured ? 0 : 70)
  }, [settingsOpen, groups, error, configured])

  useEffect(() => {
    void invoke('resize_panel', { height: panelHeight })
  }, [panelHeight])

  const refresh = useCallback(async () => {
    setLoading(true)
    setError('')
    try {
      const next = await invoke<UsageReport>('get_usage_report')
      setReport(next)
      const nextGroups = groupReport(next)
      await invoke('update_tray_status', { status: trayStatus(worstStatus(nextGroups)) })
      const lowest = mostUsedWindow(nextGroups)
      if (lowest) {
        await invoke('notify_if_needed', { snapshot: lowest })
      }
    } catch (e) {
      // Keep the last report on screen: old numbers with a visible error beat none.
      setError(String(e))
      await invoke('update_tray_status', { status: 'warning' })
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    const init = async () => {
      try {
        const loaded = await invoke<AppSettings>('get_settings')
        setSettings(loaded)
        setSavedSettings(loaded)
      } catch (e) {
        setError(String(e))
      }
    }
    void init()

    const appWindow = getCurrentWindow()
    const unlisten = appWindow.onFocusChanged(({ payload: focused }) => {
      if (!focused) {
        setTimeout(() => {
          if (!document.hasFocus()) {
            void invoke('hide_panel')
            setSettingsOpen(false)
          }
        }, 200)
      }
    })

    return () => {
      unlisten.then((fn) => fn())
    }
  }, [])

  useEffect(() => {
    if (!configured || savedSettings === null) return
    const initialRefresh = window.setTimeout(() => {
      void refresh()
    }, 0)
    const timer = window.setInterval(() => {
      void refresh()
    }, Math.max(15, savedSettings.refreshIntervalSec) * 1000)

    return () => {
      window.clearTimeout(initialRefresh)
      window.clearInterval(timer)
    }
  }, [refresh, configured, savedSettings])

  const saveSettings = async () => {
    try {
      const saved = await invoke<AppSettings>('save_settings', { settings })
      setSettings(saved)
      setSavedSettings(saved)
      setError('')
    } catch (e) {
      setError(String(e))
    }
  }

  const sendTestEmail = async () => {
    try {
      setTestEmailStatus('Sending...')
      const message = await invoke<string>('send_test_email')
      setTestEmailStatus(message)
    } catch (e) {
      setTestEmailStatus(String(e))
    }
  }

  return (
    <div
      className={`panel ${status}`}
      onMouseEnter={() => void invoke('panel_mouse_enter')}
      onMouseLeave={() => void invoke('panel_mouse_leave')}
    >
      <div className="content">
        <div className="header">
          <div>
            <h1>AI Usage Monitor</h1>
            <p>Server polled {fmtTime(report.lastPollAt)}</p>
          </div>
          <button type="button" className="ghost" onClick={() => setSettingsOpen((prev) => !prev)}>
            {settingsOpen ? 'Back' : 'Settings'}
          </button>
        </div>

        {!settingsOpen && (
          <>
            {configured && groups.length === 0 && !loading && !error && (
              <div className="card muted">No usage data yet...</div>
            )}

            {groups.map((group) => (
              <div key={group.key} className={`card ${group.error ? 'card-error' : ''} ${group.stale ? 'card-cached' : ''}`}>
                <div className="row">
                  <span className="provider-name">
                    {group.name}
                    {group.plan && <span className="plan-tag"> {group.plan}</span>}
                  </span>
                  {group.stale && (
                    <span className="cached-badge">
                      {group.lastPushAt > 0
                        ? `stale ${formatDuration(nowSec - group.lastPushAt)}`
                        : 'no data yet'}
                    </span>
                  )}
                </div>
                {group.error && <div className="card-error-msg">{group.error}</div>}
                {group.windows.map((window) => (
                  <div key={window.windowType} className="window">
                    <div className="meta">
                      <span className="window-name">{windowLabel(window.windowType)}</span>
                      <strong>{Math.round(window.used)}% used</strong>
                    </div>
                    <div className="bar">
                      <div
                        className={`bar-fill ${window.status}`}
                        style={{ width: `${Math.max(0, Math.min(100, window.used))}%` }}
                      />
                    </div>
                    <div className="meta reset-text">{resetText(window, nowSec)}</div>
                  </div>
                ))}
              </div>
            ))}

            <div className="status-row">
              {loading ? 'Refreshing...' : `Updated: ${fmtTime(report.serverTime)}`}
              <span className="version-tag">v0.2.0</span>
            </div>

            {savedSettings !== null && !configured && (
              <div className="warning">
                <p>Server is not configured.</p>
                <p>Open Settings and enter the server URL and access token.</p>
              </div>
            )}
          </>
        )}

        {settingsOpen && (
          <div className="settings">
            <label>
              Server URL
              <input
                type="text"
                value={settings.serverUrl}
                onChange={(e) => setSettings({ ...settings, serverUrl: e.target.value })}
                placeholder="http://100.x.y.z:8787"
              />
            </label>
            <label>
              Access token
              <input
                type="password"
                value={settings.serverApiToken}
                onChange={(e) => setSettings({ ...settings, serverApiToken: e.target.value })}
                placeholder="Stored only in the local app config"
                autoComplete="off"
              />
            </label>
            <button type="button" className="secondary-btn" onClick={() => void sendTestEmail()}>
              Send test email
            </button>
            {testEmailStatus && <p className="settings-status">{testEmailStatus}</p>}
            <label>
              Refresh interval (sec)
              <input
                type="number"
                min={15}
                value={settings.refreshIntervalSec}
                onChange={(e) =>
                  setSettings({ ...settings, refreshIntervalSec: Number(e.target.value) || 60 })
                }
              />
            </label>
            <label>
              Alert threshold (%)
              <input
                type="number"
                min={1}
                max={99}
                value={settings.alertThresholdPercent}
                onChange={(e) =>
                  setSettings({
                    ...settings,
                    alertThresholdPercent: Number(e.target.value) || 20,
                  })
                }
              />
            </label>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={settings.autoStart}
                onChange={(e) => setSettings({ ...settings, autoStart: e.target.checked })}
              />
              Start on boot
            </label>
            <button type="button" onClick={() => void saveSettings()}>
              Save settings
            </button>
          </div>
        )}

        {error && <div className="error">{error}</div>}
      </div>
    </div>
  )
}

export default App
