// Instance Detail（P9-008 / DESIGN §19.4）。
//
// 后端 InstanceView 提供 Desired/Runtime/Exit IP/Colo/Latency/Last error；
// 内部端口按固定规则 40000 + id 显示（AGENTS.md 基线）。

import { useState } from 'react'
import { Link, useParams } from 'react-router'
import { useTranslation } from 'react-i18next'

import {
  useAccountProfiles,
  useDeleteInstanceMutation,
  useInstance,
  useRebindInstanceMutation,
  useRestartInstanceMutation,
  useStartInstanceMutation,
  useStopInstanceMutation,
} from '../api/queries'
import { ApiError } from '../api/client'
import { DesiredBadge, EmptyState, ErrorState, Spinner, StateBadge } from '../components/Feedback'
import { ConfirmDialog } from '../components/ConfirmDialog'
import { useNotify } from '../components/useNotify'
import { formatLatency } from '../lib/format'

export function InstanceDetailPage() {
  const { t } = useTranslation()
  const { id: idParam } = useParams()
  const id = Number(idParam)
  const instance = useInstance(id)
  const notify = useNotify()

  const [confirmingDelete, setConfirmingDelete] = useState(false)
  const [bindProfileId, setBindProfileId] = useState<string>('')
  const start = useStartInstanceMutation()
  const stop = useStopInstanceMutation()
  const restart = useRestartInstanceMutation()
  const del = useDeleteInstanceMutation()
  const profiles = useAccountProfiles()
  const rebind = useRebindInstanceMutation()

  if (!Number.isInteger(id) || id < 0) {
    return <EmptyState title={t('detail.invalidId')} />
  }
  if (instance.isLoading) {
    return <Spinner label={t('detail.loading')} />
  }
  if (instance.isError) {
    if (instance.error instanceof ApiError && instance.error.code === 'NOT_FOUND') {
      return <EmptyState title={t('detail.notFound')} hint={t('detail.notFoundHint')} />
    }
    return (
      <ErrorState message={instance.error.message} onRetry={() => void instance.refetch()} />
    )
  }
  if (!instance.data) {
    return null
  }
  const inst = instance.data

  // 下拉初值同步当前绑定；默认档显示为占位项（'default' 哨兵 → 后端显式 null）。
  const boundId = inst.account?.profile_id ?? 1
  const currentBind = boundId === 1 ? 'default' : String(boundId)
  const effectiveBind = bindProfileId === '' ? currentBind : bindProfileId
  const defaultProfile = profiles.data?.find((p) => p.default) ?? null

  const runAction = (fn: (i: number) => void, action: string) => {
    fn(id)
    notify('info', t('detail.actionRequested', { action }))
  }

  return (
    <div className="page">
      <header className="page-header">
        <h1>{inst.name}</h1>
        <Link className="table-link" to="/instances">
          {t('detail.back')}
        </Link>
      </header>

      <section className="card">
        <h2>{t('detail.status')}</h2>
        <dl className="detail-list">
          <div>
            <dt>{t('detail.runtimeState')}</dt>
            <dd>
              <StateBadge state={inst.runtime_state} />
            </dd>
          </div>
          <div>
            <dt>{t('detail.desiredState')}</dt>
            <dd>
              <DesiredBadge desired={inst.desired_state} />
            </dd>
          </div>
          <div>
            <dt>{t('detail.exitIpV4')}</dt>
            <dd className="mono">{inst.exit_ip_v4 ?? t('common.dash')}</dd>
          </div>
          <div>
            <dt>{t('detail.exitIpV6')}</dt>
            <dd className="mono">{inst.exit_ip_v6 ?? t('common.dash')}</dd>
          </div>
          <div>
            <dt>{t('detail.colo')}</dt>
            <dd>{inst.colo ?? t('common.dash')}</dd>
          </div>
          <div>
            <dt>{t('detail.latency')}</dt>
            <dd>{formatLatency(inst.latency_ms)}</dd>
          </div>
          <div>
            <dt>{t('detail.internalPort')}</dt>
            <dd>{40000 + inst.id}</dd>
          </div>
          <div>
            <dt>{t('detail.account')}</dt>
            <dd>
              {inst.account
                ? `${inst.account.name} (${inst.account.mode})`
                : t('common.dash')}
            </dd>
          </div>
          <div>
            <dt>{t('detail.lastError')}</dt>
            <dd className="mono">{inst.last_error ?? t('common.dash')}</dd>
          </div>
        </dl>
      </section>

      <section className="card">
        <h2>{t('detail.actions')}</h2>
        <div className="btn-row">
          <button
            type="button"
            className="btn"
            onClick={() => runAction((i) => start.mutate(i), t('detail.start'))}
          >
            {t('detail.start')}
          </button>
          <button
            type="button"
            className="btn"
            onClick={() => runAction((i) => stop.mutate(i), t('detail.stop'))}
          >
            {t('detail.stop')}
          </button>
          <button
            type="button"
            className="btn"
            onClick={() => runAction((i) => restart.mutate(i), t('detail.restart'))}
          >
            {t('detail.restart')}
          </button>
          <button
            type="button"
            className="btn btn-danger"
            onClick={() => setConfirmingDelete(true)}
          >
            {t('detail.delete')}
          </button>
        </div>
      </section>

      <section className="card">
        <h2>{t('detail.accountBinding')}</h2>
        <p className="hint">{t('detail.accountBindingHint')}</p>
        <div className="form-field">
          <label htmlFor="detail-account-profile">{t('detail.accountProfile')}</label>
          <select
            id="detail-account-profile"
            value={effectiveBind}
            onChange={(e) => setBindProfileId(e.target.value)}
            disabled={rebind.isPending}
          >
            <option value="default">
              {t('detail.profileDefault', { name: defaultProfile?.name ?? 'free' })}
            </option>
            {profiles.data
              ?.filter((p) => !p.default)
              .map((p) => (
                <option key={p.id} value={String(p.id)}>
                  {p.name} ({p.mode})
                </option>
              ))}
          </select>
        </div>
        <button
          type="button"
          className="btn btn-primary"
          disabled={rebind.isPending || bindProfileId === '' || effectiveBind === currentBind}
          onClick={() => {
            rebind.mutate(
              { id, input: { account_profile_id: effectiveBind === 'default' ? null : Number(effectiveBind) } },
              {
                onSuccess: () => {
                  notify('success', t('detail.accountBound'))
                  setBindProfileId('')
                },
                onError: (err) => notify('error', err.message),
              },
            )
          }}
        >
          {rebind.isPending ? t('detail.binding') : t('detail.bind')}
        </button>
      </section>

      <ConfirmDialog
        open={confirmingDelete}
        title={t('detail.deleteTitle', { name: inst.name })}
        message={t('detail.deleteMessage')}
        confirmLabel={t('detail.deleteConfirm')}
        danger
        busy={del.isPending}
        onConfirm={() => {
          del.mutate(id, {
            onSuccess: () => notify('success', t('detail.deleted')),
            onSettled: () => setConfirmingDelete(false),
          })
        }}
        onCancel={() => setConfirmingDelete(false)}
      />
    </div>
  )
}