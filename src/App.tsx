import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import type { AppSettings, OAuthStatus, UsageSnapshot } from './types'
import './App.css'

const CLAUDE_OAUTH_CLIENT_ID = '9d1c250a-e61b-44d9-88ed-5944d1962f5e'
const CLAUDE_OAUTH_REDIRECT_URI = 'https://platform.claude.com/oauth/code/callback'
const CLAUDE_OAUTH_SCOPES = 'user:profile user:inference user:sessions:claude_code'

const defaultSettings: AppSettings = {
  provider: 'openai_oauth',
  refreshIntervalSec: 120,
  alertThresholdPercent: 20,
  collapseDelayMs: 0,
  autoStart: true,
  dockPosition: 'top-right',
  selectedWindowType: 'hourly5',
  codexAuthPath: '',
  claudeAuthPath: '',
  limitsByWindow: {
    weekly: 100,
    hourly5: 100,
  },
}

const defaultOAuthStatus: OAuthStatus = {
  available: false,
  source: 'none',
  message: 'Checking OAuth status...',
}

function fmtTime(epochSec: number): string {
  return new Date(epochSec * 1000).toLocaleString()
}

function base64urlEncode(buffer: Uint8Array): string {
  let binary = ''
  for (const byte of buffer) {
    binary += String.fromCharCode(byte)
  }
  return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/g, '')
}

type DisplayStatus = 'ok' | 'warning' | 'danger' | 'error' | 'cached'

function getWorstDisplayStatus(items: UsageSnapshot[]): DisplayStatus {
  const statuses = items.map((item) => item.status)
  if (statuses.includes('danger')) return 'danger'
  if (statuses.includes('error')) return 'error'
  if (statuses.includes('warning')) return 'warning'
  if (statuses.includes('cached')) return 'cached'
  return 'ok'
}

function getTrayStatus(items: UsageSnapshot[]): 'ok' | 'warning' | 'danger' {
  const worst = getWorstDisplayStatus(items)
  if (worst === 'danger') return 'danger'
  if (worst === 'warning' || worst === 'error' || worst === 'cached') return 'warning'
  return 'ok'
}

function App() {
  const [platform, setPlatform] = useState('windows')
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [settings, setSettings] = useState<AppSettings>(defaultSettings)
  const [oauthStatus, setOauthStatus] = useState<OAuthStatus>(defaultOAuthStatus)
  const [snapshots, setSnapshots] = useState<UsageSnapshot[]>([])
  const snapshotsRef = useRef<UsageSnapshot[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState('')

  // Claude OAuth login state
  const [claudeLoginStep, setClaudeLoginStep] = useState<'idle' | 'waiting' | 'exchanging' | 'success'>('idle')
  const claudeLoginStepRef = useRef(claudeLoginStep)
  const [claudeAuthCode, setClaudeAuthCode] = useState('')
  const [claudeLoginError, setClaudeLoginError] = useState('')
  const [codeVerifier, setCodeVerifier] = useState('')
  const [oauthState, setOauthState] = useState('')

  // OpenAI OAuth login state
  const [openaiLoginStep, setOpenaiLoginStep] = useState<'idle' | 'logging_in' | 'success'>('idle')
  const openaiLoginStepRef = useRef(openaiLoginStep)
  const [openaiLoginError, setOpenaiLoginError] = useState('')

  // Keep refs in sync for use in blur handler
  useEffect(() => { claudeLoginStepRef.current = claudeLoginStep }, [claudeLoginStep])
  useEffect(() => { openaiLoginStepRef.current = openaiLoginStep }, [openaiLoginStep])
  useEffect(() => { snapshotsRef.current = snapshots }, [snapshots])

  // Calculate worst status across all providers
  const worstStatus = useMemo(() => getWorstDisplayStatus(snapshots), [snapshots])

  // Calculate panel height dynamically
  const panelHeight = useMemo(() => {
    if (settingsOpen) return 540
    const cardCount = snapshots.length || 1
    let h = 58 + cardCount * 120 + 24 + 20
    if (!oauthStatus.available) h += 70
    if (error) h += 45
    return Math.max(200, h)
  }, [settingsOpen, snapshots.length, oauthStatus.available, error])

  // Resize panel when content changes
  useEffect(() => {
    void invoke('resize_panel', { height: panelHeight })
  }, [panelHeight])

  const refresh = useCallback(async () => {
    if (!oauthStatus.available) return

    setLoading(true)
    setError('')
    try {
      const results = await invoke<UsageSnapshot[]>('get_all_usage_snapshots')
      const prevByKey = new Map<string, UsageSnapshot>()
      for (const snapshot of snapshotsRef.current) {
        if (snapshot.status !== 'error') {
          prevByKey.set(snapshot.provider.startsWith('openai') ? 'openai' : 'claude', snapshot)
        }
      }

      const displayed = results.map((snap) => {
        if (snap.status === 'error') {
          const key = snap.provider.startsWith('openai') ? 'openai' : 'claude'
          const cached = prevByKey.get(key)
          if (cached) {
            return { ...cached, status: 'cached', message: `(cached) ${snap.message ?? 'rate limited'}` }
          }
        }
        return snap
      })

      snapshotsRef.current = displayed
      setSnapshots(displayed)
      await invoke('update_tray_status', { status: getTrayStatus(displayed) })

      const worstSnap = results
        .filter((s) => s.status !== 'error')
        .sort((a, b) => a.remainingPercent - b.remainingPercent)[0]
      if (worstSnap) {
        await invoke('notify_if_needed', { snapshot: worstSnap })
      }
    } catch (e) {
      setError(String(e))
    } finally {
      setLoading(false)
    }
  }, [oauthStatus.available])

  const loadSettings = async (): Promise<AppSettings | null> => {
    try {
      const s = await invoke<AppSettings>('get_settings')
      setSettings(s)
      return s
    } catch (e) {
      setError(String(e))
      return null
    }
  }

  const loadOAuthStatus = async () => {
    try {
      const status = await invoke<OAuthStatus>('get_oauth_status')
      setOauthStatus(status)
      if (!status.available) {
        setSnapshots([])
      }
    } catch (e) {
      setOauthStatus({
        available: false,
        source: 'none',
        message: String(e),
      })
    }
  }

  useEffect(() => {
    const init = async () => {
      const p = await invoke<string>('get_platform')
      setPlatform(p)
      await loadSettings()
      await loadOAuthStatus()
    }
    void init()

    const appWindow = getCurrentWindow()
    const unlisten = appWindow.onFocusChanged(({ payload: focused }) => {
      if (!focused) {
        setTimeout(() => {
          if (!document.hasFocus()) {
            // Don't auto-hide during OAuth login flow — user needs to paste the code
            if (claudeLoginStepRef.current === 'waiting') return
            if (openaiLoginStepRef.current === 'logging_in') return
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
    void refresh()
    const timer = window.setInterval(() => {
      void refresh()
    }, Math.max(15, settings.refreshIntervalSec) * 1000)

    return () => {
      window.clearInterval(timer)
    }
  }, [refresh, settings.refreshIntervalSec])

  const toggleSettings = () => {
    setSettingsOpen((prev) => !prev)
  }

  const saveSettings = async () => {
    try {
      const saved = await invoke<AppSettings>('save_settings', { settings })
      setSettings(saved)
      await loadOAuthStatus()
      setError('')
      await refresh()
    } catch (e) {
      setError(String(e))
    }
  }

  const startClaudeLogin = async () => {
    try {
      setClaudeLoginError('')
      const array = new Uint8Array(32)
      crypto.getRandomValues(array)
      const verifier = base64urlEncode(array)
      setCodeVerifier(verifier)

      const digest = await crypto.subtle.digest(
        'SHA-256',
        new TextEncoder().encode(verifier),
      )
      const challenge = base64urlEncode(new Uint8Array(digest))

      const stateArray = new Uint8Array(32)
      crypto.getRandomValues(stateArray)
      const state = base64urlEncode(stateArray)
      setOauthState(state)

      const params = new URLSearchParams({
        code: 'true',
        client_id: CLAUDE_OAUTH_CLIENT_ID,
        response_type: 'code',
        redirect_uri: CLAUDE_OAUTH_REDIRECT_URI,
        scope: CLAUDE_OAUTH_SCOPES,
        code_challenge: challenge,
        code_challenge_method: 'S256',
        state,
      })

      const authUrl = `https://claude.ai/oauth/authorize?${params.toString()}`
      await invoke('open_external_url', { url: authUrl })
      setClaudeLoginStep('waiting')
    } catch (e) {
      setClaudeLoginError(String(e))
    }
  }

  const submitClaudeCode = async () => {
    try {
      setClaudeLoginError('')
      setClaudeLoginStep('exchanging')

      let code = claudeAuthCode.trim()
      // Allow pasting the full callback URL
      const urlMatch = code.match(/[?&]code=([^&#]+)/)
      if (urlMatch) {
        code = urlMatch[1]
      }
      // Handle CODE#STATE format — strip the #state suffix
      const hashIdx = code.indexOf('#')
      if (hashIdx > 0) {
        code = code.substring(0, hashIdx)
      }

      await invoke('exchange_claude_auth_code', { code, codeVerifier, oauthState })
      setClaudeLoginStep('success')
      setClaudeAuthCode('')
      await loadOAuthStatus()
      await refresh()
      setTimeout(() => setClaudeLoginStep('idle'), 3000)
    } catch (e) {
      setClaudeLoginStep('waiting')
      setClaudeLoginError(String(e))
    }
  }

  const startOpenAILogin = async () => {
    try {
      setOpenaiLoginError('')
      setOpenaiLoginStep('logging_in')

      const array = new Uint8Array(32)
      crypto.getRandomValues(array)
      const verifier = base64urlEncode(array)

      const digest = await crypto.subtle.digest(
        'SHA-256',
        new TextEncoder().encode(verifier),
      )
      const challenge = base64urlEncode(new Uint8Array(digest))

      const stateArray = new Uint8Array(32)
      crypto.getRandomValues(stateArray)
      const state = base64urlEncode(stateArray)

      await invoke('start_openai_oauth', {
        codeChallenge: challenge,
        codeVerifier: verifier,
        oauthState: state,
      })

      setOpenaiLoginStep('success')
      await loadOAuthStatus()
      await refresh()
      setTimeout(() => setOpenaiLoginStep('idle'), 3000)
    } catch (e) {
      setOpenaiLoginStep('idle')
      setOpenaiLoginError(String(e))
    }
  }

  const windowLabel = useMemo(() => {
    const m: Record<string, string> = {
      hourly5: 'Current Session',
      weekly: 'Weekly (7d)',
      code_review_weekly: 'Code Review (7d)',
    }
    return m[settings.selectedWindowType] ?? settings.selectedWindowType
  }, [settings.selectedWindowType])

  const lastUpdated = useMemo(() => {
    const ts = snapshots.filter((s) => s.status !== 'error').map((s) => s.updatedAt)
    if (ts.length === 0) return '--'
    return fmtTime(Math.max(...ts))
  }, [snapshots])

  return (
    <div
      className={`panel ${worstStatus}`}
      onMouseEnter={() => void invoke('panel_mouse_enter')}
      onMouseLeave={() => void invoke('panel_mouse_leave')}
    >
      <div className="content">
        <div className="header">
          <div>
            <h1>AI Usage Monitor</h1>
            <p>{windowLabel} &middot; {settings.refreshIntervalSec}s</p>
          </div>
          <button type="button" className="ghost" onClick={toggleSettings}>
            {settingsOpen ? 'Back' : 'Settings'}
          </button>
        </div>

        {!settingsOpen && (
          <>
            {snapshots.length === 0 && !loading && oauthStatus.available && (
              <div className="card muted">No usage data yet...</div>
            )}

            {snapshots.map((snap, i) => (
              <div key={i} className={`card ${snap.status === 'error' ? 'card-error' : ''} ${snap.status === 'cached' ? 'card-cached' : ''}`}>
                <div className="row">
                  <span className="provider-name">{snap.provider}</span>
                  {snap.status !== 'error' && (
                    <strong>
                      {snap.used.toFixed(1)}% used
                      {snap.status === 'cached' && <span className="cached-badge"> (stale)</span>}
                    </strong>
                  )}
                </div>
                {snap.status === 'error' ? (
                  <div className="card-error-msg">{snap.message}</div>
                ) : (
                  <>
                    <div className="bar">
                      <div
                        className={`bar-fill ${snap.status === 'cached' ? 'warning' : snap.status}`}
                        style={{
                          width: `${Math.max(0, Math.min(100, snap.used))}%`,
                        }}
                      />
                    </div>
                    <div className="meta">
                      <span>
                        Used: {snap.used.toFixed(1)} / {snap.limit.toFixed(0)}
                      </span>
                      <span>Reset: {fmtTime(snap.resetAt)}</span>
                    </div>
                    {snap.status === 'cached' && snap.message && (
                      <div className="cached-msg">{snap.message}</div>
                    )}
                  </>
                )}
              </div>
            ))}

            <div className="status-row">
              {loading ? 'Refreshing...' : `Updated: ${lastUpdated}`}
              <span className="version-tag">v0.1.2</span>
            </div>

            {!oauthStatus.available && (
              <div className="warning">
                <p>No OAuth providers found.</p>
                <p>{oauthStatus.message}</p>
                <button type="button" onClick={() => void loadOAuthStatus()}>
                  Retry
                </button>
              </div>
            )}
          </>
        )}

        {settingsOpen && (
          <div className="settings">
            <label>
              Codex auth path (optional)
              <input
                type="text"
                value={settings.codexAuthPath}
                onChange={(e) =>
                  setSettings({ ...settings, codexAuthPath: e.target.value })
                }
                placeholder={platform === 'macos' ? '~/.codex/auth.json' : 'C:\\Users\\you\\.codex\\auth.json'}
              />
            </label>
            <div className="login-section">
              {openaiLoginStep === 'idle' && (
                <button type="button" className="login-btn" onClick={() => void startOpenAILogin()}>
                  Login with OpenAI
                </button>
              )}
              {openaiLoginStep === 'logging_in' && (
                <p className="login-hint">Waiting for browser login...</p>
              )}
              {openaiLoginStep === 'success' && (
                <p className="login-success">OpenAI login successful!</p>
              )}
              {openaiLoginError && <p className="login-error">{openaiLoginError}</p>}
            </div>

            <label>
              Claude auth path (optional)
              <input
                type="text"
                value={settings.claudeAuthPath}
                onChange={(e) =>
                  setSettings({ ...settings, claudeAuthPath: e.target.value })
                }
                placeholder={platform === 'macos' ? '~/.claude/.credentials.json' : 'C:\\Users\\you\\.claude\\.credentials.json'}
              />
            </label>

            <div className="login-section">
              {claudeLoginStep === 'idle' && (
                <button type="button" className="login-btn" onClick={() => void startClaudeLogin()}>
                  Login with Claude
                </button>
              )}
              {claudeLoginStep === 'waiting' && (
                <>
                  <p className="login-hint">Paste the code from the browser callback page:</p>
                  <input
                    type="text"
                    value={claudeAuthCode}
                    onChange={(e) => setClaudeAuthCode(e.target.value)}
                    placeholder="Paste code or callback URL"
                    onKeyDown={(e) => {
                      if (e.key === 'Enter' && claudeAuthCode.trim()) {
                        void submitClaudeCode()
                      }
                    }}
                  />
                  <div className="login-actions">
                    <button type="button" onClick={() => void submitClaudeCode()} disabled={!claudeAuthCode.trim()}>
                      Submit
                    </button>
                    <button type="button" className="ghost" onClick={() => { setClaudeLoginStep('idle'); setClaudeLoginError('') }}>
                      Cancel
                    </button>
                  </div>
                </>
              )}
              {claudeLoginStep === 'exchanging' && (
                <p className="login-hint">Exchanging code for tokens...</p>
              )}
              {claudeLoginStep === 'success' && (
                <p className="login-success">Claude login successful!</p>
              )}
              {claudeLoginError && <p className="login-error">{claudeLoginError}</p>}
            </div>

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
            <label>
              Usage window
              <select
                value={settings.selectedWindowType}
                onChange={(e) =>
                  setSettings({
                    ...settings,
                    selectedWindowType: e.target.value as
                      | 'hourly5'
                      | 'weekly'
                      | 'code_review_weekly',
                  })
                }
              >
                <option value="hourly5">Current Session</option>
                <option value="weekly">Weekly (7-day)</option>
                <option value="code_review_weekly">Code Review (7-day)</option>
              </select>
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
