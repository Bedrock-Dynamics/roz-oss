use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub database_url: String,
    pub nats_url: Option<String>,
    /// Phase 18 D-discretion: object-store root for skill bundled assets.
    /// Defaults to `{data_dir}/skills` (data_dir defaults to `/var/lib/roz`).
    /// Env: `ROZ_SKILL_STORE_ROOT`.
    #[serde(default)]
    pub skill_store_root: Option<std::path::PathBuf>,
    /// Local data directory; used as the default base for `skill_store_root` when
    /// the latter is unset. Defaults to `/var/lib/roz`. Env: `ROZ_DATA_DIR`.
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

fn default_data_dir() -> String {
    "/var/lib/roz".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8080,
            database_url: String::new(),
            nats_url: None,
            skill_store_root: None,
            data_dir: default_data_dir(),
        }
    }
}

impl Config {
    #[allow(dead_code)]
    pub fn from_env() -> Result<Self, Box<figment::Error>> {
        use figment::{Figment, providers::Env};
        Ok(Figment::new().merge(Env::prefixed("ROZ_")).extract()?)
    }

    /// Resolve the effective `skill_store_root` path, defaulting to
    /// `{data_dir}/skills` when unset (Phase 18 D-discretion in CONTEXT).
    #[allow(dead_code)]
    #[must_use]
    pub fn resolved_skill_store_root(&self) -> std::path::PathBuf {
        self.skill_store_root
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(&self.data_dir).join("skills"))
    }
}
