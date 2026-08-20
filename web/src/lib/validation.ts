// 前端表单校验（DESIGN §22.3）：与后端校验规则保持一致，
// 但后端仍是最终权威（此处只为即时反馈）。
// 文案走 i18n：schema 变为工厂函数，调用处传入当前语言的 t()。

import { z } from 'zod'
import type { TFunction } from 'i18next'

export const usernameSchema = (t: TFunction) =>
  z
    .string()
    .trim()
    .min(1, t('validation.usernameRequired'))
    .max(64, t('validation.usernameMax'))

export const passwordSchema = (t: TFunction) =>
  z
    .string()
    .min(8, t('validation.passwordMin'))
    .max(1024, t('validation.passwordMax'))

export const loginSchema = (t: TFunction) =>
  z.object({
    username: usernameSchema(t),
    password: z.string().min(1, t('validation.passwordRequired')),
  })

export const setupSchema = (t: TFunction) =>
  z
    .object({
      username: usernameSchema(t),
      password: passwordSchema(t),
      confirmPassword: z.string(),
    })
    .refine((v) => v.password === v.confirmPassword, {
      message: t('validation.passwordsMismatch'),
      path: ['confirmPassword'],
    })

export const instanceNameSchema = (t: TFunction) =>
  z
    .string()
    .trim()
    .min(1, t('validation.nameRequired'))
    .max(64, t('validation.nameMax'))

/** CIDR / IP 或空行；`-` 行代表允许所有（与后端 allowed_ips 空数组等价）。 */
const ipEntrySchema = (t: TFunction) =>
  z
    .string()
    .trim()
    .refine((v) => v.length === 0 || v === '-' || isCidr(v), t('validation.invalidIp'))

function isCidr(value: string): boolean {
  const [addr, prefix] = value.split('/')
  if (!addr) {
    return false
  }
  const octets = addr.split('.').map(Number)
  const validAddr =
    octets.length === 4 &&
    octets.every((o) => Number.isInteger(o) && o >= 0 && o <= 255)
  if (!validAddr) {
    return false
  }
  if (prefix === undefined) {
    return true
  }
  const n = Number(prefix)
  return Number.isInteger(n) && n >= 0 && n <= 32
}

export const proxyAllowedIpsSchema = (t: TFunction) =>
  z
    .string()
    .transform((text) =>
      text
        .split(/\r?\n/)
        .map((line) => line.trim())
        .filter((line) => line.length > 0 && line !== '-'),
    )
    .pipe(z.array(ipEntrySchema(t)))

export const proxySchema = (t: TFunction) =>
  z.object({
    allowedIpsText: proxyAllowedIpsSchema(t),
    maxConnections: z.union([z.coerce.number().int().min(1), z.literal(0), z.literal(null)]),
    maxRps: z.union([z.coerce.number().int().min(1), z.literal(0), z.literal(null)]),
  })

export const accountSchema = (t: TFunction) =>
  z
    .object({
      mode: z.enum(['free', 'warp_plus', 'zero_trust']),
      license: z.string().trim(),
      zeroTrustOrg: z.string().trim(),
      clientId: z.string().trim(),
      clientSecret: z.string(),
    })
    .superRefine((v, ctx) => {
      if (v.mode === 'warp_plus' && v.license.length === 0) {
        ctx.addIssue({
          code: 'custom',
          path: ['license'],
          message: t('validation.licenseRequired'),
        })
      }
      if (v.mode === 'zero_trust') {
        for (const [field, key] of [
          ['zeroTrustOrg', 'validation.orgRequired'],
          ['clientId', 'validation.clientIdRequired'],
          ['clientSecret', 'validation.clientSecretRequired'],
        ] as const) {
          if (v[field].length === 0) {
            ctx.addIssue({ code: 'custom', path: [field], message: t(key) })
          }
        }
      }
    })

export type SetupFormValues = z.infer<ReturnType<typeof setupSchema>>
export type ProxyFormValues = z.infer<ReturnType<typeof proxySchema>>
export type AccountFormValues = z.infer<ReturnType<typeof accountSchema>>