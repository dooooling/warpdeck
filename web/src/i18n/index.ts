// i18n 初始化：navigator.language 自动检测 + 持久化用户选择。
// 语言包很小（<10KB），直接打包进 bundle，不引入按需加载复杂度。

import i18n from 'i18next'
import { initReactI18next } from 'react-i18next'

import en from './locales/en'
import zh from './locales/zh'

export type AppLanguage = 'en' | 'zh'

const STORAGE_KEY = 'warpdeck.lang'

export function detectLanguage(): AppLanguage {
  const stored = typeof window !== 'undefined' ? window.localStorage.getItem(STORAGE_KEY) : null
  if (stored === 'en' || stored === 'zh') {
    return stored
  }
  const nav = typeof navigator !== 'undefined' ? navigator.language.toLowerCase() : 'en'
  return nav.startsWith('zh') ? 'zh' : 'en'
}

export function changeLanguage(lang: AppLanguage) {
  void i18n.changeLanguage(lang)
  if (typeof window !== 'undefined') {
    window.localStorage.setItem(STORAGE_KEY, lang)
  }
}

i18n.use(initReactI18next).init({
  resources: {
    en: { translation: en },
    zh: { translation: zh },
  },
  lng: detectLanguage(),
  fallbackLng: 'en',
  interpolation: {
    escapeValue: false, // React 已做 XSS 转义
  },
  returnNull: false,
})

export default i18n