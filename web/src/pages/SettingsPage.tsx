// Settings 页（P9-012）：合并原 Proxy 页（P9-009）配置到单一设置页。
// 代理配置：端口（11080/18080）只读展示——Host 映射由 Compose/.env 管理，
// UI 不提供修改。auth 密码为 secret：GET 不回填，只显示「已配置」状态。
// 另含系统信息（版本/运行时间/Web 端口）、安全说明与语言切换。

import { useEffect, useState, type FormEvent } from 'react'
import { useTranslation } from 'react-i18next'

import {
  useProxyConfig,
  useSystemStatus,
  useUpdateProxyMutation,
} from '../api/queries'
import { proxySchema, type ProxyFormValues } from '../lib/validation'
import type { UpdateProxyRequest } from '../api/types'
import { changeLanguage, type AppLanguage } from '../i18n'
import { ErrorState, Spinner } from '../components/Feedback'
import { ConfirmDialog } from '../components/ConfirmDialog'
import { useNotify } from '../components/useNotify'
import { formatUptime } from '../lib/format'

const LANGUAGES: { value: AppLanguage; label: string }[] = [
  { value: 'en', label: 'English' },
  { value: 'zh', label: '中文' },
]

interface ProxyFormState {
  socks5Enabled: boolean
  httpEnabled: boolean
  authEnabled: boolean
  username: string
  password: string
  allowedIpsText: string
  maxConnections: string
  maxRps: string
}

const EMPTY_FORM: ProxyFormState = {
  socks5Enabled: true,
  httpEnabled: true,
  authEnabled: false,
  username: '',
  password: '',
  allowedIpsText: '',
  maxConnections: '',
  maxRps: '',
}

export function SettingsPage() {
  const { t, i18n } = useTranslation()
  const status = useSystemStatus()
  const config = useProxyConfig()
  const save = useUpdateProxyMutation()
  const notify = useNotify()

  const [form, setForm] = useState<ProxyFormState>(EMPTY_FORM)
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({})
  const [submitError, setSubmitError] = useState<string | null>(null)
  const [clearingPassword, setClearingPassword] = useState(false)

  useEffect(() => {
    if (config.data) {
      setForm({
        socks5Enabled: config.data.socks5_enabled,
        httpEnabled: config.data.http_enabled,
        authEnabled: config.data.auth_enabled,
        username: '',
        password: '',
        allowedIpsText: config.data.allowed_ips.join('\n'),
        maxConnections: config.data.max_connections?.toString() ?? '',
        maxRps: config.data.max_rps?.toString() ?? '',
      })
    }
  }, [config.data])

  if (status.isLoading || config.isLoading) {
    return <Spinner label={t('settings.loading')} />
  }
  if (status.isError || config.isError) {
    return (
      <ErrorState message={config.error?.message ?? status.error?.message ?? 'Unknown error'} />
    )
  }
  if (!status.data || !config.data) {
    return null
  }
  const currentLang: AppLanguage = i18n.language.startsWith('zh') ? 'zh' : 'en'

  const set = <K extends keyof ProxyFormState>(key: K, value: ProxyFormState[K]) =>
    setForm((prev) => ({ ...prev, [key]: value }))

  const buildRequest = (values: ProxyFormValues): UpdateProxyRequest => {
    const changed: UpdateProxyRequest = {
      socks5_enabled: form.socks5Enabled,
      http_enabled: form.httpEnabled,
      auth_enabled: form.authEnabled,
      allowed_ips: values.allowedIpsText,
      max_connections: values.maxConnections === 0 ? null : values.maxConnections,
      max_rps: values.maxRps === 0 ? null : values.maxRps,
    }
    const username = form.username.trim()
    if (username) {
      changed.username = username
    }
    if (form.password) {
      changed.password = form.password
    }
    return changed
  }

  const onSubmit = (e: FormEvent) => {
    e.preventDefault()
    setFieldErrors({})
    setSubmitError(null)
    const parsed = proxySchema(t).safeParse({
      allowedIpsText: form.allowedIpsText,
      maxConnections: form.maxConnections === '' ? null : Number(form.maxConnections),
      maxRps: form.maxRps === '' ? null : Number(form.maxRps),
    })
    if (!parsed.success) {
      const errors: Record<string, string> = {}
      for (const issue of parsed.error.issues) {
        const path = issue.path[0]?.toString() ?? 'form'
        if (!errors[path]) {
          errors[path] = issue.message
        }
      }
      setFieldErrors(errors)
      return
    }
    save.mutate(buildRequest(parsed.data), {
      onSuccess: () => {
        notify('success', t('proxy.saved'))
        setForm((prev) => ({ ...prev, password: '' }))
      },
      onError: (err) => setSubmitError(err.message),
    })
  }

  const clearPassword = () => {
    setClearingPassword(false)
    save.mutate({ password: '' }, {
      onSuccess: () => notify('success', t('proxy.cleared')),
      onError: (err) => notify('error', err.message),
    })
  }

  return (
    <div className="page">
      <header className="page-header">
        <h1>{t('settings.title')}</h1>
      </header>

      <section className="card">
        <h2>{t('proxy.listeners')}</h2>
        <p className="hint">{t('proxy.listenersHint')}</p>
        <dl className="detail-list">
          <div>
            <dt>{t('proxy.socks5')}</dt>
            <dd>
              <span className="mono">:11080</span> · {form.socks5Enabled ? t('common.on') : t('common.off')}
            </dd>
          </div>
          <div>
            <dt>{t('proxy.http')}</dt>
            <dd>
              <span className="mono">:18080</span> · {form.httpEnabled ? t('common.on') : t('common.off')}
            </dd>
          </div>
          {config.data.actual ? (
            <div>
              <dt>{t('proxy.actualStatus')}</dt>
              <dd>
                <span className={`mono state-${config.data.actual.status}`}>
                  {config.data.actual.status}
                  {config.data.actual.pid != null ? ` (pid ${config.data.actual.pid})` : ''}
                </span>
                {config.data.actual.reason ? (
                  <span className="hint"> — {config.data.actual.reason}</span>
                ) : null}
              </dd>
            </div>
          ) : null}
        </dl>
      </section>

      {status.data?.last_apply_error ? (
        <div className="form-error" role="alert">
          <strong>{t('proxy.lastApplyError')}:</strong>{' '}
          <span className="mono">{status.data.last_apply_error.error}</span>
          <span className="hint"> ({status.data.last_apply_error.at_rfc3339})</span>
        </div>
      ) : null}

      {submitError ? (
        <div className="form-error" role="alert">
          {submitError}
        </div>
      ) : null}

      <form className="card" onSubmit={onSubmit} noValidate>
        <h2>{t('proxy.settings')}</h2>

        <div className="form-row">
          <label htmlFor="proxy-socks5">
            <input
              id="proxy-socks5"
              type="checkbox"
              checked={form.socks5Enabled}
              onChange={(e) => set('socks5Enabled', e.target.checked)}
            />
            {t('proxy.enableSocks5')}
          </label>
          <label htmlFor="proxy-http">
            <input
              id="proxy-http"
              type="checkbox"
              checked={form.httpEnabled}
              onChange={(e) => set('httpEnabled', e.target.checked)}
            />
            {t('proxy.enableHttp')}
          </label>
        </div>

        <fieldset className="form-fieldset">
          <legend>{t('proxy.authentication')}</legend>
          {!form.authEnabled && (form.socks5Enabled || form.httpEnabled) ? (
            <p className="form-warning" role="alert">
              {t('proxy.authWarning')}
            </p>
          ) : null}
          <label htmlFor="proxy-auth">
            <input
              id="proxy-auth"
              type="checkbox"
              checked={form.authEnabled}
              onChange={(e) => set('authEnabled', e.target.checked)}
            />
            {t('proxy.requireAuth')}
          </label>
          {form.authEnabled ? (
            <>
              <div className="form-field">
                <label htmlFor="proxy-username">{t('proxy.username')}</label>
                <input
                  id="proxy-username"
                  type="text"
                  value={form.username}
                  placeholder={
                    config.data.auth_configured
                      ? t('proxy.pwdKeep')
                      : t('proxy.usernamePlaceholder')
                  }
                  onChange={(e) => set('username', e.target.value)}
                  autoComplete="off"
                />
              </div>
              <div className="form-field">
                <label htmlFor="proxy-password">
                  {t('proxy.password')}{' '}
                  {config.data.auth_configured ? t('proxy.pwdConfigured') : ''}
                </label>
                <div className="btn-row">
                  <input
                    id="proxy-password"
                    type="password"
                    value={form.password}
                    placeholder={
                      config.data.auth_configured
                        ? t('proxy.pwdReplace')
                        : t('proxy.pwdPlaceholder')
                    }
                    onChange={(e) => set('password', e.target.value)}
                    autoComplete="new-password"
                  />
                  {config.data.auth_configured ? (
                    <button
                      type="button"
                      className="btn btn-sm btn-danger"
                      onClick={() => setClearingPassword(true)}
                    >
                      {t('proxy.clear')}
                    </button>
                  ) : null}
                </div>
              </div>
            </>
          ) : null}
        </fieldset>

        <div className="form-field">
          <label htmlFor="proxy-allowlist">{t('proxy.allowedNetworks')}</label>
          <textarea
            id="proxy-allowlist"
            rows={4}
            value={form.allowedIpsText}
            placeholder={'192.168.1.0/24\n10.0.0.10/32'}
            onChange={(e) => set('allowedIpsText', e.target.value)}
            aria-invalid={fieldErrors.allowedIpsText ? true : undefined}
          />
          <p className="hint">{t('proxy.allowedHint')}</p>
          {fieldErrors.allowedIpsText ? (
            <p className="field-error">{fieldErrors.allowedIpsText}</p>
          ) : null}
        </div>

        <div className="form-row">
          <div className="form-field">
            <label htmlFor="proxy-max-conn">{t('proxy.maxConnections')}</label>
            <input
              id="proxy-max-conn"
              type="number"
              min={0}
              value={form.maxConnections}
              placeholder={t('proxy.noLimit')}
              onChange={(e) => set('maxConnections', e.target.value)}
              aria-invalid={fieldErrors.maxConnections ? true : undefined}
            />
            {fieldErrors.maxConnections ? (
              <p className="field-error">{fieldErrors.maxConnections}</p>
            ) : null}
          </div>
          <div className="form-field">
            <label htmlFor="proxy-max-rps">{t('proxy.maxRps')}</label>
            <input
              id="proxy-max-rps"
              type="number"
              min={0}
              value={form.maxRps}
              placeholder={t('proxy.noLimit')}
              onChange={(e) => set('maxRps', e.target.value)}
              aria-invalid={fieldErrors.maxRps ? true : undefined}
            />
            {fieldErrors.maxRps ? <p className="field-error">{fieldErrors.maxRps}</p> : null}
          </div>
        </div>

        <button type="submit" className="btn btn-primary" disabled={save.isPending}>
          {save.isPending ? t('proxy.saving') : t('proxy.save')}
        </button>
      </form>

      <section className="card">
        <h2>{t('settings.server')}</h2>
        <dl className="detail-list">
          <div>
            <dt>{t('settings.version')}</dt>
            <dd className="mono">{status.data.version}</dd>
          </div>
          <div>
            <dt>{t('settings.uptime')}</dt>
            <dd>{formatUptime(status.data.uptime_secs)}</dd>
          </div>
          <div>
            <dt>{t('settings.webPort')}</dt>
            <dd className="mono">:9000 (in-container)</dd>
          </div>
        </dl>
      </section>

      <section className="card">
        <h2>{t('settings.language')}</h2>
        <p className="hint">{t('settings.languageHint')}</p>
        <div className="btn-row">
          {LANGUAGES.map((lang) => (
            <button
              key={lang.value}
              type="button"
              className={`btn btn-sm${currentLang === lang.value ? ' btn-primary' : ''}`}
              onClick={() => changeLanguage(lang.value)}
            >
              {lang.label}
            </button>
          ))}
        </div>
      </section>

      <ConfirmDialog
        open={clearingPassword}
        title={t('proxy.clearTitle')}
        message={t('proxy.clearMessage')}
        confirmLabel={t('proxy.clearConfirm')}
        danger
        busy={save.isPending}
        onConfirm={clearPassword}
        onCancel={() => setClearingPassword(false)}
      />
    </div>
  )
}