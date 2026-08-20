// Dashboard（P9-005 / DESIGN §19.2）：只展示决策需要的数据。

import { Link } from 'react-router'
import { useTranslation } from 'react-i18next'

import { useInstances, useProxyConfig, useSystemStatus } from '../api/queries'
import { EmptyState, ErrorState, Spinner, StateBadge } from '../components/Feedback'
import { formatLatency, formatUptime } from '../lib/format'

export function DashboardPage() {
  const { t } = useTranslation()
  const status = useSystemStatus()
  const instances = useInstances()
  const proxy = useProxyConfig()

  if (status.isLoading || instances.isLoading || proxy.isLoading) {
    return <Spinner label={t('dashboard.loading')} />
  }
  if (status.isError) {
    return <ErrorState message={status.error.message} onRetry={() => void status.refetch()} />
  }
  if (!status.data) {
    return null
  }
  const counts = status.data.instances
  const runningInstances = instances.data ?? []
  const proxyStatus =
    proxy.data === undefined
      ? t('common.dash')
      : proxy.data.socks5_enabled && proxy.data.http_enabled
        ? t('dashboard.proxyRunning')
        : t('dashboard.proxyPartial')
  const version = status.data.version

  return (
    <div className="page">
      <header className="page-header">
        <h1>{t('dashboard.title')}</h1>
        <span className="page-meta">
          {t('dashboard.meta', { version, uptime: formatUptime(status.data.uptime_secs) })}
        </span>
      </header>

      <div className="stat-grid">
        <div className="stat-card">
          <span className="stat-value">{counts.total}</span>
          <span className="stat-label">{t('dashboard.statInstances')}</span>
        </div>
        <div className="stat-card">
          <span className="stat-value stat-good">{counts.healthy}</span>
          <span className="stat-label">{t('dashboard.statHealthy')}</span>
        </div>
        <div className="stat-card">
          <span className="stat-value stat-bad">{counts.failed}</span>
          <span className="stat-label">{t('dashboard.statFailed')}</span>
        </div>
        <div className="stat-card">
          <span className="stat-value">{proxyStatus}</span>
          <span className="stat-label">{t('dashboard.statProxy')}</span>
        </div>
      </div>

      <section className="card">
        <h2>{t('dashboard.cardInstances')}</h2>
        {runningInstances.length === 0 ? (
          <EmptyState title={t('dashboard.emptyTitle')} hint={t('dashboard.emptyHint')} />
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>{t('dashboard.thName')}</th>
                <th>{t('dashboard.thState')}</th>
                <th>{t('dashboard.thExitIp')}</th>
                <th>{t('dashboard.thColo')}</th>
                <th>{t('dashboard.thLatency')}</th>
              </tr>
            </thead>
            <tbody>
              {runningInstances.map((inst) => (
                <tr key={inst.id}>
                  <td>
                    <Link className="table-link" to={`/instances/${inst.id}`}>
                      {inst.name}
                    </Link>
                  </td>
                  <td>
                    <StateBadge state={inst.runtime_state} />
                  </td>
                  <td>{inst.exit_ip ?? t('common.dash')}</td>
                  <td>{inst.colo ?? t('common.dash')}</td>
                  <td>{formatLatency(inst.latency_ms)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      <section className="card">
        <h2>{t('dashboard.cardProxy')}</h2>
        {proxy.data === undefined ? (
          <EmptyState title={t('proxy.empty')} />
        ) : (
          <table className="table">
            <tbody>
              <tr>
                <th scope="row">{t('proxy.socks5')}</th>
                <td>:11080</td>
                <td>{proxy.data.socks5_enabled ? t('common.on') : t('common.off')}</td>
              </tr>
              <tr>
                <th scope="row">{t('proxy.http')}</th>
                <td>:18080</td>
                <td>{proxy.data.http_enabled ? t('common.on') : t('common.off')}</td>
              </tr>
            </tbody>
          </table>
        )}
      </section>
    </div>
  )
}