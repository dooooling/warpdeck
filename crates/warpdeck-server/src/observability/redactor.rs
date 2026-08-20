//! 中央 secret 脱敏（P8-010）。
//!
//! 设计（DESIGN §27.2）：统一 `Sensitive<T>` 包装——任何进入日志/错误/审计
//! detail 的 secret（proxy password、WARP+ license、Zero Trust client secret、
//! session id、CSRF token）必须经 `Sensitive` 包装；其 `Debug`/`Display`
//! 恒输出 `[REDACTED]`，`Deref` 在受控代码内取原始值。
//!
//! 禁止绕过的反例：直接把 secret 拼进 `format!`/tracing 字段字符串。

use std::fmt;
use std::ops::Deref;

/// 脱敏占位符（日志/错误/审计中的统一输出）。
pub const REDACTED: &str = "[REDACTED]";

/// 敏感值包装：`Debug`/`Display` 恒输出 `[REDACTED]`。
///
/// 例：`tracing::info!(license = %Sensitive(&license))` → 日志输出 `license=[REDACTED]`。
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Sensitive<T>(pub T);

impl<T> Sensitive<T> {
    pub fn new(value: T) -> Self {
        Sensitive(value)
    }

    /// 受控获取原始值（仅限必须使用明文的内部代码路径）。
    pub fn reveal(&self) -> &T {
        &self.0
    }
}

impl<T> Deref for Sensitive<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> fmt::Debug for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl<T> fmt::Display for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// 字符串脱敏工具：整串替换为占位符（用于已格式化的字符串）。
pub fn redact(value: impl AsRef<str>) -> String {
    if value.as_ref().is_empty() {
        String::new()
    } else {
        REDACTED.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_are_redacted() {
        let s = Sensitive("TEST_SECRET_DO_NOT_LEAK_123");
        assert_eq!(format!("{s:?}"), REDACTED);
        assert_eq!(format!("{s}"), REDACTED);
        assert_eq!(format!("{:?}", Sensitive(String::from("x"))), REDACTED);
    }

    #[test]
    fn deref_and_reveal_access_the_value() {
        let s = Sensitive(String::from("plain"));
        assert_eq!(s.len(), 5);
        assert_eq!(s.reveal(), "plain");
        assert_eq!(*s, "plain");
    }

    #[test]
    fn redact_str_replaces_non_empty() {
        assert_eq!(redact("secret"), REDACTED);
        assert_eq!(redact(""), "");
    }
}
