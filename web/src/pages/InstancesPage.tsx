// Instances 列表 + Add Instance（P9-006/007 / DESIGN §19.3）。
//
// 注意：后端 CreateInstanceRequest 只有 name（创建即期望 running、enabled，
// 内部端口 40000+id 自动分配）——auto start/auto restart 属期望状态字段
// （P7 后端暂未暴露），表单按后端契约保持最小。

import { useState, type FormEvent } from 'react'
import { Link } from 'react-router'
import { useTranslation } from 'react-i18next'

import {
  useAccountProfiles,
  useCreateInstanceMutation,
  useDeleteInstanceMutation,
  useInstances,
  useRestartInstanceMutation,
  useStartInstanceMutation,
  useStopInstanceMutation,
} from '../api/queries'
import { instanceNameSchema } from '../lib/validation'
import { EmptyState, ErrorState, Spinner, StateBadge, DesiredBadge } from '../components/Feedback'
import { ConfirmDialog } from '../components/ConfirmDialog'
import { useNotify } from '../components/useNotify'
import { formatLatency } from '../lib/format'
import type { RuntimeState } from '../api/types'

const STOPPED_STATES: RuntimeState[] = ['stopped', 'disabled', 'failed']
const ACTIVE_STATES: RuntimeState[] = [
  'starting',
  'registering',
  'connecting',
  'healthy',
  'degraded',
  'stopping',
]

export function InstancesPage() {
  const { t } = useTranslation()
  const instances = useInstances()
  const profiles = useAccountProfiles()
  const notify = useNotify()
  const create = useCreateInstanceMutation()

  const [showAdd, setShowAdd] = useState(false)
  const [name, setName] = useState('')
  const [nameError, setNameError] = useState<string | null>(null)
  const [profileId, setProfileId] = useState<number | null>(null)
  const defaultProfile = profiles.data?.find((p) => p.default) ?? null
  const [addError, setAddError] = useState<string | null>(null)

  const [deleting, setDeleting] = useState<{ id: number; name: string } | null>(null)
  const start = useStartInstanceMutation()
  const stop = useStopInstanceMutation()
  const restart = useRestartInstanceMutation()
  const del = useDeleteInstanceMutation()

  const runAction = (fn: (id: number) => void, id: number, action: string) => {
    fn(id)
    notify('info', t('instances.actionRequested', { action, id }))
  }

  const onAddSubmit = (e: FormEvent) => {
    e.preventDefault()
    setNameError(null)
    setAddError(null)
    const parsed = instanceNameSchema(t).safeParse(name)
    if (!parsed.success) {
      setNameError(parsed.error.issues[0]?.message ?? t('validation.nameRequired'))
      return
    }
    create.mutate(
      { name: parsed.data, account_profile_id: profileId },
      {
        onSuccess: () => {
          notify('success', t('instances.created', { name: parsed.data }))
          setName('')
          setProfileId(null)
          setShowAdd(false)
        },
        onError: (err) => setAddError(err.message),
      },
    )
  }

  return (
    <div className="page">
      <header className="page-header">
        <h1>{t('instances.title')}</h1>
        <button type="button" className="btn btn-primary" onClick={() => setShowAdd(true)}>
          + {t('instances.add')}
        </button>
      </header>

      {instances.isLoading ? <Spinner label={t('instances.loading')} /> : null}
      {instances.isError ? (
        <ErrorState message={instances.error.message} onRetry={() => void instances.refetch()} />
      ) : null}
      {instances.isSuccess && (instances.data?.length ?? 0) === 0 ? (
        <EmptyState title={t('instances.emptyTitle')} hint={t('instances.emptyHint')} />
      ) : null}
      {instances.data && instances.data.length > 0 ? (
        <table className="table card">
          <thead>
            <tr>
              <th>{t('instances.thName')}</th>
              <th>{t('instances.thState')}</th>
              <th>{t('instances.thDesired')}</th>
              <th>{t('instances.thExitIp')}</th>
              <th>{t('instances.thColo')}</th>
              <th>{t('instances.thLatency')}</th>
              <th>{t('instances.thAccount')}</th>
              <th>{t('instances.thActions')}</th>
            </tr>
          </thead>
          <tbody>
            {instances.data.map((inst) => (
              <tr key={inst.id}>
                <td>
                  <Link className="table-link" to={`/instances/${inst.id}`}>
                    {inst.name}
                  </Link>
                </td>
                <td>
                  <StateBadge state={inst.runtime_state} />
                </td>
                <td>
                  <DesiredBadge desired={inst.desired_state} />
                </td>
                <td>
                  <span className="mono">{inst.exit_ip_v4 ?? t('common.dash')}</span>
                  {inst.exit_ip_v6 && (
                    <>
                      <br />
                      <span className="mono" style={{ color: 'var(--body-mid)' }}>
                        {inst.exit_ip_v6}
                      </span>
                    </>
                  )}
                </td>
                <td>{inst.colo ?? t('common.dash')}</td>
                <td>{formatLatency(inst.latency_ms)}</td>
                <td>
                  {inst.account ? (
                    <span title={t('instances.accountMode', { mode: inst.account.mode })}>
                      {inst.account.name}
                    </span>
                  ) : (
                    t('common.dash')
                  )}
                </td>
                <td>
                  <div className="btn-row">
                    {STOPPED_STATES.includes(inst.runtime_state) ? (
                      <button
                        type="button"
                        className="btn btn-sm"
                        onClick={() => runAction((i) => start.mutate(i), inst.id, t('instances.start'))}
                      >
                        {t('instances.start')}
                      </button>
                    ) : null}
                    {ACTIVE_STATES.includes(inst.runtime_state) ? (
                      <>
                        <button
                          type="button"
                          className="btn btn-sm"
                          onClick={() => runAction((i) => stop.mutate(i), inst.id, t('instances.stop'))}
                        >
                          {t('instances.stop')}
                        </button>
                        <button
                          type="button"
                          className="btn btn-sm"
                          onClick={() =>
                            runAction((i) => restart.mutate(i), inst.id, t('instances.restart'))
                          }
                        >
                          {t('instances.restart')}
                        </button>
                      </>
                    ) : null}
                    <button
                      type="button"
                      className="btn btn-sm btn-danger"
                      onClick={() => setDeleting({ id: inst.id, name: inst.name })}
                    >
                      {t('instances.delete')}
                    </button>
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}

      {showAdd ? (
        <div className="modal-overlay" role="presentation" onMouseDown={() => setShowAdd(false)}>
          <form
            className="modal card"
            onSubmit={onAddSubmit}
            onMouseDown={(e) => e.stopPropagation()}
            aria-labelledby="add-instance-title"
          >
            <h2 className="modal-title" id="add-instance-title">
              {t('instances.addTitle')}
            </h2>
            <p className="modal-message">{t('instances.addMessage')}</p>

            {addError ? (
              <div className="form-error" role="alert">
                {addError}
              </div>
            ) : null}

            <div className="form-field">
              <label htmlFor="add-instance-name">{t('instances.nameLabel')}</label>
              <input
                id="add-instance-name"
                type="text"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder={t('instances.namePlaceholder')}
                aria-invalid={nameError ? true : undefined}
                autoFocus
              />
              {nameError ? <p className="field-error">{nameError}</p> : null}
            </div>

            <div className="form-field">
              <label htmlFor="add-instance-profile">{t('instances.profileLabel')}</label>
              <select
                id="add-instance-profile"
                value={profileId ?? ''}
                onChange={(e) => {
                  const v = e.target.value
                  setProfileId(v === '' ? null : Number(v))
                }}
              >
                <option value="">
                  {t('instances.profileDefault', { name: defaultProfile?.name ?? 'free' })}
                </option>
                {profiles.data
                  ?.filter((p) => !p.default)
                  .map((p) => (
                    <option key={p.id} value={p.id}>
                      {p.name} ({p.mode})
                    </option>
                  ))}
              </select>
              <p className="field-error">{profiles.error?.message ?? ''}</p>
              <p className="hint">{t('instances.profileHint')}</p>
            </div>

            <div className="modal-actions">
              <button type="button" className="btn" onClick={() => setShowAdd(false)}>
                {t('instances.cancel')}
              </button>
              <button type="submit" className="btn btn-primary">
                {t('instances.create')}
              </button>
            </div>
          </form>
        </div>
      ) : null}

      <ConfirmDialog
        open={deleting !== null}
        title={t('instances.deleteTitle', { name: deleting?.name ?? '' })}
        message={t('instances.deleteMessage')}
        confirmLabel={t('instances.deleteConfirm')}
        danger
        busy={del.isPending}
        onConfirm={() => {
          if (!deleting) {
            return
          }
          del.mutate(deleting.id, {
            onSuccess: () => notify('success', t('instances.deleted', { id: deleting.id })),
            onError: (err) => notify('error', err.message),
            onSettled: () => setDeleting(null),
          })
        }}
        onCancel={() => setDeleting(null)}
      />
    </div>
  )
}