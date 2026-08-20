import { describe, expect, it } from 'vitest'

import { classifyLogLine } from './format'

describe('classifyLogLine', () => {
  it('recognizes tracing fmt level tokens', () => {
    expect(classifyLogLine('2026-08-19T23:00:00.000Z  INFO warpdeck: started')).toBe('info')
    expect(classifyLogLine('2026-08-19T23:00:00.000Z  WARN gost: retrying')).toBe('warn')
    expect(classifyLogLine('2026-08-19T23:00:00.000Z ERROR warpdeck: boom')).toBe('error')
    expect(classifyLogLine('2026-08-19T23:00:00.000Z DEBUG warpdeck: trace out')).toBe('debug')
  })

  it('recognizes bracket and key=value forms', () => {
    expect(classifyLogLine('[ERROR] connection reset')).toBe('error')
    expect(classifyLogLine('(WARNING) disk low')).toBe('warn')
    expect(classifyLogLine('level=info listening on 18080')).toBe('info')
    expect(classifyLogLine('"level":"error" message failed')).toBe('error')
    expect(classifyLogLine('severity: critical timeout reached')).toBe('error')
    expect(classifyLogLine('panic: index out of range')).toBe('error')
    expect(classifyLogLine('fatal: cannot start daemon')).toBe('error')
  })

  it('does not match words that merely contain level names', () => {
    expect(classifyLogLine('GET /api/v1/info/users 200')).toBeNull()
    expect(classifyLogLine('debugging loopback interface')).toBeNull()
    expect(classifyLogLine('user/info.json not found')).toBeNull()
    expect(classifyLogLine('startup complete')).toBeNull()
  })

  it('stays case-insensitive', () => {
    expect(classifyLogLine('Error: timeout')).toBe('error')
    expect(classifyLogLine('Warn: backoff')).toBe('warn')
  })
})