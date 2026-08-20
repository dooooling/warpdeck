// E2E：setup -> login 全流程（DESIGN §20.1：/setup 永久关闭；§20.3 session）。

import { expect, test } from '@playwright/test'

import { PASSWORD, USERNAME, loginWith, resetMock } from './common'

test.describe('setup and login', () => {
  test('uninitialized app redirects to /setup', async ({ page }) => {
    await resetMock()
    await page.goto('/')
    await page.waitForURL('**/setup')
    await expect(page.getByRole('heading', { name: 'WarpDeck Setup' })).toBeVisible()
  })

  test('creates admin, then locks /setup and logs in', async ({ page }) => {
    await resetMock()
    await page.goto('/')
    await page.waitForURL('**/setup')

    await page.getByLabel('Username').fill(USERNAME)
    await page.getByLabel('Password', { exact: true }).fill(PASSWORD)
    await page.getByLabel('Confirm password').fill(PASSWORD)
    await page.getByRole('button', { name: 'Create admin account' }).click()

    await page.waitForURL('**/login')
    // /setup 已锁定：直接访问回到登录页。
    await page.goto('/setup')
    await expect(page).toHaveURL(/\/login$/)

    await loginWith(page, USERNAME, PASSWORD)
    await expect(page.getByRole('heading', { name: 'Dashboard' })).toBeVisible()
  })

  test('rejects mismatched confirm password', async ({ page }) => {
    await resetMock()
    await page.goto('/setup')
    await page.getByLabel('Username').fill(USERNAME)
    await page.getByLabel('Password', { exact: true }).fill(PASSWORD)
    await page.getByLabel('Confirm password').fill('different-456')
    await page.getByRole('button', { name: 'Create admin account' }).click()
    await expect(page.getByText('Passwords do not match')).toBeVisible()
  })

  test('rejects wrong password on login', async ({ page }) => {
    await resetMock()
    await page.goto('/setup')
    await page.getByLabel('Username').fill(USERNAME)
    await page.getByLabel('Password', { exact: true }).fill(PASSWORD)
    await page.getByLabel('Confirm password').fill(PASSWORD)
    await page.getByRole('button', { name: 'Create admin account' }).click()
    await page.waitForURL('**/login')

    await page.getByLabel('Username').fill(USERNAME)
    await page.getByLabel('Password', { exact: true }).fill('wrong-password-123')
    await page.getByRole('button', { name: 'Log in' }).click()
    await expect(page.getByText('Invalid username or password.')).toBeVisible()
    await expect(page).toHaveURL(/\/login$/)
  })

  test('logout clears the session back to /login', async ({ page }) => {
    await resetMock()
    await page.goto('/setup')
    await page.getByLabel('Username').fill(USERNAME)
    await page.getByLabel('Password', { exact: true }).fill(PASSWORD)
    await page.getByLabel('Confirm password').fill(PASSWORD)
    await page.getByRole('button', { name: 'Create admin account' }).click()
    await page.waitForURL('**/login')
    await loginWith(page, USERNAME, PASSWORD)

    await page.getByRole('button', { name: 'Log out' }).first().click()
    await page.getByRole('button', { name: 'Log out' }).last().click()
    await page.waitForURL('**/login')
  })

  test('unauthenticated navigation to protected pages redirects to /login', async ({ page }) => {
    await resetMock()
    await page.goto('/')
    await page.waitForURL('**/setup')
    await page.getByLabel('Username').fill(USERNAME)
    await page.getByLabel('Password', { exact: true }).fill(PASSWORD)
    await page.getByLabel('Confirm password').fill(PASSWORD)
    await page.getByRole('button', { name: 'Create admin account' }).click()
    await page.waitForURL('**/login')

    await page.goto('/instances')
    await expect(page).toHaveURL(/\/login$/)
  })
})