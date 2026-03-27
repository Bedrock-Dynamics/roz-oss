use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub database_url: String,
    pub nats_url: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8080,
            database_url: String::new(),
            nats_url: None,
        }
    }
}

impl Config {
    #[allow(dead_code)]
    pub fn from_env() -> Result<Self, Box<figment::Error>> {
        use figment::{Figment, providers::Env};
        Ok(Figment::new().merge(Env::prefixed("ROZ_")).extract()?)
    }
}
