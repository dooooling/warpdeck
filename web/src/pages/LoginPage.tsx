// Login 页（P9-004 / DESIGN §20.3-20.4）：bad password / rate limited /
// server unavailable 三类错误分别处理。

import { useState, type FormEvent } from 'react'
import { Navigate, useLocation, useNavigate } from 'react-router'
import { useTranslation } from 'react-i18next'

import { useAuth } from '../auth/useAuth'
import { ApiError } from '../api/client'
import { loginSchema } from '../lib/validation'
import { Spinner } from '../components/Feedback'

export function LoginPage() {
  const { t } = useTranslation()
  const { setup, login } = useAuth()
  const navigate = useNavigate()
  const location = useLocation()

  // RequireAuth 踢来时带 `state.from`（原页面路径），登录成功回跳；直接访问
  // /login 则回默认首页。
  const from =
    typeof location.state === 'object' &&
    location.state !== null &&
    'from' in location.state &&
    typeof location.state.from === 'string'
      ? location.state.from
      : '/dashboard'

  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({})
  const [submitError, setSubmitError] = useState<string | null>(null)

  if (setup.isLoading) {
    return <Spinner label={t('login.checkSetup')} />
  }
  if (!setup.data?.initialized) {
    return <Navigate to="/setup" replace />
  }

  const onSubmit = (e: FormEvent) => {
    e.preventDefault()
    setFieldErrors({})
    setSubmitError(null)
    const parsed = loginSchema(t).safeParse({ username, password })
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
    login.mutate(
      { username, password },
      {
        onSuccess: () => navigate(from, { replace: true }),
        onError: (err) => {
          if (err instanceof ApiError) {
            switch (err.code) {
              case 'UNAUTHORIZED':
                setSubmitError(t('login.errUnauthorized'))
                break
              case 'FORBIDDEN':
                setSubmitError(t('login.errForbidden'))
                break
              default:
                setSubmitError(err.message)
            }
          } else {
            setSubmitError(t('login.errUnavailable'))
          }
        },
      },
    )
  }

  return (
    <div className="auth-shell">
      <form className="card auth-card" onSubmit={onSubmit} noValidate>
        <h1 className="auth-title">WarpDeck</h1>
        <p className="auth-subtitle">{t('login.subtitle')}</p>

        {submitError ? (
          <div className="form-error" role="alert">
            {submitError}
          </div>
        ) : null}

        <div className="form-field">
          <label htmlFor="login-username">{t('login.username')}</label>
          <input
            id="login-username"
            type="text"
            autoComplete="username"
            autoFocus
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            aria-invalid={fieldErrors.username ? true : undefined}
          />
          {fieldErrors.username ? <p className="field-error">{fieldErrors.username}</p> : null}
        </div>

        <div className="form-field">
          <label htmlFor="login-password">{t('login.password')}</label>
          <input
            id="login-password"
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            aria-invalid={fieldErrors.password ? true : undefined}
          />
          {fieldErrors.password ? <p className="field-error">{fieldErrors.password}</p> : null}
        </div>

        <button type="submit" className="btn btn-primary" disabled={login.isPending}>
          {login.isPending ? t('login.submitting') : t('login.submit')}
        </button>
      </form>
    </div>
  )
}