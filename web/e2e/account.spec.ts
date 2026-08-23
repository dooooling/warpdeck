// E2E：账号档案（v0.2 §17.6 profiles 流程；masked 秘密边界）。
// P1 审查 R3#5：旧版单账号 UI 已被档案化重写，本文件按当前 UI 重写。

import { expect, test } from '@playwright/test'

import { loginAsAdmin } from './common'

async function openAccounts(page: import('@playwright/test').Page) {
  await page.getByRole('link', { name: 'Account' }).click()
}

async function openNewProfile(page: import('@playwright/test').Page) {
  // 头部按钮渲染为 `+ New Profile`。
  await page.getByRole('button', { name: '+ New Profile' }).click()
  await expect(page.getByRole('heading', { name: 'New Account Profile' })).toBeVisible()
}

test('warp_plus requires a license and never echoes it back', async ({ page }) => {
  await loginAsAdmin(page)
  await openAccounts(page)
  await openNewProfile(page)

  await page.locator('#profile-name').fill('plus-profile')
  await page.getByLabel(/WARP\+/).check()
  // 缺 license 直接提交 → 字段错误。
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(page.getByText('WARP+ requires a license key')).toBeVisible()

  await page.locator('#profile-license').fill('LICENSE-SECRET-ABC-123')
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(page.getByText('Profile created')).toBeVisible()

  // 列表：凭据列显示 Configured mask，且整页不出现明文。
  const row = page.locator('tbody tr').filter({ hasText: 'plus-profile' })
  await expect(row).toHaveCount(1)
  await expect(row).not.toContainText('LICENSE-SECRET-ABC-123')

  // 重新打开编辑：明文不回填。
  await row.getByRole('button', { name: 'Edit' }).click()
  await expect(page.locator('#profile-license')).toHaveValue('')
})

test('zero_trust requires org, client id and secret', async ({ page }) => {
  await loginAsAdmin(page)
  await openAccounts(page)
  await openNewProfile(page)

  await page.locator('#profile-name').fill('zt-profile')
  await page.getByLabel(/Zero Trust/).check()
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(page.getByText('Organization is required')).toBeVisible()
  await expect(page.getByText('Client ID is required')).toBeVisible()
  await expect(page.getByText('Client secret is required')).toBeVisible()

  await page.locator('#profile-org').fill('team-default')
  await page.locator('#profile-client-id').fill('zt-id-123')
  await page.locator('#profile-client-secret').fill('zt-secret-456')
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(page.getByText('Profile created')).toBeVisible()

  const row = page.locator('tbody tr').filter({ hasText: 'zt-profile' })
  await expect(row).toContainText('Configured · team-default')
  await expect(page.locator('body')).not.toContainText('zt-secret-456')

  // 编辑留空 = 保持现有凭据（不要求重填）。
  await row.getByRole('button', { name: 'Edit' }).click()
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(page.getByText('Profile updated')).toBeVisible()
})

test('default free profile is protected (read-only row)', async ({ page }) => {
  await loginAsAdmin(page)
  await openAccounts(page)

  const def = page.locator('tbody tr').filter({ hasText: 'default' })
  await expect(def).toHaveCount(1)
  await expect(def).toContainText('default')
  await expect(def.getByRole('button', { name: 'Edit' })).toBeDisabled()
  await expect(def.getByRole('button', { name: 'Delete' })).toBeDisabled()
})

test('delete flow confirms before removing a profile', async ({ page }) => {
  await loginAsAdmin(page)
  await openAccounts(page)
  await openNewProfile(page)

  await page.locator('#profile-name').fill('throwaway')
  await page.getByLabel(/Cloudflare Zero Trust/).check()
  await page.locator('#profile-org').fill('org-x')
  await page.locator('#profile-client-id').fill('id-x')
  await page.locator('#profile-client-secret').fill('sec-x')
  await page.getByRole('button', { name: 'Save', exact: true }).click()
  await expect(page.getByText('Profile created')).toBeVisible()

  const row = page.locator('tbody tr').filter({ hasText: 'throwaway' })
  await row.getByRole('button', { name: 'Delete' }).click()

  const dialog = page.getByRole('alertdialog')
  await expect(dialog).toBeVisible()
  await dialog.getByRole('button', { name: 'Cancel' }).click()
  await expect(row).toHaveCount(1)

  await row.getByRole('button', { name: 'Delete' }).click()
  await dialog.getByRole('button', { name: 'Delete profile' }).click()
  await expect(page.getByText('Profile "throwaway" deleted')).toBeVisible()
  await expect(row).toHaveCount(0)
})
