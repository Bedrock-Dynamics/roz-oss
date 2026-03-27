use std::path::PathBuf;

/// CLI configuration loaded from environment, keyring, and config files.
pub struct CliConfig {
    /// Base URL for the Roz API server.
    pub api_url: String,
    /// Active configuration profile name.
    pub profile: String,
    /// Optional access token for API authentication.
    pub access_token: Option<String>,
}

impl CliConfig {
    /// Load configuration from environment variables, keyring, and config files.
    ///
    /// The `profile` parameter selects a named configuration profile. If `None`,
    /// the `"default"` profile is used.
    #[allow(clippy::unnecessary_wraps)]
    pub fn load(profile: Option<&str>) -> anyhow::Result<Self> {
        let api_url = std::env::var("ROZ_API_URL").unwrap_or_else(|_| "http://localhost:8080".into());
        let profile_name = profile.unwrap_or("default").to_string();
        let access_token = Self::load_global_api_key(&profile_name);

        Ok(Self {
            api_url,
            profile: profile_name,
            access_token,
        })
    }

    /// Returns `~/.roz/` as the global configuration directory.
    pub fn config_dir() -> anyhow::Result<PathBuf> {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .map_err(|_| anyhow::anyhow!("cannot determine home directory"))?;
        Ok(PathBuf::from(home).join(".roz"))
    }

    /// Resolve an API key or access token from all credential sources.
    ///
    /// Priority:
    /// 1. `ROZ_API_KEY` env var
    /// 2. `ANTHROPIC_API_KEY` env var
    /// 3. System keyring (`roz` / profile)
    /// 4. `~/.roz/credentials.toml` → `api_key` field
    /// 5. `~/.roz/credentials.toml` → `access_token` field
    pub fn load_global_api_key(profile: &str) -> Option<String> {
        // 1. ROZ_API_KEY env var
        if let Ok(key) = std::env::var("ROZ_API_KEY")
            && !key.is_empty()
        {
            return Some(key);
        }

        // 2. ANTHROPIC_API_KEY env var
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
            && !key.is_empty()
        {
            return Some(key);
        }

        // 3. System keyring
        if let Some(key) = keyring::Entry::new("roz", profile)
            .ok()
            .and_then(|e| e.get_password().ok())
        {
            return Some(key);
        }

        // 4-5. ~/.roz/credentials.toml
        Self::load_credentials_file(profile)
    }

    /// Read `api_key` or `access_token` from `~/.roz/credentials.toml`.
    fn load_credentials_file(profile: &str) -> Option<String> {
        let cred_path = Self::config_dir().ok()?.join("credentials.toml");
        let contents = std::fs::read_to_string(cred_path).ok()?;
        let table: toml::Table = contents.parse().ok()?;
        let section = table.get(profile)?.as_table()?;

        // Try api_key first (BYOK), then access_token (Roz Cloud OAuth)
        section
            .get("api_key")
            .and_then(toml::Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                section
                    .get("access_token")
                    .and_then(toml::Value::as_str)
                    .filter(|s| !s.is_empty())
            })
            .map(String::from)
    }

    /// Check if any credentials are available.
    #[allow(dead_code)] // Public API for credential checks.
    pub const fn has_credentials(&self) -> bool {
        self.access_token.is_some()
    }

    /// Save an API key to `~/.roz/credentials.toml`.
    pub fn save_global_api_key(profile: &str, api_key: &str) -> anyhow::Result<()> {
        let config_dir = Self::config_dir()?;
        std::fs::create_dir_all(&config_dir)?;
        let cred_path = config_dir.join("credentials.toml");

        let mut table: toml::Table = if cred_path.exists() {
            std::fs::read_to_string(&cred_path)?.parse().unwrap_or_default()
        } else {
            toml::Table::new()
        };

        let section = table
            .entry(profile.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let toml::Value::Table(t) = section {
            t.insert("api_key".to_string(), toml::Value::String(api_key.to_string()));
        }

        let contents = toml::to_string_pretty(&table)?;
        std::fs::write(&cred_path, &contents)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cred_path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }

    /// Save a credential for a specific provider to `~/.roz/credentials.toml`.
    pub fn save_provider_credential(
        provider: &str,
        access_token: &str,
        refresh_token: Option<&str>,
        expires_in: Option<u64>,
    ) -> anyhow::Result<()> {
        let config_dir = Self::config_dir()?;
        std::fs::create_dir_all(&config_dir)?;
        let cred_path = config_dir.join("credentials.toml");

        let mut table: toml::Table = if cred_path.exists() {
            std::fs::read_to_string(&cred_path)?.parse().unwrap_or_default()
        } else {
            toml::Table::new()
        };

        let section = table
            .entry(provider.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let toml::Value::Table(t) = section {
            t.insert(
                "access_token".to_string(),
                toml::Value::String(access_token.to_string()),
            );
            if let Some(rt) = refresh_token {
                t.insert("refresh_token".to_string(), toml::Value::String(rt.to_string()));
            }
            if let Some(exp) = expires_in {
                let expires_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock before Unix epoch")
                    .as_secs()
                    + exp;
                t.insert("expires_at".to_string(), toml::Value::Integer(expires_at.cast_signed()));
            }
        }

        let contents = toml::to_string_pretty(&table)?;
        std::fs::write(&cred_path, &contents)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cred_path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Load credential for a specific provider from `~/.roz/credentials.toml`.
    ///
    /// Note: does not check `expires_at` — token refresh is handled separately (Phase B2).
    pub fn load_provider_credential(provider: &str) -> Option<String> {
        let cred_path = Self::config_dir().ok()?.join("credentials.toml");
        let contents = std::fs::read_to_string(cred_path).ok()?;
        let table: toml::Table = contents.parse().ok()?;
        let section = table.get(provider)?.as_table()?;
        section
            .get("access_token")
            .or_else(|| section.get("api_key"))
            .and_then(toml::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
    }

    /// Build an HTTP client pre-configured with auth headers.
    pub fn api_client(&self) -> anyhow::Result<reqwest::Client> {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(token) = &self.access_token {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))?,
            );
        }
        Ok(reqwest::Client::builder().default_headers(headers).build()?)
    }
}
