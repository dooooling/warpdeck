// 表单校验单元测试（P9 §14.4 / DESIGN §22.3）。

import { describe, expect, it } from 'vitest'

import i18n from '../i18n'
import {
  accountSchema,
  instanceNameSchema,
  loginSchema,
  proxyAllowedIpsSchema,
  proxySchema,
  setupSchema,
} from './validation'

// 测试环境 navigator.language = en-US → i18n.t 返回英文文案。
const t = i18n.t.bind(i18n)

describe('setupSchema', () => {
  it('accepts valid admin credentials', () => {
    const result = setupSchema(t).safeParse({
      username: 'admin',
      password: 'correct-horse-123',
      confirmPassword: 'correct-horse-123',
    })
    expect(result.success).toBe(true)
  })

  it('rejects short passwords', () => {
    const result = setupSchema(t).safeParse({
      username: 'admin',
      password: 'short',
      confirmPassword: 'short',
    })
    expect(result.success).toBe(false)
    expect(JSON.stringify(result.error?.issues)).toContain('at least 8')
  })

  it('rejects mismatched confirmation', () => {
    const result = setupSchema(t).safeParse({
      username: 'admin',
      password: 'long-enough-123',
      confirmPassword: 'different-456',
    })
    expect(result.success).toBe(false)
    expect(JSON.stringify(result.error?.issues)).toContain('do not match')
  })

  it('rejects empty username', () => {
    const result = setupSchema(t).safeParse({
      username: '   ',
      password: 'long-enough-123',
      confirmPassword: 'long-enough-123',
    })
    expect(result.success).toBe(false)
    expect(JSON.stringify(result.error?.issues)).toContain('required')
  })
})

describe('loginSchema', () => {
  it('requires a username', () => {
    const result = loginSchema(t).safeParse({ username: '', password: 'x' })
    expect(result.success).toBe(false)
  })
})

describe('instanceNameSchema', () => {
  it('trims and accepts valid names', () => {
    expect(instanceNameSchema(t).safeParse('  warp-1 ').success).toBe(true)
  })

  it('rejects empty and overlong names', () => {
    expect(instanceNameSchema(t).safeParse('').success).toBe(false)
    expect(instanceNameSchema(t).safeParse('a'.repeat(65)).success).toBe(false)
    expect(instanceNameSchema(t).safeParse('a'.repeat(64)).success).toBe(true)
  })
})

describe('proxyAllowedIpsSchema', () => {
  it('parses CIDR lines and drops blanks', () => {
    const result = proxyAllowedIpsSchema(t).safeParse('192.168.1.0/24\n\n10.0.0.10/32')
    expect(result.success).toBe(true)
    expect(result.data).toEqual(['192.168.1.0/24', '10.0.0.10/32'])
  })

  it('allows empty text (allow all)', () => {
    const result = proxyAllowedIpsSchema(t).safeParse('')
    expect(result.success).toBe(true)
    expect(result.data).toEqual([])
  })

  it('rejects garbage lines', () => {
    const result = proxyAllowedIpsSchema(t).safeParse('not-an-ip')
    expect(result.success).toBe(false)
  })

  it('rejects invalid prefixes', () => {
    expect(proxyAllowedIpsSchema(t).safeParse('192.168.1.0/33').success).toBe(false)
    expect(proxyAllowedIpsSchema(t).safeParse('999.1.1.1').success).toBe(false)
  })
})

describe('proxySchema limits', () => {
  it('accepts blank limits as unlimited (null)', () => {
    const result = proxySchema(t).safeParse({
      allowedIpsText: '',
      maxConnections: null,
      maxRps: null,
    })
    expect(result.success).toBe(true)
  })

  it('accepts zero as unlimited and positive integers', () => {
    expect(proxySchema(t).safeParse({ allowedIpsText: '', maxConnections: 0, maxRps: null }).success).toBe(true)
    expect(proxySchema(t).safeParse({ allowedIpsText: '', maxConnections: 50, maxRps: 100 }).success).toBe(true)
  })

  it('rejects negative and fractional limits', () => {
    expect(proxySchema(t).safeParse({ allowedIpsText: '', maxConnections: -1, maxRps: null }).success).toBe(false)
    expect(proxySchema(t).safeParse({ allowedIpsText: '', maxConnections: 1.5, maxRps: null }).success).toBe(false)
  })
})

describe('accountSchema', () => {
  const base = {
    mode: 'free',
    license: '',
    zeroTrustOrg: '',
    clientId: '',
    clientSecret: '',
  }

  it('accepts free mode without credentials', () => {
    expect(accountSchema(t).safeParse(base).success).toBe(true)
  })

  it('requires a license for warp_plus', () => {
    const result = accountSchema(t).safeParse({ ...base, mode: 'warp_plus', license: '' })
    expect(result.success).toBe(false)
    expect(JSON.stringify(result.error?.issues)).toContain('license key')
  })

  it('accepts warp_plus with a license', () => {
    expect(accountSchema(t).safeParse({ ...base, mode: 'warp_plus', license: 'ABC-123' }).success).toBe(true)
  })

  it('requires all three fields for zero_trust', () => {
    const missing = accountSchema(t).safeParse({ ...base, mode: 'zero_trust', zeroTrustOrg: 'team' })
    expect(missing.success).toBe(false)
    const complete = accountSchema(t).safeParse({
      ...base,
      mode: 'zero_trust',
      zeroTrustOrg: 'team',
      clientId: 'id',
      clientSecret: 'secret',
    })
    expect(complete.success).toBe(true)
  })
})