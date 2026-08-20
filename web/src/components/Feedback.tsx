// 通用反馈组件（P9-013）：Loading / Empty / Error 三态 + 状态徽章（P9-014）。

import { useTranslation } from 'react-i18next'

import type { RuntimeState } from '../api/types'

export function Spinner({ label }: { label?: string }) {
  const { t } = useTranslation()
  return (
    <div className="spinner" role="status" aria-live="polite">
      <span className="spinner-dot" aria-hidden="true" />
      <span>{label ?? t('common.loading')}…</span>
    </div>
  )
}

export function EmptyState({ title, hint }: { title: string; hint?: string }) {
  return (
    <div className="empty-state">
      <p className="empty-state-title">{title}</p>
      {hint ? <p className="empty-state-hint">{hint}</p> : null}
    </div>
  )
}

export function ErrorState({
  message,
  onRetry,
}: {
  message: string
  onRetry?: () => void
}) {
  const { t } = useTranslation()
  return (
    <div className="error-state" role="alert">
      <p className="error-state-title">{t('feedback.errorTitle')}</p>
      <p className="error-state-message">{message}</p>
      {onRetry ? (
        <button type="button" className="btn" onClick={onRetry}>
          {t('feedback.retry')}
        </button>
      ) : null}
    </div>
  )
}

/** 状态 → 文案 + 视觉（颜色只作为增强，文字恒存在——P9-014 无障碍基线）。 */
const STATE_CLASS: Record<RuntimeState, string> = {
  healthy: 'badge-green',
  degraded: 'badge-amber',
  failed: 'badge-red',
  starting: 'badge-blue',
  registering: 'badge-blue',
  connecting: 'badge-blue',
  stopping: 'badge-blue',
  stopped: 'badge-gray',
  disabled: 'badge-gray',
}

export function StateBadge({ state }: { state: RuntimeState }) {
  const { t } = useTranslation()
  const label = t(`state.${state}`)
  return (
    <span className={`badge ${STATE_CLASS[state]}`} role="status" aria-label={t('feedback.stateAria', { state: label })}>
      {label}
    </span>
  )
}

/** 期望状态（desired_state）徽章：running / stopped。 */
export function DesiredBadge({ desired }: { desired: 'running' | 'stopped' }) {
  const { t } = useTranslation()
  const label = t(`state.${desired}`)
  return (
    <span
      className={`badge ${desired === 'running' ? 'badge-blue' : 'badge-gray'}`}
      role="status"
      aria-label={t('feedback.desiredAria', { desired: label })}
    >
      {label}
    </span>
  )
}