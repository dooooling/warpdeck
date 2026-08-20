// 危险操作二次确认对话框（DESIGN §22.5）。

import { useEffect, useRef } from 'react'
import { useTranslation } from 'react-i18next'

export interface ConfirmDialogProps {
  open: boolean
  title: string
  message: string
  confirmLabel: string
  /** 危险操作：红色确认按钮（删除实例/清除凭据等）。 */
  danger?: boolean
  busy?: boolean
  onConfirm: () => void
  onCancel: () => void
}

export function ConfirmDialog({
  open,
  title,
  message,
  confirmLabel,
  danger,
  busy,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const { t } = useTranslation()
  const cancelRef = useRef<HTMLButtonElement>(null)

  useEffect(() => {
    if (open) {
      cancelRef.current?.focus()
    }
  }, [open])

  if (!open) {
    return null
  }

  return (
    <div className="modal-overlay" role="presentation" onMouseDown={onCancel}>
      <div
        className="modal"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="confirm-title"
        aria-describedby="confirm-message"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <h2 className="modal-title" id="confirm-title">
          {title}
        </h2>
        <p className="modal-message" id="confirm-message">
          {message}
        </p>
        <div className="modal-actions">
          <button
            type="button"
            className="btn"
            ref={cancelRef}
            onClick={onCancel}
            disabled={busy}
          >
            {t('common.cancel')}
          </button>
          <button
            type="button"
            className={`btn ${danger ? 'btn-danger' : ''}`}
            onClick={onConfirm}
            disabled={busy}
          >
            {busy ? t('common.working') : confirmLabel}
          </button>
        </div>
      </div>
    </div>
  )
}