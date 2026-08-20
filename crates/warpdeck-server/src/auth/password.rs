//! Argon2id 密码哈希（P8-002）。
//!
//! 生产使用 OWASP 推荐参数（m=19456 KiB, t=2, p=1）；测试注入低参数
//! 变体避免 CI 时间膨胀。哈希格式为 PHC 字符串（自带 salt + 参数），
//! 验证时从字符串解析参数，天然支持未来参数升级。

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::{Algorithm, Argon2, Version};

/// 生产参数：19456 KiB 内存 / 2 轮 / 1 并行度（OWASP Argon2id 推荐）。
const PROD_M_KIB: u32 = 19_456;
const PROD_T: u32 = 2;
const PROD_P: u32 = 1;

/// 测试参数：小内存/少轮数（不可用于生产配置）。
const TEST_M_KIB: u32 = 256;
const TEST_T: u32 = 1;
const TEST_P: u32 = 1;

fn params(m_kib: u32, t: u32, p: u32) -> argon2::Params {
    argon2::Params::new(m_kib, t, p, Some(32)).expect("argon2 params are valid")
}

/// 生成 Argon2id 哈希（生产参数）。
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        params(PROD_M_KIB, PROD_T, PROD_P),
    )
    .hash_password(password.as_bytes(), &salt)
    .map(|h| h.to_string())
}

/// 校验密码（从 PHC 字符串读取参数）。
pub fn verify_password(hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// 测试用快速哈希（仅测试代码调用，显式命名防止误用）。
#[doc(hidden)]
pub fn hash_password_fast(password: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        params(TEST_M_KIB, TEST_T, TEST_P),
    )
    .hash_password(password.as_bytes(), &salt)
    .map(|h| h.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_roundtrip() {
        let hash = hash_password_fast("correct horse battery staple").unwrap();
        assert!(verify_password(&hash, "correct horse battery staple"));
        assert!(!verify_password(&hash, "wrong password"));
    }

    #[test]
    fn hash_is_argon2id_phc_and_salted() {
        let a = hash_password_fast("same-password").unwrap();
        let b = hash_password_fast("same-password").unwrap();
        assert_ne!(a, b, "random salt must differ");
        assert!(
            a.starts_with("$argon2id$"),
            "algorithm must be argon2id: {a}"
        );
        // 明文与任何哈希形式都不得出现在哈希串中。
        assert!(!a.contains("same-password"));
    }

    #[test]
    fn verify_rejects_garbage_hash() {
        assert!(!verify_password("not-a-hash", "anything"));
    }

    #[test]
    fn production_params_are_the_strong_default() {
        let hash = hash_password("x").unwrap();
        // PHC 字符串内嵌参数段（`$argon2id$v=19$m=...,t=...,p=...$...`）。
        let param_part = hash.split('$').nth(3).unwrap();
        assert_eq!(param_part, format!("m={PROD_M_KIB},t={PROD_T},p={PROD_P}"));
    }
}
