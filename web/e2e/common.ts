// E2E 公共辅助：重置 mock + 登录。

import type { Page } from '@playwright/test'

export const MOCK_PORT = 8787
export const USERNAME = 'admin'
export const PASSWORD = 'correct-horse-123'

export async function resetMock(): Promise<void> {
  await fetch(`http://127.0.0.1:${MOCK_PORT}/__mock/reset`, { method: 'POST' })
}

/** 重置状态后创建管理员并登录，返回已登录页面（dashboard）。 */
export async function loginAsAdmin(page: Page): Promise<void> {
  await resetMock()
  await page.goto('/')
  await page.waitForURL('**/setup')
  await page.getByLabel('Username').fill(USERNAME)
  await page.getByLabel('Password', { exact: true }).fill(PASSWORD)
  await page.getByLabel('Confirm password').fill(PASSWORD)
  await page.getByRole('button', { name: 'Create admin account' }).click()
  await page.waitForURL('**/login')
  await loginWith(page, USERNAME, PASSWORD)
}

export async function loginWith(page: Page, username: string, password: string): Promise<void> {
  await page.getByLabel('Username').fill(username)
  await page.getByLabel('Password', { exact: true }).fill(password)
  await page.getByRole('button', { name: 'Log in' }).click()
  await page.waitForURL('**/dashboard')
}