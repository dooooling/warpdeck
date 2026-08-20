// E2E：实例生命周期（创建/启动/停止/重启/删除）。

import { expect, test } from '@playwright/test'

import { loginAsAdmin } from './common'

async function addInstance(page: import('@playwright/test').Page, name: string) {
  await page.getByRole('button', { name: '+ Add Instance' }).click()
  await page.getByLabel('Name').fill(name)
  await page.getByRole('button', { name: 'Create', exact: true }).click()
}

test('create instance converges to healthy and appears in the list', async ({ page }) => {
  await loginAsAdmin(page)
  await page.getByRole('link', { name: 'Instances' }).click()
  await addInstance(page, 'warp-1')

  const row = page.locator('tbody tr').filter({ hasText: 'warp-1' })
  await expect(row).toHaveCount(1)
  // mock 600ms 后收敛为 healthy，页面 5s 轮询刷新。
  await expect(row.getByText('Healthy')).toBeVisible({ timeout: 10_000 })
})

test('stop and start change desired state', async ({ page }) => {
  await loginAsAdmin(page)
  await page.getByRole('link', { name: 'Instances' }).click()
  await addInstance(page, 'warp-2')

  const row = page.locator('tbody tr').filter({ hasText: 'warp-2' })
  await expect(row.getByText('Healthy')).toBeVisible({ timeout: 10_000 })

  await row.getByRole('button', { name: 'Stop', exact: true }).click()
  await expect(row.getByText('Stopped').first()).toBeVisible({ timeout: 10_000 })

  await row.getByRole('button', { name: 'Start', exact: true }).click()
  await expect(row.getByText('Healthy')).toBeVisible({ timeout: 10_000 })
})

test('restart cycles the instance', async ({ page }) => {
  await loginAsAdmin(page)
  await page.getByRole('link', { name: 'Instances' }).click()
  await addInstance(page, 'warp-3')

  const row = page.locator('tbody tr').filter({ hasText: 'warp-3' })
  await expect(row.getByText('Healthy')).toBeVisible({ timeout: 10_000 })

  await row.getByRole('button', { name: 'Restart', exact: true }).click()
  // restart 经 starting 回到 healthy。
  await expect
    .poll(async () => (await row.getByText('Healthy').count()) > 0, { timeout: 10_000 })
    .toBe(true)
})

test('instance detail shows lifecycle fields and actions', async ({ page }) => {
  await loginAsAdmin(page)
  await page.getByRole('link', { name: 'Instances' }).click()
  await addInstance(page, 'warp-4')

  const link = page.locator('a.table-link', { hasText: 'warp-4' })
  await expect(link).toBeVisible()
  await link.click()
  await expect(page).toHaveURL(/\/instances\/1$/)
  await expect(page.getByRole('heading', { name: 'warp-4' })).toBeVisible()
  await expect(page.getByText('Status')).toBeVisible()
  // 固定规则：内部端口 40000 + id。
  await expect(page.getByText('40001', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Restart', exact: true }).click()
})

test('delete requires confirmation and removes the row', async ({ page }) => {
  await loginAsAdmin(page)
  await page.getByRole('link', { name: 'Instances' }).click()
  await addInstance(page, 'warp-5')

  const row = page.locator('tbody tr').filter({ hasText: 'warp-5' })
  await expect(row).toHaveCount(1)

  await row.getByRole('button', { name: 'Delete' }).click()
  // 确认对话框出现；取消则不删除。
  const dialog = page.getByRole('alertdialog')
  await expect(dialog).toBeVisible()
  await dialog.getByRole('button', { name: 'Cancel' }).click()
  await expect(row).toHaveCount(1)

  await row.getByRole('button', { name: 'Delete' }).click()
  await dialog.getByRole('button', { name: 'Delete instance' }).click()
  await expect(row).toHaveCount(0, { timeout: 10_000 })
})