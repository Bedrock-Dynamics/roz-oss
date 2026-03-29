use crate::tui::provider::{Provider, ProviderConfig};

const BOULDER_ART: &str = include_str!("banner.ansi");

/// Build the four info lines for the banner.
pub fn info_lines(config: &ProviderConfig) -> [String; 4] {
    let version = format!("roz v{}", env!("CARGO_PKG_VERSION"));

    let model = config.model.clone();

    let provider = match config.provider {
        Provider::Cloud => "roz cloud".to_string(),
        Provider::Anthropic => "anthropic".to_string(),
        Provider::Ollama => config
            .api_url
            .strip_prefix("http://")
            .or_else(|| config.api_url.strip_prefix("https://"))
            .unwrap_or(&config.api_url)
            .to_string(),
        Provider::Openai => "openai".to_string(),
    };

    let green = "\x1b[38;2;74;143;89m";
    let dim = "\x1b[38;5;245m";
    let reset = "\x1b[0m";

    let status = match config.provider {
        Provider::Anthropic => {
            if config.api_key.is_some() {
                format!("{green}●{reset} key set")
            } else {
                format!("{dim}○{reset} not authenticated")
            }
        }
        Provider::Ollama => format!("{green}●{reset} local"),
        Provider::Cloud | Provider::Openai => {
            if config.api_key.is_some() {
                format!("{green}●{reset} authenticated")
            } else {
                format!("{dim}○{reset} not authenticated")
            }
        }
    };

    [version, model, provider, status]
}

/// Print the boulder banner with provider info side-by-side.
pub fn welcome_banner_with_config(config: &ProviderConfig) {
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    if !is_tty {
        let version = env!("CARGO_PKG_VERSION");
        eprintln!("roz v{version}");
        return;
    }

    let lines = info_lines(config);
    let art_lines: Vec<&str> = BOULDER_ART.lines().collect();
    let bold = "\x1b[1;38;2;232;220;200m";
    let dim = "\x1b[38;5;245m";
    let reset = "\x1b[0m";

    for (i, art_line) in art_lines.iter().enumerate() {
        let info = match i.checked_sub(2) {
            Some(0) => format!("  {bold}{}{reset}", lines[0]),
            Some(1) => format!("  {dim}{}{reset}", lines[1]),
            Some(2) => format!("  {dim}{}{reset}", lines[2]),
            Some(3) => format!("  {}", lines[3]),
            _ => String::new(),
        };
        eprintln!("{art_line}{info}");
    }
    eprintln!();
}

/// Print the ready line (shown after auth is resolved, before TUI).
pub fn welcome_ready() {
    eprintln!("\x1b[38;5;245m/help for help\x1b[0m");
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::provider::{Provider, ProviderConfig};

    #[test]
    fn info_lines_cloud() {
        let config = ProviderConfig {
            provider: Provider::Cloud,
            model: "claude-sonnet-4-6".into(),
            api_key: Some("roz_sk_test".into()),
            api_url: "https://roz-api.fly.dev".into(),
            host: None,
        };
        let lines = info_lines(&config);
        assert_eq!(lines[0], format!("roz v{}", env!("CARGO_PKG_VERSION")));
        assert_eq!(lines[1], "claude-sonnet-4-6");
        assert_eq!(lines[2], "roz cloud");
        assert_eq!(lines[3], "\x1b[38;2;74;143;89m●\x1b[0m authenticated");
    }

    #[test]
    fn info_lines_anthropic() {
        let config = ProviderConfig {
            provider: Provider::Anthropic,
            model: "claude-sonnet-4-6-20250514".into(),
            api_key: Some("sk-ant-test".into()),
            api_url: "https://api.anthropic.com".into(),
            host: None,
        };
        let lines = info_lines(&config);
        assert_eq!(lines[2], "anthropic");
        assert_eq!(lines[3], "\x1b[38;2;74;143;89m●\x1b[0m key set");
    }

    #[test]
    fn info_lines_ollama() {
        let config = ProviderConfig {
            provider: Provider::Ollama,
            model: "llama3:8b".into(),
            api_key: None,
            api_url: "http://localhost:11434".into(),
            host: None,
        };
        let lines = info_lines(&config);
        assert_eq!(lines[2], "localhost:11434");
    }

    #[test]
    fn info_lines_openai() {
        let config = ProviderConfig {
            provider: Provider::Openai,
            model: "gpt-4o".into(),
            api_key: Some("sk-openai-test".into()),
            api_url: "https://api.openai.com".into(),
            host: None,
        };
        let lines = info_lines(&config);
        assert_eq!(lines[2], "openai");
        assert!(lines[3].contains("authenticated"));
    }

    #[test]
    fn info_lines_no_credentials() {
        let config = ProviderConfig {
            provider: Provider::Anthropic,
            model: "claude-sonnet-4-6".into(),
            api_key: None,
            api_url: "https://api.anthropic.com".into(),
            host: None,
        };
        let lines = info_lines(&config);
        assert!(lines[3].contains("not authenticated"));
    }
}
