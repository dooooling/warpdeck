// 显示格式化小工具。

export function formatLatency(latencyMs: number | null): string {
  return latencyMs === null ? '—' : `${latencyMs} ms`
}

export function formatUptime(totalSecs: number): string {
  const days = Math.floor(totalSecs / 86_400)
  const hours = Math.floor((totalSecs % 86_400) / 3_600)
  const minutes = Math.floor((totalSecs % 3_600) / 60)
  if (days > 0) {
    return `${days}d ${hours}h`
  }
  if (hours > 0) {
    return `${hours}h ${minutes}m`
  }
  return `${minutes}m`
}

export function formatTimestamp(iso: string): string {
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) {
    return iso
  }
  return date.toLocaleString()
}

/** 日志行级别（UI 着色用）。`none` = 无级别可识别，使用默认色。 */
export type LogLevel = 'error' | 'warn' | 'info' | 'debug' | 'none'

// 匹配 tracing fmt（`...Z  INFO comp: msg`）、`[ERROR]`、`(WARN)`、`level=error`、
// `"level":"warn"`、`severity: critical` 等常见形态；前后字符限定为行首或分隔符
// 以降低误报（如路径 `user/info.json`、单词 `debugging` 不命中）。
const LOG_LEVEL_RE =
  /(?:^|[\s[(:==",{,])(trace|debug|info|warn(?:ing)?|error|fatal|critical|panic)(?:[\s\]):=;,"}]|$)/i

/** 从日志行文本推断级别（大小写不敏感），无匹配返回 `null`。 */
export function classifyLogLine(line: string): Exclude<LogLevel, 'none'> | null {
  const match = line.match(LOG_LEVEL_RE)
  if (!match) {
    return null
  }
  const token = match[1].toLowerCase()
  switch (token) {
    case 'error':
    case 'fatal':
    case 'critical':
    case 'panic':
      return 'error'
    case 'warn':
    case 'warning':
      return 'warn'
    case 'info':
      return 'info'
    default:
      return 'debug'
  }
}