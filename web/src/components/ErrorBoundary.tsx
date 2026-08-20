// 顶层错误边界（P9-001）：渲染崩溃时展示可恢复的兜底 UI，不白屏。

import { Component, type ErrorInfo, type ReactNode } from 'react'

import i18n from '../i18n'

interface ErrorBoundaryState {
  error: Error | null
}

export class ErrorBoundary extends Component<{ children: ReactNode }, ErrorBoundaryState> {
  state: ErrorBoundaryState = { error: null }

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error }
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    console.error('Uncaught UI error:', error, info.componentStack)
  }

  render(): ReactNode {
    if (this.state.error) {
      return (
        <div className="error-state error-state-full" role="alert">
          <p className="error-state-title">{i18n.t('errorBoundary.title')}</p>
          <p className="error-state-message">{this.state.error.message}</p>
          <button
            type="button"
            className="btn"
            onClick={() => {
              this.setState({ error: null })
              window.location.href = '/'
            }}
          >
            {i18n.t('errorBoundary.reload')}
          </button>
        </div>
      )
    }
    return this.props.children
  }
}