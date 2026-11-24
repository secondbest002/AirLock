use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderSecrets {
    #[serde(default)]
    pub gemini_api_key: String,
    #[serde(default)]
    pub openai_api_key: String,
    #[serde(default)]
    pub anthropic_api_key: String,
    #[serde(default)]
    pub auth_token: String,
    #[serde(default)]
    pub cloudflare_tunnel_token: String,
    #[serde(default)]
    pub downstream_bearer_tokens: HashMap<String, String>,
    #[serde(default)]
    pub downstream_envs: HashMap<String, HashMap<String, String>>,
    #[serde(default)]
    pub oauth_client_secrets: HashMap<String, String>,
    #[serde(default)]
    pub plugin_secrets: HashMap<String, HashMap<String, String>>,
}

impl ProviderSecrets {
    pub fn has_key_for(&self, provider: &str) -> bool {
        match provider.trim() {
            "openai_compat" => !self.openai_api_key.trim().is_empty(),
            "anthropic" => !self.anthropic_api_key.trim().is_empty(),
            _ => !self.gemini_api_key.trim().is_empty(),
        }
    }
}

#[cfg(target_os = "windows")]
mod win_dpapi {
    use std::ffi::c_void;

    pub const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

    #[repr(C)]
    pub struct CryptIntegerBlob {
        pub cb_data: u32,
        pub pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        pub fn CryptProtectData(
            p_data_in: *const CryptIntegerBlob,
            sz_data_descr: *const u16,
            p_optional_entropy: *const CryptIntegerBlob,
            pv_reserved: *mut c_void,
            p_prompt_struct: *const c_void,
            dw_flags: u32,
            p_data_out: *mut CryptIntegerBlob,
        ) -> i32;

        pub fn CryptUnprotectData(
            p_data_in: *const CryptIntegerBlob,
            ppsz_data_descr: *mut *mut u16,
            p_optional_entropy: *const CryptIntegerBlob,
            pv_reserved: *mut c_void,
            p_prompt_struct: *const c_void,
            dw_flags: u32,
            p_data_out: *mut CryptIntegerBlob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn LocalFree(h_mem: *mut c_void) -> *mut c_void;
    }
}

#[derive(Debug, Clone)]
pub struct SecretsStore {
    pub path: Option<PathBuf>,
}

impl SecretsStore {
    pub fn new_none() -> Self {
        Self { path: None }
    }

    pub fn new_persistent(data_dir: PathBuf) -> Self {
        Self {
            path: Some(data_dir.join("secrets.dpapi")),
        }
    }

    #[cfg(target_os = "windows")]
    fn dpapi_encrypt(plain: &str) -> Result<String, String> {
        let mut in_bytes = plain.as_bytes().to_vec();
        let cb_data = u32::try_from(in_bytes.len())
            .map_err(|_| "Secrets payload exceeds maximum DPAPI buffer size.".to_string())?;
        let in_blob = win_dpapi::CryptIntegerBlob {
            cb_data,
            pb_data: in_bytes.as_mut_ptr(),
        };
        let mut out_blob = win_dpapi::CryptIntegerBlob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };

        let ok = unsafe {
            win_dpapi::CryptProtectData(
                &in_blob,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                win_dpapi::CRYPTPROTECT_UI_FORBIDDEN,
                &mut out_blob,
            )
        };
        if ok == 0 {
            return Err(format!(
                "DPAPI CryptProtectData failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let encrypted = if out_blob.cb_data == 0 || out_blob.pb_data.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(out_blob.pb_data, out_blob.cb_data as usize) }
                .to_vec()
        };
        if !out_blob.pb_data.is_null() {
            unsafe {
                win_dpapi::LocalFree(out_blob.pb_data as *mut std::ffi::c_void);
            }
        }
        Ok(base64::engine::general_purpose::STANDARD.encode(&encrypted))
    }

    #[cfg(target_os = "windows")]
    fn dpapi_decrypt(cipher: &str) -> Result<String, String> {
        let trimmed = cipher.trim();
        if trimmed.is_empty() {
            return Ok(String::new());
        }
        let mut decoded = base64::engine::general_purpose::STANDARD
            .decode(trimmed)
            .map_err(|e| format!("DPAPI base64 decode error: {e}"))?;
        if decoded.is_empty() {
            return Ok(String::new());
        }
        let cb_data = u32::try_from(decoded.len())
            .map_err(|_| "DPAPI cipher payload exceeds maximum buffer size.".to_string())?;
        let in_blob = win_dpapi::CryptIntegerBlob {
            cb_data,
            pb_data: decoded.as_mut_ptr(),
        };
        let mut out_blob = win_dpapi::CryptIntegerBlob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };

        let ok = unsafe {
            win_dpapi::CryptUnprotectData(
                &in_blob,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                win_dpapi::CRYPTPROTECT_UI_FORBIDDEN,
                &mut out_blob,
            )
        };
        if ok == 0 {
            return Err(format!(
                "DPAPI CryptUnprotectData failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let decrypted = if out_blob.cb_data == 0 || out_blob.pb_data.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(out_blob.pb_data, out_blob.cb_data as usize) }
                .to_vec()
        };
        if !out_blob.pb_data.is_null() {
            unsafe {
                win_dpapi::LocalFree(out_blob.pb_data as *mut std::ffi::c_void);
            }
        }
        String::from_utf8(decrypted).map_err(|e| format!("DPAPI UTF-8 decode error: {e}"))
    }

    pub fn save(&self, secrets: &ProviderSecrets) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Gagal membuat direktori secrets: {e}"))?;
        }
        let json_str = serde_json::to_string(secrets)
            .map_err(|e| format!("Gagal serialisasi secrets: {e}"))?;

        #[cfg(target_os = "windows")]
        let payload = Self::dpapi_encrypt(&json_str)?;
        #[cfg(not(target_os = "windows"))]
        let payload = json_str;

        let tmp = path.with_extension("dpapi.tmp");
        std::fs::write(&tmp, &payload)
            .map_err(|e| format!("Gagal menulis tmp secrets: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        if std::fs::rename(&tmp, path).is_err() {
            std::fs::write(path, &payload)
                .map_err(|e| format!("Gagal menyimpan secrets: {e}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(())
    }

    pub fn load(&self) -> ProviderSecrets {
        let Some(path) = &self.path else {
            return ProviderSecrets::default();
        };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return ProviderSecrets::default();
        };
        if raw.trim().is_empty() {
            return ProviderSecrets::default();
        }

        #[cfg(target_os = "windows")]
        let decoded = Self::dpapi_decrypt(&raw).unwrap_or_default();
        #[cfg(not(target_os = "windows"))]
        let decoded = raw;

        serde_json::from_str::<ProviderSecrets>(&decoded).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_secrets_store_roundtrip() {
        let temp_dir = std::env::temp_dir().join("smart_mcp_secrets_dpapi_test");
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let store = SecretsStore::new_persistent(temp_dir.clone());
        let mut downstream_bearer_tokens = HashMap::new();
        downstream_bearer_tokens.insert("ds-1".to_string(), "bearer-vault-token-xyz".to_string());

        let mut env_map = HashMap::new();
        env_map.insert("GITHUB_TOKEN".to_string(), "ghp_unicode_αβγ_secret".to_string());
        let mut downstream_envs = HashMap::new();
        downstream_envs.insert("ds-1".to_string(), env_map);

        let mut oauth_client_secrets = HashMap::new();
        oauth_client_secrets.insert("mcp_client_1".to_string(), "oauth-secret-999".to_string());

        let mut plg_secret_map = HashMap::new();
        plg_secret_map.insert("API_KEY".to_string(), "plg-secret-val-777".to_string());
        let mut plugin_secrets = HashMap::new();
        plugin_secrets.insert("plg_demo_123456".to_string(), plg_secret_map);

        let secrets = ProviderSecrets {
            gemini_api_key: "AIzaTestKey123".to_string(),
            openai_api_key: "sk-test-openai-456".to_string(),
            anthropic_api_key: "sk-ant-test-789".to_string(),
            auth_token: "master-auth-token-abc".to_string(),
            cloudflare_tunnel_token: "eyJ_tunnel_secret_unicode_🔐".to_string(),
            downstream_bearer_tokens,
            downstream_envs,
            oauth_client_secrets,
            plugin_secrets,
        };

        store.save(&secrets).unwrap();

        // Stored ciphertext must not contain plaintext secrets on Windows DPAPI
        let on_disk = fs::read_to_string(temp_dir.join("secrets.dpapi")).unwrap();
        #[cfg(target_os = "windows")]
        {
            assert!(!on_disk.contains("AIzaTestKey123"));
            assert!(!on_disk.contains("master-auth-token-abc"));
            assert!(!on_disk.contains("bearer-vault-token-xyz"));
            assert!(!on_disk.contains("oauth-secret-999"));
            assert!(!on_disk.contains("ghp_unicode"));
            assert!(!on_disk.contains("plg-secret-val-777"));
        }

        let loaded = store.load();
        assert_eq!(loaded, secrets);

        // Also ensure legacy 3-field JSON payload deserializes cleanly
        let legacy_json = r#"{"gemini_api_key":"g1","openai_api_key":"o1","anthropic_api_key":"a1"}"#;
        let legacy: ProviderSecrets = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(legacy.gemini_api_key, "g1");
        assert!(legacy.downstream_bearer_tokens.is_empty());
        assert!(legacy.plugin_secrets.is_empty());

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
