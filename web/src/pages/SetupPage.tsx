// Setup 页（P9-003 / DESIGN §20.1）：首次创建管理员；完成后永久关闭。

import { useState, type FormEvent } from 'react'
import { Navigate, useNavigate } from 'react-router'
import { useTranslation } from 'react-i18next'

import { useAuth } from '../auth/useAuth'
import { ApiError } from '../api/client'
import { setupSchema } from '../lib/validation'
import { Spinner } from '../components/Feedback'
import { useNotify } from '../components/useNotify'

export function SetupPage() {
  const { t } = useTranslation()
  const { setup, createAdmin } = useAuth()
  const navigate = useNavigate()
  const notify = useNotify()

  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [confirmPassword, setConfirmPassword] = useState('')
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({})
  const [submitError, setSubmitError] = useState<string | null>(null)

  if (setup.isLoading) {
    return <Spinner label={t('setup.checkStatus')} />
  }
  if (setup.data?.initialized) {
    return <Navigate to="/login" replace />
  }

  const onSubmit = (e: FormEvent) => {
    e.preventDefault()
    setFieldErrors({})
    setSubmitError(null)
    const parsed = setupSchema(t).safeParse({ username, password, confirmPassword })
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
    createAdmin.mutate(
      { username, password },
      {
        onSuccess: () => {
          notify('success', t('setup.created'))
          navigate('/login')
        },
        onError: (err) => {
          if (err instanceof ApiError && err.status === 409) {
            setSubmitError(t('setup.alreadyDone'))
          } else {
            setSubmitError(err.message)
          }
        },
      },
    )
  }

  return (
    <div className="auth-shell">
      <form className="card auth-card" onSubmit={onSubmit} noValidate>
        <h1 className="auth-title">{t('setup.title')}</h1>
        <p className="auth-subtitle">{t('setup.subtitle')}</p>

        {submitError ? (
          <div className="form-error" role="alert">
            {submitError}
          </div>
        ) : null}

        <div className="form-field">
          <label htmlFor="setup-username">{t('setup.username')}</label>
          <input
            id="setup-username"
            type="text"
            autoComplete="username"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            aria-invalid={fieldErrors.username ? true : undefined}
          />
          {fieldErrors.username ? <p className="field-error">{fieldErrors.username}</p> : null}
        </div>

        <div className="form-field">
          <label htmlFor="setup-password">{t('setup.password')}</label>
          <input
            id="setup-password"
            type="password"
            autoComplete="new-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            aria-invalid={fieldErrors.password ? true : undefined}
          />
          {fieldErrors.password ? <p className="field-error">{fieldErrors.password}</p> : null}
        </div>

        <div className="form-field">
          <label htmlFor="setup-confirm">{t('setup.confirmPassword')}</label>
          <input
            id="setup-confirm"
            type="password"
            autoComplete="new-password"
            value={confirmPassword}
            onChange={(e) => setConfirmPassword(e.target.value)}
            aria-invalid={fieldErrors.confirmPassword ? true : undefined}
          />
          {fieldErrors.confirmPassword ? (
            <p className="field-error">{fieldErrors.confirmPassword}</p>
          ) : null}
        </div>

        <button type="submit" className="btn btn-primary" disabled={createAdmin.isPending}>
          {createAdmin.isPending ? t('setup.submitting') : t('setup.submit')}
        </button>
      </form>
    </div>
  )
}