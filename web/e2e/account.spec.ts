// E2E：Account 秘密表单（DESIGN §19.6/§20.6：GET 不回填、缺字段被拒、清除需确认）。

import { expect, test } from '@playwright/test'

import { loginAsAdmin } from './common'

test('warp_plus requires a license and never echoes it back', async ({ page }) => {
  await loginAsAdmin(page)
  await page.getByRole('link', { name: 'Account' }).click()

  // 缺 license 直接被拒绝。
  await page.getByLabel(/WARP\+/).check()
  await page.getByRole('button', { name: 'Save account config' }).click()
  await expect(page.getByText('WARP+ requires a license key')).toBeVisible()

  await page.locator('#account-license').fill('LICENSE-SECRET-ABC-123')
  await page.getByRole('button', { name: 'Save account config' }).click()
  await expect(page.getByText('Account config saved')).toBeVisible()
  await expect(page.getByText(/configured — type to replace/)).toBeVisible()
  // 明文不回填。
  await expect(page.locator('#account-license')).toHaveValue('')
  await expect(page.getByText('LICENSE-SECRET-ABC-123')).toHaveCount(0)
})

test('zero_trust requires org, client id and secret', async ({ page }) => {
  await loginAsAdmin(page)
  await page.getByRole('link', { name: 'Account' }).click()

  await page.getByLabel(/Zero Trust/).check()
  await page.locator('#account-org').fill('team-default')
  await page.getByRole('button', { name: 'Save account config' }).click()
  await expect(page.getByText('Client ID is required')).toBeVisible()
  await expect(page.getByText('Client secret is required')).toBeVisible()

  await page.locator('#account-client-id').fill('zt-id-123')
  await page.locator('#account-client-secret').fill('zt-secret-456')
  await page.getByRole('button', { name: 'Save account config' }).click()
  await expect(page.getByText('Account config saved')).toBeVisible()
  await expect(page.getByText(/Credentials are configured/)).toHaveCount(0)
  await expect(page.getByText('zt-secret-456')).toHaveCount(0)
})

test('clears credentials with confirmation (dangerous action)', async ({ page }) => {
  await loginAsAdmin(page)
  await page.getByRole('link', { name: 'Account' }).click()

  // 先配置 warp_plus。
  await page.getByLabel(/WARP\+/).check()
  await page.locator('#account-license').fill('LICENSE-TO-CLEAR-1')
  await page.getByRole('button', { name: 'Save account config' }).click()
  await expect(page.getByText(/configured — type to replace/)).toBeVisible()

  await page.getByRole('button', { name: 'Clear license' }).click()
  const dialog = page.getByRole('alertdialog')
  await expect(dialog).toBeVisible()
  // 取消不生效。
  await dialog.getByRole('button', { name: 'Cancel' }).click()
  await expect(page.getByText(/configured — type to replace/)).toBeVisible()

  await page.getByRole('button', { name: 'Clear license' }).click()
  await dialog.getByRole('button', { name: 'Clear credentials' }).click()
  await expect(page.getByText('Credentials cleared')).toBeVisible()
  await expect(page.getByText(/configured — type to replace/)).toHaveCount(0)
})