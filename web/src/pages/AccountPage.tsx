// Accounts 页（v0.2 §17.6 / DESIGN §19.6）：多账号档案列表 + 创建/编辑/删除。
//
// 秘密边界：GET 永不回填 license/client id/client secret 明文——只显示
// 「已配置」mask；输入新值 = 替换，清除 = 二次确认（DESIGN §22.5）。
// 默认档（id=1）不可删除；仍被 enabled 实例绑定的档案删除会被后端 409 拒绝，
// 前端直接展示该错误（不伪造成功）。

import { useState, type FormEvent } from 'react'
import { useTranslation } from 'react-i18next'

import {
  useAccountProfiles,
  useCreateAccountProfileMutation,
  useDeleteAccountProfileMutation,
  useUpdateAccountProfileMutation,
} from '../api/queries'
import type { AccountMode, AccountProfileView, AccountProfileWriteRequest } from '../api/types'
import { accountSchema, type AccountFormValues } from '../lib/validation'
import { ErrorState, Spinner } from '../components/Feedback'
import { ConfirmDialog } from '../components/ConfirmDialog'
import { useNotify } from '../components/useNotify'

const MODES: { value: AccountMode; tKey: string; tHint: string }[] = [
  { value: 'free', tKey: 'account.modeFree', tHint: 'account.modeFreeHint' },
  { value: 'warp_plus', tKey: 'account.modePlus', tHint: 'account.modePlusHint' },
  { value: 'zero_trust', tKey: 'account.modeZeroTrust', tHint: 'account.modeZeroTrustHint' },
]

interface ProfileFormState {
  name: string
  mode: AccountMode
  license: string
  zeroTrustOrg: string
  clientId: string
  clientSecret: string
}

const EMPTY_FORM: ProfileFormState = {
  name: '',
  mode: 'free',
  license: '',
  zeroTrustOrg: '',
  clientId: '',
  clientSecret: '',
}

function modeLabel(t: (k: string) => string, mode: AccountMode): string {
  const key =
    mode === 'warp_plus' ? 'account.modePlus' : mode === 'zero_trust' ? 'account.modeZeroTrust' : 'account.modeFree'
  return t(key)
}

export function AccountPage() {
  const { t } = useTranslation()
  const profiles = useAccountProfiles()
  const create = useCreateAccountProfileMutation()
  const notify = useNotify()
  // free 全局唯一：仅当系统尚不存在 free 档（默认档被升级后）才允许创建；
  // 编辑 free 档本身时保留该选项。
  const hasFree = profiles.data?.some((p) => p.mode === 'free') ?? false

  // modal 状态：undefined = 关闭；null = 新建；{ profile } = 编辑。
  const [editing, setEditing] = useState<AccountProfileView | null | undefined>(undefined)
  const [form, setForm] = useState<ProfileFormState>(EMPTY_FORM)
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({})
  const [submitError, setSubmitError] = useState<string | null>(null)
  const [deleting, setDeleting] = useState<AccountProfileView | null>(null)
  const updateMut = useUpdateAccountProfileMutation(editing?.id ?? -1)
  const deleteMut = useDeleteAccountProfileMutation(deleting?.id ?? -1)

  const openCreate = () => {
    setForm({ ...EMPTY_FORM, mode: hasFree ? 'zero_trust' : 'free' })
    setFieldErrors({})
    setSubmitError(null)
    setEditing(null)
  }

  const openEdit = (profile: AccountProfileView) => {
    setForm({
      name: profile.name,
      mode: profile.mode,
      // org 非 secret 可回填；凭证明文不回填。
      license: '',
      zeroTrustOrg: profile.zero_trust_org ?? '',
      clientId: '',
      clientSecret: '',
    })
    setFieldErrors({})
    setSubmitError(null)
    setEditing(profile)
  }

  const set = <K extends keyof ProfileFormState>(key: K, value: ProfileFormState[K]) =>
    setForm((prev) => ({ ...prev, [key]: value }))

  /** 提交请求：空白 secret 字段不发送（= 保持后端现有值）。 */
  const buildRequest = (values: AccountFormValues, mode: AccountMode): AccountProfileWriteRequest => {
    return {
      mode,
      zero_trust_org: values.zeroTrustOrg.trim() || undefined,
      license: values.license.trim() || undefined,
      client_id: values.clientId.trim() || undefined,
      client_secret: values.clientSecret || undefined,
    }
  }

  // create 模式时 secret 必须提供；edit 模式时空白 = 保持（已配置时无需重填）。
  // 后端仍是最终权威（mode 校验见 §16.9）。
  const onSubmit = (e: FormEvent) => {
    e.preventDefault()
    setFieldErrors({})
    setSubmitError(null)
    if (!form.name.trim()) {
      setFieldErrors({ name: t('validation.nameRequired') })
      return
    }
    const parsed = accountSchema(t).safeParse(form)
    if (!parsed.success) {
      const errors: Record<string, string> = {}
      for (const issue of parsed.error.issues) {
        const path = issue.path[0]?.toString() ?? 'form'
        // 编辑已配置档案：空白 = 保持现有 secret，不要求重填。
        if (
          editing &&
          ((path === 'license' && editing.license_configured) ||
            (path === 'clientId' && editing.client_id_configured) ||
            (path === 'clientSecret' && editing.client_secret_configured))
        ) {
          continue
        }
        if (!errors[path]) {
          errors[path] = issue.message
        }
      }
      setFieldErrors(errors)
      return
    }
    const payload = buildRequest(parsed.data, form.mode)
    if (editing) {
      // PATCH 需要 name（后端 always 校验 name）；保持原名即可。
      updateMut.mutate(
        { ...payload, name: form.name.trim() },
        {
          onSuccess: () => {
            notify('success', t('accounts.updated'))
            setEditing(undefined)
          },
          onError: (err) => setSubmitError(err.message),
        },
      )
      return
    }
    create.mutate(
      { ...payload, name: form.name.trim() },
      {
        onSuccess: () => {
          notify('success', t('accounts.created'))
          setEditing(undefined)
        },
        onError: (err) => setSubmitError(err.message),
      },
    )
  }

  if (profiles.isLoading) {
    return <Spinner label={t('accounts.loading')} />
  }
  if (profiles.isError) {
    return <ErrorState message={profiles.error.message} onRetry={() => void profiles.refetch()} />
  }

  return (
    <div className="page">
      <header className="page-header">
        <h1>{t('accounts.title')}</h1>
        <button type="button" className="btn btn-primary" onClick={openCreate}>
          + {t('accounts.add')}
        </button>
      </header>

      <p className="hint">{t('accounts.warpPlusWarning')}</p>

      {profiles.data && profiles.data.length > 0 ? (
        <table className="table card">
          <thead>
            <tr>
              <th>{t('accounts.thName')}</th>
              <th>{t('accounts.thMode')}</th>
              <th>{t('accounts.thCredentials')}</th>
              <th>{t('accounts.thInstances')}</th>
              <th>{t('accounts.thActions')}</th>
            </tr>
          </thead>
          <tbody>
            {profiles.data.map((p) => (
              <tr key={p.id}>
                <td>
                  {p.name}
                  {p.default ? <span className="tag"> {t('accounts.defaultTag')}</span> : null}
                </td>
                <td>{modeLabel(t, p.mode)}</td>
                <td className="mono">
                  {p.mode === 'warp_plus'
                    ? p.license_configured
                      ? t('accounts.licenseSet')
                      : t('accounts.notSet')
                    : p.mode === 'zero_trust'
                      ? `${p.client_id_configured && p.client_secret_configured ? t('accounts.ztSet') : t('accounts.notSet')}${p.zero_trust_org ? ` · ${p.zero_trust_org}` : ''}`
                      : t('accounts.freeNoCreds')}
                </td>
                <td>{p.instance_count}</td>
                <td>
                  <div className="btn-row">
                    <button
                      type="button"
                      className="btn btn-sm"
                      disabled={p.default || p.instance_count > 0}
                      title={
                        p.default
                          ? t('accounts.freeReadOnly')
                          : p.instance_count > 0
                            ? t('accounts.boundReadOnly')
                            : undefined
                      }
                      onClick={() => openEdit(p)}
                    >
                      {t('accounts.edit')}
                    </button>
                    <button
                      type="button"
                      className="btn btn-sm btn-danger"
                      disabled={p.default || p.instance_count > 0}
                      title={
                        p.default
                          ? t('accounts.defaultProtected')
                          : p.instance_count > 0
                            ? t('accounts.boundReadOnly')
                            : undefined
                      }
                      onClick={() => setDeleting(p)}
                    >
                      {t('accounts.delete')}
                    </button>
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}

      {editing !== undefined ? (
        <div className="modal-overlay" role="presentation" onMouseDown={() => setEditing(undefined)}>
          <form
            className="modal card"
            onSubmit={onSubmit}
            onMouseDown={(e) => e.stopPropagation()}
            aria-labelledby="profile-modal-title"
          >
            <h2 className="modal-title" id="profile-modal-title">
              {editing ? t('accounts.editTitle', { name: editing.name }) : t('accounts.addTitle')}
            </h2>

            {submitError ? (
              <div className="form-error" role="alert">
                {submitError}
              </div>
            ) : null}

            <div className="form-field">
              <label htmlFor="profile-name">{t('accounts.nameLabel')}</label>
              <input
                id="profile-name"
                type="text"
                value={form.name}
                onChange={(e) => set('name', e.target.value)}
                placeholder={t('accounts.namePlaceholder')}
                aria-invalid={fieldErrors.name ? true : undefined}
                autoFocus
              />
              {fieldErrors.name ? <p className="field-error">{fieldErrors.name}</p> : null}
            </div>

            <fieldset className="form-fieldset">
              <legend className="visually-hidden">{t('account.modeLegend')}</legend>
              {MODES.filter((m) => m.value !== 'free' || !hasFree || editing?.mode === 'free').map((mode) => (
                <div key={mode.value} className="radio-option">
                  <label htmlFor={`profile-mode-${mode.value}`}>
                    <input
                      id={`profile-mode-${mode.value}`}
                      type="radio"
                      name="profile-mode"
                      value={mode.value}
                      checked={form.mode === mode.value}
                      onChange={() => set('mode', mode.value)}
                    />
                    {t(mode.tKey)}
                    <span className="hint"> — {t(mode.tHint)}</span>
                  </label>
                </div>
              ))}
            </fieldset>

            {form.mode === 'warp_plus' ? (
              <div className="form-field">
                <label htmlFor="profile-license">
                  {t('account.licenseKey')}{' '}
                  {editing?.license_configured ? t('account.configuredReplace') : ''}
                </label>
                <input
                  id="profile-license"
                  type="password"
                  value={form.license}
                  onChange={(e) => set('license', e.target.value)}
                  placeholder={
                    editing?.license_configured
                      ? t('account.licenseConfigured')
                      : t('account.licensePlaceholder')
                  }
                  autoComplete="off"
                  aria-invalid={fieldErrors.license ? true : undefined}
                />
                {fieldErrors.license ? <p className="field-error">{fieldErrors.license}</p> : null}
              </div>
            ) : null}

            {form.mode === 'zero_trust' ? (
              <>
                <div className="form-field">
                  <label htmlFor="profile-org">{t('account.organization')}</label>
                  <input
                    id="profile-org"
                    type="text"
                    value={form.zeroTrustOrg}
                    onChange={(e) => set('zeroTrustOrg', e.target.value)}
                    placeholder={t('account.orgPlaceholder')}
                    aria-invalid={fieldErrors.zeroTrustOrg ? true : undefined}
                  />
                  {fieldErrors.zeroTrustOrg ? (
                    <p className="field-error">{fieldErrors.zeroTrustOrg}</p>
                  ) : null}
                </div>
                <div className="form-field">
                  <label htmlFor="profile-client-id">
                    {t('account.clientId')}{' '}
                    {editing?.client_id_configured ? t('account.configuredReplace') : ''}
                  </label>
                  <input
                    id="profile-client-id"
                    type="password"
                    value={form.clientId}
                    onChange={(e) => set('clientId', e.target.value)}
                    autoComplete="off"
                    aria-invalid={fieldErrors.clientId ? true : undefined}
                  />
                  {fieldErrors.clientId ? (
                    <p className="field-error">{fieldErrors.clientId}</p>
                  ) : null}
                </div>
                <div className="form-field">
                  <label htmlFor="profile-client-secret">
                    {t('account.clientSecret')}{' '}
                    {editing?.client_secret_configured ? t('account.configuredReplace') : ''}
                  </label>
                  <input
                    id="profile-client-secret"
                    type="password"
                    value={form.clientSecret}
                    onChange={(e) => set('clientSecret', e.target.value)}
                    autoComplete="off"
                    aria-invalid={fieldErrors.clientSecret ? true : undefined}
                  />
                  {fieldErrors.clientSecret ? (
                    <p className="field-error">{fieldErrors.clientSecret}</p>
                  ) : null}
                </div>
              </>
            ) : null}

            <p className="hint">{t('accounts.restartHint')}</p>

            <div className="modal-actions">
              <button type="button" className="btn" onClick={() => setEditing(undefined)}>
                {t('accounts.cancel')}
              </button>
              <button type="submit" className="btn btn-primary" disabled={create.isPending}>
                {t('accounts.save')}
              </button>
            </div>
          </form>
        </div>
      ) : null}

      <ConfirmDialog
        open={deleting !== null}
        title={t('accounts.deleteTitle', { name: deleting?.name ?? '' })}
        message={
          deleting && deleting.instance_count > 0
            ? t('accounts.deleteBounded', { count: deleting.instance_count })
            : t('accounts.deleteMessage')
        }
        confirmLabel={t('accounts.deleteConfirm')}
        danger
        busy={deleteMut.isPending}
        onConfirm={() => {
          if (!deleting) {
            return
          }
          deleteMut.mutate(undefined, {
            onSuccess: () => {
              notify('success', t('accounts.deleted', { name: deleting.name }))
              setDeleting(null)
            },
            onError: (err) => {
              // 后端 409（仍被引用）直接浮出错误，不伪造成功。
              notify('error', err.message)
              setDeleting(null)
            },
          })
        }}
        onCancel={() => setDeleting(null)}
      />
    </div>
  )
}