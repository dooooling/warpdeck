//! mdm.xml（WARP MDM 策略文件）的渲染与同步（v0.2 ZeroTrust 服务令牌注册）。
//!
//! 背景（DESIGN §11.2 注入点 / Cloudflare 官方 MDM 部署）：
//! - Cloudflare One Client（Linux）在 warp-svc 的配置目录（本实现 = 实例
//!   `STATE_DIRECTORY`，即 `{data_dir}/instances/{id}/state`）发现 `mdm.xml`
//!   时，随启动自动按文件内 `organization` / `auth_client_id` /
//!   `auth_client_secret` 以 **service token** 注册——无需（也无法 headless
//!   执行）`teams-enroll` 交互式 OAuth。
//! - Teams（managed）账号**禁止本地改代理端口/模式**：`warp-cli mode proxy` /
//!   `warp-cli proxy port` 会报 `Invalid setting for this account type`。因此
//!   ZeroTrust 实例的 `service_mode=proxy` + `proxy_port=<40000+id>` 也必须经
//!   mdm.xml 下发（上游 docker-warp-proxy 同法）。
//! - ZeroTrust 档案必须在 warp-svc 启动**前**写入 mdm.xml；非 ZeroTrust 必须
//!   移除此前残留，否则换档后旧文件仍会驱动 ZT 注册（实例状态污染）。
//! - 上游服务令牌 MDM 格式（Cloudflare 官方参数文档）：
//!   ```xml
//!   <dict>
//!     <key>organization</key><string>TEAM</string>
//!     <key>auth_client_id</key><string>….access</string>
//!     <key>auth_client_secret</key><string>SECRET</string>
//!     <key>service_mode</key><string>proxy</string>
//!     <key>proxy_port</key><integer>4000X</integer>
//!   </dict>
//!   ```
//! - **`warp_tunnel_protocol=masque` 必须随 mdm.xml 下发**（E2E-08 实测）：org 设备
//!   档案（NetworkPolicy）默认 `warp_tunnel_protocol=Wireguard`，而 `service_mode=proxy`
//!   的 WarpProxy 模式只支持 MASQUE——缺此项时连接直接失败
//!   `InvalidKey("Proxy mode only supports MASQUE")`。LocalPolicy（mdm）优先级高于
//!   组织网络策略，因此在此显式强制 MASQUE 可稳定覆盖组织默认的 Wireguard。
//! - mdm.xml 是明文（含 client_secret），与 reg.json 同置于实例私有 state
//!   目录；不得进入日志/API（redactor 兜底，AGENTS.md）。

use std::path::Path;

use super::credentials::{CredentialMode, InstanceCredentials};

/// MDM 策略文件名（warp-svc 启动时读取）。
pub const MDM_FILE: &str = "mdm.xml";

/// mdm.xml 同步错误。
#[derive(Debug, thiserror::Error)]
pub enum MdmError {
    #[error("io error syncing mdm.xml: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot build mdm.xml: {0}")]
    Incomplete(&'static str),
}

/// 渲染 mdm.xml（ZeroTrust 专用）。除 service token 外还写入 `service_mode` 与
/// `proxy_port`（Teams managed 账号不能经 CLI 改这两项，只能由 mdm 下发），并强制
/// `warp_tunnel_protocol=masque`（WarpProxy 模式只支持 MASQUE；否则组织网络策略的
/// Wireguard 会导致 `InvalidKey("Proxy mode only supports MASQUE")`，E2E-08 实测）。
/// 非 ZeroTrust（或字段缺失）返回 `None`。
pub fn render_mdm_xml(credentials: &InstanceCredentials, proxy_port: u16) -> Option<String> {
    if credentials.mode != CredentialMode::ZeroTrust {
        return None;
    }
    let org = credentials.zero_trust_org.as_deref()?;
    let client_id = credentials.zt_client_id.as_deref()?;
    let client_secret = credentials.zt_client_secret.as_deref()?;
    Some(format!(
        concat!(
            "<dict>\n",
            "  <key>organization</key>\n",
            "  <string>{org}</string>\n",
            "  <key>auth_client_id</key>\n",
            "  <string>{client_id}</string>\n",
            "  <key>auth_client_secret</key>\n",
            "  <string>{client_secret}</string>\n",
            "  <key>service_mode</key>\n",
            "  <string>proxy</string>\n",
            "  <key>proxy_port</key>\n",
            "  <integer>{port}</integer>\n",
            "  <key>warp_tunnel_protocol</key>\n",
            "  <string>masque</string>\n",
            "</dict>\n"
        ),
        org = escape_xml(org),
        client_id = escape_xml(client_id),
        client_secret = escape_xml(client_secret),
        port = proxy_port,
    ))
}

/// XML 文本节点转义：防 org / id / secret 中的 `&<>"'` 破坏结构或注入。
pub fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// warp-svc 启动前的 mdm.xml 同步：
/// - ZeroTrust：目录就绪后写入（覆盖）mdm.xml（含 service_mode/proxy_port）；
///   字段缺失 = 显式失败（禁止伪装成功，AGENTS.md）。
/// - 其它模式：尽力删除可能残留的 mdm.xml（NotFound 视为成功）。
pub async fn sync_mdm_xml(
    state_dir: &Path,
    credentials: &InstanceCredentials,
    proxy_port: u16,
) -> Result<(), MdmError> {
    let path = state_dir.join(MDM_FILE);
    if credentials.mode == CredentialMode::ZeroTrust {
        let content = render_mdm_xml(credentials, proxy_port).ok_or(MdmError::Incomplete(
            "zero_trust profile missing organization/client id/client secret",
        ))?;
        tokio::fs::create_dir_all(state_dir).await?;
        tokio::fs::write(&path, content).await?;
        return Ok(());
    }
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zt() -> InstanceCredentials {
        InstanceCredentials {
            mode: CredentialMode::ZeroTrust,
            zero_trust_org: Some("acme-corp".into()),
            zt_client_id: Some("abcd1234.access".into()),
            zt_client_secret: Some("s3cr3t".into()),
            ..InstanceCredentials::free()
        }
    }

    #[test]
    fn renders_service_token_mdm_xml() {
        let xml = render_mdm_xml(&zt(), 40002).unwrap();
        assert!(xml.contains("<key>organization</key>"));
        assert!(xml.contains("<string>acme-corp</string>"));
        assert!(xml.contains("<string>abcd1234.access</string>"));
        assert!(xml.contains("<string>s3cr3t</string>"));
        assert!(xml.contains("<key>service_mode</key>"));
        assert!(xml.contains("<string>proxy</string>"));
        assert!(xml.contains("<key>proxy_port</key>"));
        assert!(xml.contains("<integer>40002</integer>"));
        assert!(xml.contains("<key>warp_tunnel_protocol</key>"));
        assert!(xml.contains("<string>masque</string>"));
    }

    #[test]
    fn renders_none_for_non_zero_trust() {
        assert_eq!(render_mdm_xml(&InstanceCredentials::free(), 40000), None);
        let warpplus = InstanceCredentials {
            mode: CredentialMode::WarpPlus,
            license: Some("WPL-X".into()),
            ..InstanceCredentials::free()
        };
        assert_eq!(render_mdm_xml(&warpplus, 40000), None);
    }

    #[test]
    fn renders_none_when_zero_trust_fields_missing() {
        let incomplete = InstanceCredentials {
            mode: CredentialMode::ZeroTrust,
            zero_trust_org: Some("acme-corp".into()),
            ..InstanceCredentials::free()
        };
        assert_eq!(render_mdm_xml(&incomplete, 40000), None);
    }

    #[test]
    fn escape_xml_handles_special_chars() {
        assert_eq!(
            escape_xml("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
        assert_eq!(escape_xml("acme&co"), "acme&amp;co");
    }

    #[tokio::test]
    async fn sync_writes_then_removes_on_non_zero_trust() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = dir.path().join("instances").join("0").join("state");

        sync_mdm_xml(&state, &zt(), 40002).await.unwrap();
        let content = std::fs::read_to_string(state.join(MDM_FILE)).unwrap();
        assert!(content.contains("s3cr3t"));
        assert!(content.contains("<integer>40002</integer>"));

        sync_mdm_xml(&state, &InstanceCredentials::free(), 40002)
            .await
            .unwrap();
        assert!(!state.join(MDM_FILE).exists(), "残留 mdm.xml 必须被清除");
    }

    #[tokio::test]
    async fn sync_rejects_incomplete_zero_trust() {
        let dir = tempfile::TempDir::new().unwrap();
        let incomplete = InstanceCredentials {
            mode: CredentialMode::ZeroTrust,
            zero_trust_org: Some("acme-corp".into()),
            ..InstanceCredentials::free()
        };
        let err = sync_mdm_xml(dir.path(), &incomplete, 40000)
            .await
            .unwrap_err();
        assert!(matches!(err, MdmError::Incomplete(_)));
    }
}
