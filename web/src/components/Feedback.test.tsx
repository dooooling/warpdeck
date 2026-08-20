// 状态徽章组件测试（P9 §14.4：state badge；P9-014 状态不只靠颜色）。

import { describe, expect, it } from 'vitest'
import { render, screen } from '@testing-library/react'

import { DesiredBadge, StateBadge } from './Feedback'

describe('StateBadge', () => {
  it('renders healthy with both text and aria label', () => {
    render(<StateBadge state="healthy" />)
    const badge = screen.getByRole('status')
    expect(badge).toHaveTextContent('Healthy')
    expect(badge).toHaveAttribute('aria-label', 'State: Healthy')
  })

  it('renders a label for every runtime state (text not color only)', () => {
    const states = [
      'disabled',
      'stopped',
      'starting',
      'registering',
      'connecting',
      'healthy',
      'degraded',
      'stopping',
      'failed',
    ] as const
    for (const state of states) {
      const { unmount } = render(<StateBadge state={state} />)
      const badge = screen.getByRole('status')
      const label = badge.textContent ?? ''
      expect(label.length).toBeGreaterThan(0)
      expect(badge.className).toMatch(/^badge badge-/)
      unmount()
    }
  })

  it('renders desired running/stopped distinctly', () => {
    const { unmount } = render(<DesiredBadge desired="running" />)
    expect(screen.getByRole('status')).toHaveTextContent('Running')
    unmount()
    render(<DesiredBadge desired="stopped" />)
    expect(screen.getByRole('status')).toHaveTextContent('Stopped')
  })
})