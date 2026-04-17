use std::path::PathBuf;

/// Full credential record returned by [`CliConfig::load_provider_credential_full`].
///
/// Provides the fields the TUI Provider::Openai arm needs to construct a
/// [`roz_core::model_endpoint::OAuthCredentials`] without re-parsing the JWT
/// on every launch.
#[derive(Debug, Clone)]
pub struct StoredProviderCreds {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub account_id: Option<String>,
}

/// CLI configuration loaded from environment, keyring, and config files.
pub struct CliConfig {
    /// Base URL for the Roz API server.
    pub api_url: String,
    /// Active configuration profile name.
    pub profile: String,
    /// Optional access token for API authentication.
    pub access_token: Option<String>,
}

fn store_in_keyring(service: &str, account: &str, secret: &str) -> bool {
    keyring::Entry::new(service, account)
        .ok()
        .and_then(|entry| entry.set_password(secret).ok())
        .is_some()
}

/// Write `contents` to `path` so the file never exists on disk with looser
/// permissions than 0o600 on Unix (WR-01 TOCTOU fix).
///
/// On Unix, opens with `O_CREAT | O_TRUNC | O_WRONLY` and `mode=0o600` so the
/// file is created with the restricted mode atomically. On non-Unix the
/// fallback is `std::fs::write` (Windows ACLs differ from POSIX modes).
fn write_credentials_file(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new()
            .mode(0o600)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        // Defensive: if the file already existed with looser perms, tighten
        // explicitly. The fresh-create path already sets 0o600 via .mode().
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)?;
    }
    Ok(())
}

impl CliConfig {
    /// Load configuration from environment variables, keyring, and config files.
    ///
    /// The `profile` parameter selects a named configuration profile. If `None`,
    /// the `"default"` profile is used.
    #[allow(clippy::unnecessary_wraps)]
    pub fn load(profile: Option<&str>) -> anyhow::Result<Self> {
        let api_url = std::env::var("ROZ_API_URL").unwrap_or_else(|_| "https://roz-api.fly.dev".into());
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
    /// 1. `ROZ_API_KEY` env var (explicit override)
    /// 2. System keyring (`roz` / profile) — `roz auth login` stores here
    /// 3. `~/.roz/credentials.toml` → `api_key` field
    /// 4. `~/.roz/credentials.toml` → `access_token` field
    /// 5. `ANTHROPIC_API_KEY` env var (BYOK fallback)
    ///
    /// Roz Cloud credentials (from `roz auth login`) always take priority
    /// over `ANTHROPIC_API_KEY`, which is only a fallback for users who
    /// haven't logged in but have a direct Anthropic key.
    pub fn load_global_api_key(profile: &str) -> Option<String> {
        // 1. ROZ_API_KEY env var
        if let Ok(key) = std::env::var("ROZ_API_KEY")
            && !key.is_empty()
        {
            return Some(key);
        }

        // 2. System keyring
        if let Some(key) = keyring::Entry::new("roz", profile)
            .ok()
            .and_then(|e| e.get_password().ok())
        {
            return Some(key);
        }

        // 3-4. ~/.roz/credentials.toml
        if let Some(key) = Self::load_credentials_file(profile) {
            return Some(key);
        }

        // 5. ANTHROPIC_API_KEY env var (BYOK fallback)
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
            && !key.is_empty()
        {
            return Some(key);
        }

        None
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
        if store_in_keyring("roz", profile, api_key) {
            return Ok(());
        }

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
        write_credentials_file(&cred_path, &contents)?;
        Ok(())
    }

    /// Save a credential for a specific provider to `~/.roz/credentials.toml`.
    pub fn save_provider_credential(
        provider: &str,
        access_token: &str,
        refresh_token: Option<&str>,
        expires_in: Option<u64>,
    ) -> anyhow::Result<()> {
        if refresh_token.is_none() && expires_in.is_none() && store_in_keyring("roz-provider", provider, access_token) {
            return Ok(());
        }

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
        write_credentials_file(&cred_path, &contents)?;
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

    /// Save a provider credential with the v2 schema (absolute `expires_at` +
    /// optional `account_id`). Plan 19-15 uses this to record ChatGPT OAuth
    /// sessions so the TUI Provider::Openai arm can reconstruct an
    /// [`roz_core::model_endpoint::OAuthCredentials`] without re-parsing the
    /// JWT on every request.
    ///
    /// Backward compatibility: the file is still readable by
    /// [`Self::load_provider_credential`] (which only needs `access_token`).
    pub fn save_provider_credential_v2(
        provider: &str,
        access_token: &str,
        refresh_token: Option<&str>,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
        account_id: Option<&str>,
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
            if let Some(exp) = expires_at {
                t.insert("expires_at".to_string(), toml::Value::Integer(exp.timestamp()));
            }
            if let Some(acct) = account_id {
                t.insert("account_id".to_string(), toml::Value::String(acct.to_string()));
            }
        }

        let contents = toml::to_string_pretty(&table)?;
        write_credentials_file(&cred_path, &contents)?;
        Ok(())
    }

    /// Full v2 credential load — returns access_token + optional refresh_token
    /// + absolute expires_at + optional account_id.
    ///
    /// Legacy fallback: if `expires_at` is missing but `expires_in` is present,
    /// synthesize an absolute expiry from `expires_in + now()`. If neither is
    /// present, `expires_at` defaults to `now() + 1h` (conservative ChatGPT
    /// default) so the OAuth refresh loop still has a valid comparison point.
    ///
    /// Account id re-extraction from a stored `id_token` is not attempted here
    /// (the CLI does not persist id_token); callers who need account_id must
    /// either run `roz auth login openai` again or have it explicitly saved.
    #[must_use]
    pub fn load_provider_credential_full(provider: &str) -> Option<StoredProviderCreds> {
        let cred_path = Self::config_dir().ok()?.join("credentials.toml");
        let contents = std::fs::read_to_string(cred_path).ok()?;
        let table: toml::Table = contents.parse().ok()?;
        let section = table.get(provider)?.as_table()?;

        let access_token = section
            .get("access_token")
            .or_else(|| section.get("api_key"))
            .and_then(toml::Value::as_str)
            .filter(|s| !s.is_empty())?
            .to_string();

        let refresh_token = section
            .get("refresh_token")
            .and_then(toml::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from);

        let expires_at = section
            .get("expires_at")
            .and_then(toml::Value::as_integer)
            .and_then(|ts| chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0))
            .or_else(|| {
                // Legacy: synthesize from expires_in + now().
                section
                    .get("expires_in")
                    .and_then(toml::Value::as_integer)
                    .map(|secs| chrono::Utc::now() + chrono::Duration::seconds(secs))
            })
            .unwrap_or_else(|| chrono::Utc::now() + chrono::Duration::hours(1));

        let account_id = section
            .get("account_id")
            .and_then(toml::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from);

        Some(StoredProviderCreds {
            access_token,
            refresh_token,
            expires_at,
            account_id,
        })
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
