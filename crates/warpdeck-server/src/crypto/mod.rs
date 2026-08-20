//! 加密基础设施（P8-007/008）。
//!
//! 设计（DESIGN §15.2/15.3，AGENTS.md 秘密基线）：
//! - Master key：`WARPDECK_MASTER_KEY`（base64）优先，否则 `<data_dir>/master.key`
//!   （0600）；两者都缺时生成随机 key 落盘。key 永不出现在日志/API。
//! - Secret 加密：XChaCha20-Poly1305（AEAD），每次写入新 nonce（24B 随机）；
//!   DB 只存 ciphertext + nonce + key_version。
//! - 解密失败（key 丢失/损坏）返回 typed error：调用方标记凭据不可用，
//!   不 panic、不崩溃循环（DESIGN §28.4）。

pub mod master_key;
pub mod secret_store;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use thiserror::Error;

/// Master key 长度（XChaCha20 需要 32 字节）。
pub const KEY_LEN: usize = 32;
/// XChaCha20-Poly1305 nonce 长度（24 字节）。
pub const NONCE_LEN: usize = 24;

/// 加密错误（不携带 secret 内容）。
#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("master key unavailable: {0}")]
    MasterKey(String),
    #[error("encryption failed")]
    Encrypt,
    #[error("decryption failed: {0}")]
    Decrypt(String),
}

/// 用 32 字节 key 加密 `plaintext`，返回 `(ciphertext, nonce)`。
pub fn encrypt(
    key: &[u8; KEY_LEN],
    plaintext: &[u8],
) -> Result<(Vec<u8>, [u8; NONCE_LEN]), CryptoError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; NONCE_LEN];
    use rand_core::{OsRng, RngCore};
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .map_err(|_| CryptoError::Encrypt)?;
    Ok((ciphertext, nonce))
}

/// 解密。`nonce` 与加密时相同（随密文存储）。
pub fn decrypt(
    key: &[u8; KEY_LEN],
    ciphertext: &[u8],
    nonce: &[u8; NONCE_LEN],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|_| CryptoError::Decrypt("ciphertext corrupted or wrong key".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; KEY_LEN] {
        let mut k = [0u8; KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    #[test]
    fn roundtrip_encrypt_decrypt() {
        let (ct, nonce) = encrypt(&test_key(), b"warp+ license 123").unwrap();
        let plain = decrypt(&test_key(), &ct, &nonce).unwrap();
        assert_eq!(plain, b"warp+ license 123");
        // ciphertext 不包含明文。
        assert!(!ct.windows(b"license".len()).any(|w| w == b"license"));
    }

    #[test]
    fn ciphertext_differs_each_encryption() {
        let (a, _) = encrypt(&test_key(), b"same secret").unwrap();
        let (b, _) = encrypt(&test_key(), b"same secret").unwrap();
        assert_ne!(a, b, "random nonce must produce distinct ciphertexts");
    }

    #[test]
    fn wrong_key_fails() {
        let (ct, nonce) = encrypt(&test_key(), b"data").unwrap();
        let wrong = [0u8; KEY_LEN];
        assert!(decrypt(&wrong, &ct, &nonce).is_err());
    }

    #[test]
    fn corrupted_ciphertext_fails() {
        let (mut ct, nonce) = encrypt(&test_key(), b"data").unwrap();
        let last = ct.len() - 1;
        ct[last] ^= 0xFF;
        assert!(decrypt(&test_key(), &ct, &nonce).is_err());
    }

    #[test]
    fn nonce_and_key_lengths_match_algorithm() {
        assert_eq!(KEY_LEN, 32);
        assert_eq!(NONCE_LEN, 24);
    }
}
