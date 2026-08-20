//! Master key 生成/加载（P8-007）。
//!
//! 优先级（DESIGN §15.2）：
//! 1. `WARPDECK_MASTER_KEY` 环境变量（base64 编码的 32 字节）。
//! 2. `<data_dir>/master.key`（0600）。
//! 3. 首次启动生成随机 key 并原子落盘（0600）。
//!
//! 权限要求：Unix 上强制 0600（生成与加载后均校验）；Windows 无 posix
//! 权限位，仅记录提示。key 本身只在内存停留，绝不进入日志。

use std::fs;
use std::io::Write;
use std::path::Path;

use base64::Engine;
use rand_core::{OsRng, RngCore};

use super::{CryptoError, KEY_LEN};

/// 固定文件名（位于 data_dir 下）。
pub const MASTER_KEY_FILE: &str = "master.key";

/// 从环境变量或文件加载/生成 master key。
///
/// - `env_key`: `WARPDECK_MASTER_KEY` 的值（None = 未设置）。
/// - `data_dir`: 持久化目录（key 文件落点）。
pub fn load_or_create(
    env_key: Option<&str>,
    data_dir: &Path,
) -> Result<[u8; KEY_LEN], CryptoError> {
    if let Some(raw) = env_key.filter(|v| !v.is_empty()) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(raw)
            .map_err(|_| {
                CryptoError::MasterKey("WARPDECK_MASTER_KEY is not valid base64".into())
            })?;
        let arr: [u8; KEY_LEN] = bytes.try_into().map_err(|_| {
            CryptoError::MasterKey("WARPDECK_MASTER_KEY must decode to 32 bytes".into())
        })?;
        return Ok(arr);
    }

    let path = data_dir.join(MASTER_KEY_FILE);
    if path.exists() {
        return read_key_file(&path);
    }

    // 生成并原子写入（临时文件 + rename），避免半写文件被当成 key。
    fs::create_dir_all(data_dir)
        .map_err(|e| CryptoError::MasterKey(format!("cannot create data dir: {e}")))?;
    let mut key = [0u8; KEY_LEN];
    OsRng.fill_bytes(&mut key);
    let encoded = base64::engine::general_purpose::STANDARD.encode(key);
    let tmp = data_dir.join(format!("{MASTER_KEY_FILE}.tmp"));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| CryptoError::MasterKey(format!("cannot create key file: {e}")))?;
    f.write_all(encoded.as_bytes())
        .and_then(|_| f.write_all(b"\n"))
        .map_err(|e| CryptoError::MasterKey(format!("cannot write key file: {e}")))?;
    drop(f);
    set_private(&tmp);
    fs::rename(&tmp, &path)
        .map_err(|e| CryptoError::MasterKey(format!("cannot finalize key file: {e}")))?;
    set_private(&path);
    Ok(key)
}

fn read_key_file(path: &Path) -> Result<[u8; KEY_LEN], CryptoError> {
    let raw = fs::read_to_string(path)
        .map_err(|e| CryptoError::MasterKey(format!("cannot read key file: {e}")))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .map_err(|_| CryptoError::MasterKey("master.key is not valid base64".into()))?;
    let arr: [u8; KEY_LEN] = bytes
        .try_into()
        .map_err(|_| CryptoError::MasterKey("master.key must contain 32 bytes".into()))?;
    set_private(path);
    Ok(arr)
}

/// Unix：强制 0600（文件属主可读写，其余无权限）。
#[cfg(unix)]
fn set_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).ok();
        }
    }
}

/// Windows：无 posix 权限位；要求目录 ACL 由部署方控制（文档注明）。
#[cfg(not(unix))]
fn set_private(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_data_dir() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        (dir, path)
    }

    #[test]
    fn env_key_wins_and_roundtrips() {
        let (dir, _path) = tmp_data_dir();
        let key = [7u8; KEY_LEN];
        let encoded = base64::engine::general_purpose::STANDARD.encode(key);
        let loaded = load_or_create(Some(&encoded), dir.path()).unwrap();
        assert_eq!(loaded, key);
        // env 存在时不写文件。
        assert!(!dir.path().join(MASTER_KEY_FILE).exists());
    }

    #[test]
    fn generates_and_reloads_key_file() {
        let (_dir, path) = tmp_data_dir();
        let first = load_or_create(None, &path).unwrap();
        let second = load_or_create(None, &path).unwrap();
        assert_eq!(first, second, "reload must read the persisted key");
        assert!(path.join(MASTER_KEY_FILE).exists());
    }

    #[test]
    fn two_datadirs_get_distinct_keys() {
        let (a, path_a) = tmp_data_dir();
        let (b, path_b) = tmp_data_dir();
        let _ = a;
        let _ = b;
        let ka = load_or_create(None, &path_a).unwrap();
        let kb = load_or_create(None, &path_b).unwrap();
        assert_ne!(ka, kb);
    }

    #[test]
    fn rejects_invalid_env_key() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_or_create(Some("not-base64!"), dir.path()).is_err());
        // 合法 base64 但不是 32 字节。
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        assert!(load_or_create(Some(&short), dir.path()).is_err());
    }

    #[test]
    fn rejects_corrupt_key_file() {
        let (_dir, path) = tmp_data_dir();
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join(MASTER_KEY_FILE), "garbage").unwrap();
        assert!(load_or_create(None, &path).is_err());
    }

    /// P8-007 gate「master key 权限正确」：Unix 上必须 0600
    /// （属主可读写，组/其他无任何权限）。
    #[cfg(unix)]
    #[test]
    fn key_file_permissions_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = tmp_data_dir();
        let _ = load_or_create(None, &path).unwrap();
        let key_path = path.join(MASTER_KEY_FILE);
        let meta = fs::metadata(&key_path).unwrap();
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "master.key must be 0600, got {:o}",
            meta.permissions().mode() & 0o777
        );
    }
}
