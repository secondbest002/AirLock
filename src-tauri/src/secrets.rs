use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderSecrets {
    #[serde(default)]
    pub gemini_api_key: String,
    #[serde(default)]
    pub openai_api_key: String,
    #[serde(default)]
    pub anthropic_api_key: String,
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
        use std::os::windows::process::CommandExt;
        let mut child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "Add-Type -AssemblyName System.Security; [Console]::InputEncoding=[System.Text.Encoding]::UTF8; $s=[Console]::In.ReadToEnd(); $b=[System.Text.Encoding]::UTF8.GetBytes($s); $e=[System.Security.Cryptography.ProtectedData]::Protect($b,$null,[System.Security.Cryptography.DataProtectionScope]::CurrentUser); [Convert]::ToBase64String($e)",
            ])
            .creation_flags(0x08000000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Gagal menjalankan PowerShell DPAPI encrypt: {e}"))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(plain.as_bytes())
                .map_err(|e| format!("Gagal menulis secret ke stdin: {e}"))?;
        }

        let out = child
            .wait_with_output()
            .map_err(|e| format!("Gagal menunggu DPAPI encrypt: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("DPAPI encrypt error: {}", err.trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    #[cfg(target_os = "windows")]
    fn dpapi_decrypt(cipher: &str) -> Result<String, String> {
        use std::os::windows::process::CommandExt;
        if cipher.trim().is_empty() {
            return Ok(String::new());
        }
        let mut child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "Add-Type -AssemblyName System.Security; [Console]::InputEncoding=[System.Text.Encoding]::UTF8; [Console]::OutputEncoding=[System.Text.Encoding]::UTF8; $c=[Console]::In.ReadToEnd().Trim(); if(-not $c){exit 0}; $e=[Convert]::FromBase64String($c); $b=[System.Security.Cryptography.ProtectedData]::Unprotect($e,$null,[System.Security.Cryptography.DataProtectionScope]::CurrentUser); [System.Text.Encoding]::UTF8.GetString($b)",
            ])
            .creation_flags(0x08000000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Gagal menjalankan PowerShell DPAPI decrypt: {e}"))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(cipher.as_bytes())
                .map_err(|e| format!("Gagal menulis cipher ke stdin: {e}"))?;
        }

        let out = child
            .wait_with_output()
            .map_err(|e| format!("Gagal menunggu DPAPI decrypt: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("DPAPI decrypt error: {}", err.trim()));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .trim_end_matches(&['\r', '\n'][..])
            .to_string())
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
        if std::fs::rename(&tmp, path).is_err() {
            std::fs::write(path, &payload)
                .map_err(|e| format!("Gagal menyimpan secrets: {e}"))?;
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
        let secrets = ProviderSecrets {
            gemini_api_key: "AIzaTestKey123".to_string(),
            openai_api_key: "sk-test-openai-456".to_string(),
            anthropic_api_key: "sk-ant-test-789".to_string(),
        };

        store.save(&secrets).unwrap();

        // Stored ciphertext must not contain plaintext API keys on Windows DPAPI
        let on_disk = fs::read_to_string(temp_dir.join("secrets.dpapi")).unwrap();
        #[cfg(target_os = "windows")]
        assert!(!on_disk.contains("AIzaTestKey123"));

        let loaded = store.load();
        assert_eq!(loaded, secrets);

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
