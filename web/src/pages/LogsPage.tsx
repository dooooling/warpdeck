// Logs 页（P10-006/007 / DESIGN §19.7）：
// 运行时日志：源选择（manager/instance:*）+ 历史分页 + 实时流（log.line）。
// 展示区占满页面高度，支持级别着色与换行/不换行切换。

import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'

import { useLogHistory, useLogSources } from '../api/queries'
import { useSseEvents, type SseEnvelope } from '../api/sse'
import type { LogLineEvent } from '../api/types'
import { EmptyState, ErrorState, Spinner } from '../components/Feedback'
import { classifyLogLine } from '../lib/format'

/** 实时行客户端上限（P10-007：浏览器不保留无限日志）。 */
const LIVE_LINES_MAX = 500

/** 距底部视为此阈值内即视为"在底部"（维持跟随 / 触发恢复）。 */
const BOTTOM_THRESHOLD_PX = 50

const LEVEL_CLASS: Record<string, string> = {
  error: 'log-line-error',
  warn: 'log-line-warn',
  info: 'log-line-info',
  debug: 'log-line-debug',
}

export function LogsPage() {
  const { t } = useTranslation()
  const sourcesQuery = useLogSources()
  const [source, setSource] = useState<string>('manager')
  const [offset, setOffset] = useState(0)
  const [live, setLive] = useState(true)
  const [wrap, setWrap] = useState(false)
  const [liveLines, setLiveLines] = useState<LogLineEvent[]>([])
  const scrollRef = useRef<HTMLDivElement>(null)
  const programmaticScroll = useRef(false)

  const history = useLogHistory(source, live ? 0 : offset)

  const connState = useSseEvents((env: SseEnvelope) => {
    if (env.type !== 'log.line') {
      return
    }
    const data = env.data as unknown as LogLineEvent
    if (data.source !== source) {
      return
    }
    setLiveLines((prev) => {
      const next = [...prev, data]
      return next.length > LIVE_LINES_MAX ? next.slice(-LIVE_LINES_MAX) : next
    })
  })

  const changeSource = (next: string) => {
    setSource(next)
    setOffset(0)
    setLiveLines([])
  }

  // 自动跟随（P10-007）：
  // - 滚动停留/回到底部区域 → 实时跟随；
  // - 用户向上滚动离开底部 → 自动暂停跟随，切历史分页回看；
  // - 回到底部 → 自动恢复跟随（无需手工勾选）。
  const handleScroll = () => {
    const el = scrollRef.current
    if (!el || programmaticScroll.current) {
      return
    }
    const distFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight
    if (distFromBottom <= BOTTOM_THRESHOLD_PX) {
      setLive(true)
    } else if (live) {
      setLive(false)
    }
  }

  // 实时模式自动滚底（程序滚动不触发走位判定）。
  useEffect(() => {
    if (live && scrollRef.current) {
      const el = scrollRef.current
      programmaticScroll.current = true
      el.scrollTop = el.scrollHeight
      requestAnimationFrame(() => {
        programmaticScroll.current = false
      })
    }
  }, [live, liveLines.length])

  const liveActive = live && connState === 'open'
  const shownLines = liveActive ? liveLines.map((l) => l.line) : (history.data?.lines ?? [])

  // 每行按级别着色（display:block 保持每行占满横幅）。
  const rendered = shownLines.map((line, i) => {
    const level = classifyLogLine(line)
    const cls = level === null ? 'log-line' : `log-line ${LEVEL_CLASS[level]}`
    return (
      <span key={i} className={cls}>
        {line}
      </span>
    )
  })

  return (
    <div className="page logs-page">
      <header className="page-header">
        <h1>{t('logs.title')}</h1>
      </header>
      <div className="runtime-logs">
        <div className="filter-row">
          <div className="form-field">
            <label htmlFor="logs-source">{t('logs.source')}</label>
            <select
              id="logs-source"
              value={source}
              onChange={(e) => changeSource(e.target.value)}
              disabled={sourcesQuery.isLoading}
            >
              {sourcesQuery.data?.map((s) => (
                <option key={s.source} value={s.source}>
                  {s.source}
                </option>
              )) ?? <option value="manager">manager</option>}
            </select>
          </div>
          <div className="form-field live-toggle">
            <span className={live ? 'follow-badge live' : 'follow-badge paused'}>
              {live ? t('logs.following') : t('logs.paused')}
            </span>
          </div>
          <div className="form-field live-toggle">
            <label>
              <input
                type="checkbox"
                checked={wrap}
                onChange={() => setWrap((v) => !v)}
              />
              {t('logs.wrap')}
            </label>
          </div>
          {!live && history.data?.has_more ? (
            <button
              type="button"
              className="btn btn-sm"
              onClick={() => setOffset((o) => o + 1)}
              disabled={history.isFetching}
            >
              {t('logs.older')}
            </button>
          ) : null}
          {!live && offset > 0 ? (
            <button type="button" className="btn btn-sm" onClick={() => setOffset(0)}>
              {t('logs.newest')}
            </button>
          ) : null}
        </div>
        <div className={`log-view mono${wrap ? ' log-wrap' : ''}`} ref={scrollRef} onScroll={handleScroll}>
          {liveActive ? (
            shownLines.length === 0 ? (
              <EmptyState title={t('logs.waitingLines', { source })} />
            ) : (
              <pre>{rendered}</pre>
            )
          ) : history.isLoading ? (
            <Spinner label={t('logs.loadingHistory')} />
          ) : history.isError ? (
            <ErrorState message={history.error.message} />
          ) : shownLines.length === 0 ? (
            <EmptyState title={t('logs.noLines', { source })} />
          ) : (
            <pre>{rendered}</pre>
          )}
        </div>
      </div>
    </div>
  )
}