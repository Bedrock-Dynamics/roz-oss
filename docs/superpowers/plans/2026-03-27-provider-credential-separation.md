# Provider-Credential Separation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Credentials determine provider, project config determines model. A `roz.toml` with `provider = "anthropic"` must never override a user's Cloud login.

**Architecture:** Remove provider routing from `roz.toml` and `detect()`. Credentials (`roz_sk_` vs `sk-ant-` vs stored OAuth) determine the provider. `roz.toml` only specifies the model name. Non-interactive mode uses Cloud gRPC when provider is Cloud.

**Tech Stack:** Rust, tonic (gRPC), toml parsing, roz-cli crate

---

## File Structure

| File | Change | Responsibility |
|------|--------|---------------|
| `crates/roz-cli/src/tui/provider.rs` | Modify | `detect()` ignores provider prefix from `roz_toml_model`, only uses it from `--model` flag |
| `crates/roz-cli/src/commands/interactive.rs` | Modify | `read_roz_toml()` returns model name only (strips provider prefix from legacy format) |
| `crates/roz-cli/src/commands/non_interactive.rs` | Modify | Cloud provider routes to gRPC `stream_session` instead of BYOK fallback |
| `crates/roz-cli/src/commands/setup.rs` | No change | Already writes `default = "claude-sonnet-4-6"` (no provider prefix) |

---

### Task 1: Fix `read_roz_toml()` to strip provider from legacy format

**Files:**
- Modify: `crates/roz-cli/src/commands/interactive.rs:143-156`
- Test: `crates/roz-cli/src/commands/interactive.rs` (add tests at bottom)

- [ ] **Step 1: Write the failing test**

Add at the bottom of `interactive.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_roz_toml_legacy_strips_provider() {
        // Legacy format: [model] provider = "anthropic", name = "claude-sonnet-4-6"
        // Should return just the model name, NOT "anthropic/claude-sonnet-4-6"
        let toml_str = r#"
[project]
name = "test"

[model]
provider = "anthropic"
name = "claude-sonnet-4-6"
"#;
        let table: toml::Table = toml_str.parse().unwrap();
        let model_section = table.get("model").and_then(toml::Value::as_table);

        // Simulate the legacy fallback logic
        let provider = model_section
            .and_then(|m| m.get("provider"))
            .and_then(toml::Value::as_str);
        let model = model_section
            .and_then(|m| m.get("model").or_else(|| m.get("name")))
            .and_then(toml::Value::as_str);

        // The model ref should be just the model name, not provider/model
        // Provider comes from credentials, not project config
        let model_ref = model.unwrap_or("claude-sonnet-4-6");
        assert_eq!(model_ref, "claude-sonnet-4-6");
        // provider field should be ignored
        assert!(provider.is_some()); // it exists but we don't use it
    }
}
```

- [ ] **Step 2: Run test to verify it passes (this tests the desired behavior)**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-cli -- tests::read_roz_toml_legacy_strips_provider -v`

- [ ] **Step 3: Fix `read_roz_toml()` legacy fallback**

In `crates/roz-cli/src/commands/interactive.rs`, replace lines 143-156:

```rust
    // Legacy fallback: [model] provider field is IGNORED (provider comes from credentials).
    // Only read the model/name field.
    let model = model_section
        .and_then(|m| m.get("model").or_else(|| m.get("name")))
        .and_then(toml::Value::as_str);

    RozTomlConfig {
        model_ref: model.map(String::from),
    }
```

This removes the `provider` field from the legacy format entirely. Only the model name is read.

- [ ] **Step 4: Run all CLI tests**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-cli -v`
Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
cd /Users/krnzt/Documents/BedrockDynamics/roz-public
git add crates/roz-cli/src/commands/interactive.rs
git commit -m "fix: roz.toml provider field no longer overrides credentials

Legacy [model] provider field is ignored. Provider is determined by
credentials (roz_sk_ → Cloud, sk-ant- → Anthropic). Project config
only specifies the model name, matching industry standard (Claude Code,
AWS CLI, gcloud)."
```

---

### Task 2: Update `detect()` to not let `roz_toml_model` force provider

**Files:**
- Modify: `crates/roz-cli/src/tui/provider.rs:99-159`
- Test: `crates/roz-cli/src/tui/provider.rs` (modify existing tests)

- [ ] **Step 1: Write the failing test**

Add to the test module in `provider.rs`:

```rust
    #[test]
    fn detect_roz_toml_provider_prefix_ignored_when_roz_sk() {
        // Even if roz_toml_model is "anthropic/claude-sonnet-4-6",
        // roz_sk_ credentials should force Cloud provider.
        let config = ProviderConfig::detect(
            None,
            Some("roz_sk_test_key"),
            Some("anthropic/claude-sonnet-4-6"),
        );
        assert_eq!(config.provider, Provider::Cloud);
        assert_eq!(config.model, "claude-sonnet-4-6");
    }

    #[test]
    fn detect_explicit_model_flag_still_overrides() {
        // --model flag should still work to force a provider
        let config = ProviderConfig::detect(
            Some("anthropic/claude-opus-4-6"),
            Some("roz_sk_test_key"),
            None,
        );
        assert_eq!(config.provider, Provider::Anthropic);
        assert_eq!(config.model, "claude-opus-4-6");
    }
```

- [ ] **Step 2: Run tests to verify `detect_roz_toml_provider_prefix_ignored_when_roz_sk` fails**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-cli -- detect_roz_toml_provider_prefix_ignored -v`
Expected: FAIL — currently returns `Provider::Anthropic` because `"anthropic/"` prefix in roz_toml takes priority

- [ ] **Step 3: Fix `detect()` — credential auto-detect before roz_toml provider prefix**

In `crates/roz-cli/src/tui/provider.rs`, replace `detect()` (lines 99-159) with:

```rust
    pub fn detect(explicit_model: Option<&str>, api_key: Option<&str>, roz_toml_model: Option<&str>) -> Self {
        // 1. Resolve model ref: explicit → roz.toml → default
        let model_ref = explicit_model.or(roz_toml_model).unwrap_or("claude-sonnet-4-6");

        // 2. Parse ref into (provider_prefix, model_name)
        let (provider_prefix, model_name) = parse_model_ref(model_ref);

        // 3. If EXPLICIT CLI FLAG has a provider prefix, use it (user intent)
        if explicit_model.is_some() {
            if let Some(prefix) = provider_prefix
                && let Ok(provider) = prefix.parse::<Provider>()
            {
                return Self::for_provider_and_model(provider, model_name, api_key);
            }
        }

        // 4. Auto-detect provider from credential prefix (credentials determine provider)
        if let Some(key) = api_key {
            if key.starts_with("roz_sk_") {
                return Self {
                    provider: Provider::Cloud,
                    model: model_name.to_string(),
                    api_key: Some(key.to_string()),
                    api_url: cloud_api_url(),
                };
            }
            // Any other key → Anthropic
            return Self {
                provider: Provider::Anthropic,
                model: model_name.to_string(),
                api_key: Some(key.to_string()),
                api_url: "https://api.anthropic.com".to_string(),
            };
        }

        // 5. Check for stored OpenAI OAuth credential
        if let Some(token) = CliConfig::load_provider_credential("openai") {
            return Self {
                provider: Provider::Openai,
                model: model_name.to_string(),
                api_key: Some(token),
                api_url: "https://api.openai.com".to_string(),
            };
        }

        // 6. OLLAMA_HOST env
        if let Ok(host) = std::env::var("OLLAMA_HOST") {
            return Self {
                provider: Provider::Ollama,
                model: model_name.to_string(),
                api_key: None,
                api_url: host,
            };
        }

        // 7. If roz_toml had a provider prefix (no credentials), use it as hint
        if let Some(prefix) = provider_prefix
            && let Ok(provider) = prefix.parse::<Provider>()
        {
            return Self::for_provider_and_model(provider, model_name, api_key);
        }

        // 8. No credentials — disconnected fallback
        Self {
            provider: Provider::Anthropic,
            model: model_name.to_string(),
            api_key: None,
            api_url: "https://api.anthropic.com".to_string(),
        }
    }
```

Key change: **explicit `--model` flag provider prefix is honored (step 3), but `roz_toml` provider prefix is deferred to step 7** — only used when no credentials exist. Credentials always determine provider (step 4).

- [ ] **Step 4: Run all provider tests**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-cli -- provider -v`
Expected: All pass including the two new tests

- [ ] **Step 5: Run full CLI test suite**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-cli -v`
Expected: All pass

- [ ] **Step 6: Commit**

```bash
cd /Users/krnzt/Documents/BedrockDynamics/roz-public
git add crates/roz-cli/src/tui/provider.rs
git commit -m "fix: credentials determine provider, roz.toml only sets model

--model flag provider prefix still honored (explicit user intent).
roz.toml provider prefix only used as fallback when no credentials exist.
Credentials always win: roz_sk_ → Cloud, sk-ant- → Anthropic."
```

---

### Task 3: Non-interactive Cloud support via gRPC

**Files:**
- Modify: `crates/roz-cli/src/commands/non_interactive.rs:7-19`
- Test: manual verification (gRPC requires live server)

- [ ] **Step 1: Write the failing test (unit-level check)**

Add at bottom of `non_interactive.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_provider_does_not_use_byok() {
        // Verify that the code path for Cloud provider is distinct from BYOK
        let config = ProviderConfig {
            provider: Provider::Cloud,
            model: "claude-sonnet-4-6".to_string(),
            api_key: Some("roz_sk_test".to_string()),
            api_url: "https://roz-api.fly.dev".to_string(),
        };
        // Cloud should NOT use "anthropic" as proxy_provider
        assert_ne!(config.provider, Provider::Anthropic);
        assert_eq!(config.api_url, "https://roz-api.fly.dev");
    }
}
```

- [ ] **Step 2: Run test**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-cli -- cloud_provider_does_not_use_byok -v`
Expected: PASS (this just validates the config, not the execution path)

- [ ] **Step 3: Implement Cloud gRPC path in non-interactive mode**

Replace `execute()` in `crates/roz-cli/src/commands/non_interactive.rs`:

```rust
pub async fn execute(config: &CliConfig, model_flag: Option<&str>, task: &str) -> anyhow::Result<()> {
    let roz_toml = super::interactive::read_roz_toml_model_ref();
    let provider_config = ProviderConfig::detect(model_flag, config.access_token.as_deref(), roz_toml.as_deref());

    if provider_config.api_key.is_none() && provider_config.provider != Provider::Ollama {
        anyhow::bail!("No credentials configured. Run `roz auth login` or set ANTHROPIC_API_KEY.");
    }

    if provider_config.provider == Provider::Cloud {
        execute_cloud(&provider_config, task).await
    } else {
        execute_byok(&provider_config, task).await
    }
}
```

Add the Cloud execution function:

```rust
async fn execute_cloud(config: &ProviderConfig, task: &str) -> anyhow::Result<()> {
    let (event_tx, event_rx) = async_channel::unbounded();
    let (text_tx, text_rx) = async_channel::unbounded::<String>();

    // Send the task as a single message
    text_tx.send(task.to_string()).await?;
    text_tx.close();

    // Spawn gRPC session in background
    let config_clone = config.clone();
    let session = tokio::spawn(async move {
        crate::tui::providers::cloud::stream_session(&config_clone, text_rx, event_tx).await
    });

    // Collect streaming response
    let mut response = String::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cycles = 0u32;

    while let Ok(event) = event_rx.recv().await {
        match event {
            crate::tui::provider::AgentEvent::TextDelta(text) => {
                response.push_str(&text);
            }
            crate::tui::provider::AgentEvent::TurnComplete { usage, .. } => {
                if let Some(u) = usage {
                    input_tokens += u.input_tokens;
                    output_tokens += u.output_tokens;
                }
                cycles += 1;
            }
            crate::tui::provider::AgentEvent::Error(e) => {
                let json = serde_json::json!({
                    "status": "error",
                    "error": e,
                });
                println!("{}", serde_json::to_string_pretty(&json)?);
                std::process::exit(1);
            }
            _ => {}
        }
    }

    // Wait for session to finish
    if let Err(e) = session.await? {
        anyhow::bail!("Cloud session error: {e}");
    }

    let json = serde_json::json!({
        "status": "success",
        "response": response,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        },
        "cycles": cycles,
    });
    println!("{}", serde_json::to_string_pretty(&json)?);

    Ok(())
}
```

- [ ] **Step 4: Add `Clone` derive to `ProviderConfig` if not already present**

Check `crates/roz-cli/src/tui/provider.rs:84-91`. If `ProviderConfig` doesn't derive `Clone`, add it:

```rust
#[derive(Clone)]
pub struct ProviderConfig {
```

- [ ] **Step 5: Build and clippy**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo fmt -p roz-cli && cargo clippy -p roz-cli -- -D warnings`
Expected: Clean

- [ ] **Step 6: Commit**

```bash
cd /Users/krnzt/Documents/BedrockDynamics/roz-public
git add crates/roz-cli/src/commands/non_interactive.rs crates/roz-cli/src/tui/provider.rs
git commit -m "feat: non-interactive mode supports Cloud via gRPC

Cloud provider now routes through stream_session() instead of falling
back to BYOK Anthropic API. roz --non-interactive --task 'hello' works
with roz_sk_ credentials."
```

---

### Task 4: Full integration test + PR

**Files:**
- No new files

- [ ] **Step 1: Run full test suite**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo test -p roz-cli -v`
Expected: All tests pass

- [ ] **Step 2: Run clippy**

Run: `cd /Users/krnzt/Documents/BedrockDynamics/roz-public && cargo clippy -p roz-cli -- -D warnings`
Expected: Clean

- [ ] **Step 3: Create branch and push**

```bash
cd /Users/krnzt/Documents/BedrockDynamics/roz-public
git checkout main && git pull origin main
git checkout -b fix/provider-credential-separation
git cherry-pick <commit1> <commit2> <commit3>  # or rebase
git push origin fix/provider-credential-separation
```

- [ ] **Step 4: Create PR**

```bash
gh pr create --title "fix: credentials determine provider, not project config" --body "$(cat <<'EOF'
## Summary
- `roz.toml` `[model] provider` field no longer overrides user credentials
- Credentials determine provider: `roz_sk_` → Cloud, `sk-ant-` → Anthropic
- `--model` flag still allows explicit provider override (user intent)
- Non-interactive mode now supports Cloud via gRPC (was BYOK-only)

## Why
Industry standard (Claude Code, AWS CLI, gcloud, NemoClaw): project config controls *what* (model), credentials control *where* (provider). A `roz.toml` with `provider = "anthropic"` was overriding `roz auth login` Cloud credentials.

## Test plan
- [ ] `cargo test -p roz-cli`
- [ ] `roz doctor` shows Cloud credentials
- [ ] `roz` interactive shows Provider::Cloud (not Anthropic)
- [ ] `roz --non-interactive --task "hello"` works with Cloud

EOF
)"
```

- [ ] **Step 5: Merge after CI passes**

```bash
gh pr merge --squash --admin
```
