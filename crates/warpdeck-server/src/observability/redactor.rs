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

// ---------- 模式化行级脱敏（P1 审查 R2 补充项） ----------
//
// 旧实现：进程来源（warp-svc/gost）的每一行都整行替换为 [REDACTED]——安全但
// 零诊断价值（日志页全是占位符）。新实现：按模式只抹掉疑似 secret 的**片段**，
// 其余内容原样保留，日志页恢复可用性。
//
// 覆盖面（按序应用，先结构化后泛化）：
//   B. JSON 字段："secret"/"password"/"token"/"license" 等键的字符串值；
//   A. key=value / key: value 形态的敏感键（含 gost 配置回显、CLI --flag=）。
//
// 刻意**不做**孤立长 token 规则：无标签的高熵串（压缩行、哈希、minified
// 输出）误伤面过大（实测把 300 字节的普通输出整段吞掉）。残余风险（记录于
// DESIGN §27.2）：未知格式的 secret 可能漏网；缓解 = API 面（last_error 等）
// 已改为稳定安全摘要（P0 #9），而日志文件本身仅宿主 root 可读。

use std::sync::OnceLock;

enum Rule {
    Json,
    KeyValue,
}

fn rule(rule: Rule) -> &'static regex::Regex {
    static JSON: OnceLock<regex::Regex> = OnceLock::new();
    static KV: OnceLock<regex::Regex> = OnceLock::new();
    match rule {
        Rule::Json => JSON.get_or_init(|| {
            // "password": "..." —— 值整体替换。
            regex::Regex::new(
                r#"(?i)("(?:secret|client[_-]?secret|auth[_-]?client[_-]?secret|password|passwd|token|license|api[_-]?key|private[_-]?key)"\s*:\s*)"[^"]*""#,
            )
            .expect("static regex")
        }),
        Rule::KeyValue => KV.get_or_init(|| {
            // password=hunter2 / token: abc / --license-key XYZ
            regex::Regex::new(
                r#"(?i)\b(secret|client[_-]?secret|auth[_-]?client[_-]?secret|password|passwd|token|license(?:[_-]key)?|api[_-]?key|private[_-]?key)\b(\s*[=:]\s*|\s+)([^\s",;]{1,4096})"#,
            )
            .expect("static regex")
        }),
    }
}

/// 行级模式脱敏：只替换疑似 secret 的片段，保留其余诊断信息。
///
/// 入口约束（DESIGN §27.2）：进程来源（instance/gost）的历史读取与实时发布
/// 都必须经本函数（`runtime::logs::redact_line` 是唯一转发点）；manager 自身
/// 的结构化日志继续用 `Sensitive<T>` 在日志点保证。
pub fn scrub_line(line: &str) -> String {
    if line.is_empty() {
        return String::new();
    }
    let s = rule(Rule::Json).replace_all(line, r#"$1"[REDACTED]""#);
    let s = rule(Rule::KeyValue).replace_all(&s, "${1}${2}[REDACTED]");
    s.into_owned()
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

    // ---------- scrub_line：模式化片段脱敏（P1 审查 R2） ----------

    #[test]
    fn plain_lines_pass_through() {
        assert_eq!(scrub_line("warp: connected"), "warp: connected");
        assert_eq!(
            scrub_line("gost listening on :11080"),
            "gost listening on :11080"
        );
        assert_eq!(
            scrub_line("latency=42ms exit_ip=1.2.3.4"),
            "latency=42ms exit_ip=1.2.3.4"
        );
        assert_eq!(scrub_line(""), "");
    }

    #[test]
    fn key_value_secrets_are_masked() {
        // gost 配置回显 / CLI --flag 形态。
        assert_eq!(
            scrub_line("auth: password=hunter2"),
            "auth: password=[REDACTED]"
        );
        assert_eq!(
            scrub_line("--license-key ABCD-1234 applied"),
            "--license-key [REDACTED] applied"
        );
        assert_eq!(
            scrub_line("client_secret = topsecret; done"),
            "client_secret = [REDACTED]; done"
        );
    }

    #[test]
    fn json_string_values_are_masked() {
        assert_eq!(
            scrub_line(r#"{"auth_client_secret": "s3cr3t", "org": "acme"}"#),
            r#"{"auth_client_secret": "[REDACTED]", "org": "acme"}"#
        );
    }

    /// 无标签长 token 刻意**不**脱敏（误伤面过大：压缩行/哈希/普通输出）——
    /// 回归探针：普通长字母串原样保留。
    #[test]
    fn long_unlabeled_tokens_pass_through() {
        let sha = "a".repeat(64);
        assert_eq!(
            scrub_line(&format!("digest {sha} ok")),
            format!("digest {sha} ok")
        );
        // 常见运维值不受影响。
        assert_eq!(scrub_line("port 40000"), "port 40000");
        assert_eq!(scrub_line("ip 192.168.1.100"), "ip 192.168.1.100");
    }
}
