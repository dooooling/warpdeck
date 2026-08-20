// E2E：Proxy 配置（DESIGN §19.5：端口只读、auth 密码不回填、清除需确认）。

import { expect, test } from '@playwright/test'

import { loginAsAdmin } from './common'

test('shows fixed in-container ports with compose hint', async ({ page }) => {
  await loginAsAdmin(page)
  await page.getByRole('link', { name: 'Proxy' }).click()
  await expect(page.getByText(':11080')).toBeVisible()
  await expect(page.getByText(':18080')).toBeVisible()
  await expect(page.getByText(/managed by Docker Compose/)).toBeVisible()
})

test('enables auth and stores a password without echoing it back', async ({ page }) => {
  await loginAsAdmin(page)
  await page.getByRole('link', { name: 'Proxy' }).click()

  await page.getByLabel('Require authentication').check()
  await page.getByLabel('Username').fill('proxyuser')
  await page.locator('#proxy-password').fill('top-secret-123')
  await page.getByRole('button', { name: 'Save config' }).click()

  await expect(page.getByText('Proxy config saved')).toBeVisible()
  // 已配置状态显示，明文永不回填。
  await expect(page.getByText(/a password is already configured/)).toBeVisible()
  await expect(page.locator('#proxy-password')).toHaveValue('')
  await expect(page.getByText('top-secret-123')).toHaveCount(0)
})

test('rejects invalid allowlist lines with a field error', async ({ page }) => {
  await loginAsAdmin(page)
  await page.getByRole('link', { name: 'Proxy' }).click()
  await page.getByLabel('Allowed networks').fill('not-an-ip')
  await page.getByRole('button', { name: 'Save config' }).click()
  await expect(page.getByText('Invalid IP or CIDR')).toBeVisible()
})

test('clears the password after confirmation', async ({ page }) => {
  await loginAsAdmin(page)
  await page.getByRole('link', { name: 'Proxy' }).click()

  await page.getByLabel('Require authentication').check()
  await page.getByLabel('Username').fill('proxyuser')
  await page.locator('#proxy-password').fill('top-secret-123')
  await page.getByRole('button', { name: 'Save config' }).click()
  await expect(page.getByText(/a password is already configured/)).toBeVisible()

  await page.getByRole('button', { name: 'Clear password' }).click()
  const dialog = page.getByRole('alertdialog')
  await expect(dialog).toBeVisible()
  await dialog.getByRole('button', { name: 'Clear password' }).click()

  await expect(page.getByText('Proxy password cleared')).toBeVisible()
  await expect(page.getByText(/a password is already configured/)).toHaveCount(0)
})