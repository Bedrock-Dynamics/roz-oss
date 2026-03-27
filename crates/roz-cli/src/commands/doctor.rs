use crate::config::CliConfig;

pub async fn execute(config: &CliConfig) -> anyhow::Result<()> {
    eprintln!("roz doctor\n");

    let mut all_ok = true;

    // 1. Check credentials
    check("Roz Cloud credentials", check_roz_cloud(config), &mut all_ok);
    check("OpenAI credentials", check_openai(), &mut all_ok);
    check("Anthropic credentials", check_anthropic(), &mut all_ok);
    check("Ollama availability", check_ollama().await, &mut all_ok);

    // 2. Check roz.toml
    check("Project config (roz.toml)", check_roz_toml(), &mut all_ok);

    // 3. Check sessions directory
    check("Sessions directory", check_sessions_dir(), &mut all_ok);

    eprintln!();
    if all_ok {
        eprintln!("All checks passed.");
    } else {
        eprintln!("Some checks failed. See above for details.");
    }
    Ok(())
}

fn check(name: &str, result: Result<String, String>, all_ok: &mut bool) {
    match result {
        Ok(detail) => eprintln!("  \u{2713} {name}: {detail}"),
        Err(detail) => {
            eprintln!("  \u{2717} {name}: {detail}");
            *all_ok = false;
        }
    }
}

fn check_roz_cloud(config: &CliConfig) -> Result<String, String> {
    match &config.access_token {
        Some(token) if token.starts_with("roz_sk_") => {
            Ok(format!("roz_sk_...{}", &token[token.len().saturating_sub(4)..]))
        }
        Some(_) => Ok("token present (non-roz_sk_)".to_string()),
        None => Err("not authenticated. Run `roz auth login`".to_string()),
    }
}

fn check_openai() -> Result<String, String> {
    match CliConfig::load_provider_credential("openai") {
        Some(_) => Ok("OAuth token stored".to_string()),
        None => Err("not authenticated. Run `roz auth login openai`".to_string()),
    }
}

fn check_anthropic() -> Result<String, String> {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && !key.is_empty()
    {
        return Ok(format!("ANTHROPIC_API_KEY set ({}...)", &key[..key.len().min(8)]));
    }
    Err("ANTHROPIC_API_KEY not set".to_string())
}

async fn check_ollama() -> Result<String, String> {
    let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".to_string());
    match reqwest::Client::new()
        .get(format!("{host}/api/tags"))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => Ok(format!("running at {host}")),
        Ok(resp) => Err(format!("responded with {}", resp.status())),
        Err(_) => Err(format!("not reachable at {host}")),
    }
}

fn check_roz_toml() -> Result<String, String> {
    if std::path::Path::new("roz.toml").exists() {
        let contents = std::fs::read_to_string("roz.toml").map_err(|e| e.to_string())?;
        let table: toml::Table = contents.parse().map_err(|e| format!("parse error: {e}"))?;
        if table.get("model").is_some() {
            Ok("valid".to_string())
        } else {
            Err("missing [model] section".to_string())
        }
    } else {
        Err("not found (run `roz` in a project directory to create one)".to_string())
    }
}

fn check_sessions_dir() -> Result<String, String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "HOME not set".to_string())?;
    let dir = std::path::PathBuf::from(home).join(".roz").join("sessions");
    if dir.exists() {
        let count = std::fs::read_dir(&dir)
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or(0);
        Ok(format!("{count} sessions"))
    } else {
        Ok("directory will be created on first session".to_string())
    }
}
