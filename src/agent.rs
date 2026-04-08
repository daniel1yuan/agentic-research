use anyhow::Result;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Instant;
use tokio::process::Command;

#[derive(Debug, Clone, Default)]
pub struct AgentUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug)]
pub struct AgentResult {
    pub success: bool,
    pub duration_seconds: f64,
    pub error: Option<String>,
    pub usage: Option<AgentUsage>,
    /// Raw JSON response from claude CLI (for analytics).
    pub raw_response: Option<String>,
}

pub struct AgentConfig {
    pub name: String,
    pub cli_command: String,
    pub cli_env: std::collections::HashMap<String, String>,
    pub prompt: String,
    pub output_path: PathBuf,
    pub model: String,
    pub max_turns: u32,
    pub timeout_seconds: u64,
    pub allowed_tools: Vec<String>,
}

impl AgentConfig {
    pub fn research(name: &str, cli_command: &str, cli_env: &std::collections::HashMap<String, String>, prompt: String, output_path: PathBuf, model: &str, max_turns: u32, timeout: u64) -> Self {
        Self {
            name: name.to_string(),
            cli_command: cli_command.to_string(),
            cli_env: cli_env.clone(),
            prompt,
            output_path,
            model: model.to_string(),
            max_turns,
            timeout_seconds: timeout,
            allowed_tools: vec![
                "WebSearch".into(), "WebFetch".into(),
                "Read".into(), "Write".into(),
            ],
        }
    }

    pub fn synthesis(cli_command: &str, cli_env: &std::collections::HashMap<String, String>, prompt: String, output_path: PathBuf, model: &str, max_turns: u32, timeout: u64) -> Self {
        Self {
            name: "synthesizer".to_string(),
            cli_command: cli_command.to_string(),
            cli_env: cli_env.clone(),
            prompt,
            output_path,
            model: model.to_string(),
            max_turns,
            timeout_seconds: timeout,
            allowed_tools: vec![
                "Read".into(), "Write".into(), "Glob".into(), "Grep".into(),
            ],
        }
    }

    pub fn validator(name: &str, cli_command: &str, cli_env: &std::collections::HashMap<String, String>, prompt: String, output_path: PathBuf, model: &str, max_turns: u32, timeout: u64, needs_web: bool) -> Self {
        let mut tools = vec![
            "Read".into(), "Write".into(), "Glob".into(), "Grep".into(),
        ];
        if needs_web {
            tools.push("WebSearch".into());
            tools.push("WebFetch".into());
        }

        Self {
            name: name.to_string(),
            cli_command: cli_command.to_string(),
            cli_env: cli_env.clone(),
            prompt,
            output_path,
            model: model.to_string(),
            max_turns,
            timeout_seconds: timeout,
            allowed_tools: tools,
        }
    }

    pub fn revision(cli_command: &str, cli_env: &std::collections::HashMap<String, String>, prompt: String, output_path: PathBuf, model: &str, max_turns: u32, timeout: u64) -> Self {
        Self {
            name: "revision".to_string(),
            cli_command: cli_command.to_string(),
            cli_env: cli_env.clone(),
            prompt,
            output_path,
            model: model.to_string(),
            max_turns,
            timeout_seconds: timeout,
            allowed_tools: vec![
                "Read".into(), "Write".into(), "Glob".into(), "Grep".into(),
                "WebSearch".into(), "WebFetch".into(),
            ],
        }
    }
}

/// Injectable so we can swap in a fake for testing.
pub trait AgentRunner: Send + Sync {
    fn run_agent(
        &self,
        config: AgentConfig,
    ) -> Pin<Box<dyn Future<Output = Result<AgentResult>> + Send + '_>>;
}

pub struct ClaudeRunner;

impl AgentRunner for ClaudeRunner {
    fn run_agent(
        &self,
        config: AgentConfig,
    ) -> Pin<Box<dyn Future<Output = Result<AgentResult>> + Send + '_>> {
        Box::pin(invoke_claude(config))
    }
}

async fn invoke_claude(config: AgentConfig) -> Result<AgentResult> {
    let start = Instant::now();

    let full_prompt = format!(
        "{}\n\n---\nWrite your complete output to: {}\nUse the Write tool to save your findings. Do not print them to stdout.",
        config.prompt,
        config.output_path.display()
    );

    let tools_json = serde_json::to_string(&config.allowed_tools)?;

    let cli_parts: Vec<&str> = config.cli_command.split_whitespace().collect();
    let (bin, prefix_args) = cli_parts.split_first()
        .map(|(b, rest)| (*b, rest))
        .unwrap_or(("claude", &[]));

    let mut command = Command::new(bin);
    command
        .args(prefix_args)
        .arg("-p")
        .arg(&full_prompt)
        .arg("--output-format").arg("json")
        .arg("--model").arg(&config.model)
        .arg("--max-turns").arg(config.max_turns.to_string())
        .arg("--allowedTools").arg(&tools_json)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    for (key, val) in &config.cli_env {
        command.env(key, val);
    }

    if let Some(parent) = config.output_path.parent() {
        std::fs::create_dir_all(parent)?;
        command.current_dir(parent);
    }

    tracing::info!(agent = %config.name, "Starting agent invocation");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(config.timeout_seconds),
        command.output(),
    )
    .await;

    let duration = start.elapsed().as_secs_f64();

    match result {
        Err(_) => {
            tracing::error!(agent = %config.name, timeout = config.timeout_seconds, "Agent timed out");
            Ok(AgentResult {
                success: false,
                duration_seconds: duration,
                error: Some(format!("Timed out after {}s", config.timeout_seconds)),
                usage: None,
                raw_response: None,
            })
        }
        Ok(Err(e)) => {
            tracing::error!(agent = %config.name, error = %e, "Agent process error");
            Ok(AgentResult {
                success: false,
                duration_seconds: duration,
                error: Some(e.to_string()),
                usage: None,
                raw_response: None,
            })
        }
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let usage = extract_usage_from_json(&stdout);

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let preview: String = stderr.chars().take(crate::config::error_preview_len()).collect();
                tracing::error!(agent = %config.name, exit_code = ?output.status.code(), "Agent failed");
                return Ok(AgentResult {
                    success: false,
                    duration_seconds: duration,
                    error: Some(format!("Exit code {:?}: {preview}", output.status.code())),
                    usage,
                    raw_response: Some(stdout),
                });
            }

            // sometimes the agent returns its output in stdout instead of
            // writing the file. fall back to extracting from the json response.
            if !config.output_path.exists() {
                tracing::warn!(agent = %config.name, "Output file not created, extracting from response");
                let response_text = extract_result_from_json(&stdout).unwrap_or_else(|| stdout.to_string());
                if let Some(parent) = config.output_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                crate::queue::atomic_write(&config.output_path, &response_text)?;
            }

            if let Some(ref u) = usage {
                tracing::info!(
                    agent = %config.name,
                    duration_s = format!("{duration:.1}"),
                    cost_usd = format!("{:.4}", u.cost_usd),
                    input_tokens = u.input_tokens,
                    output_tokens = u.output_tokens,
                    "Agent completed"
                );
            } else {
                tracing::info!(agent = %config.name, duration_s = format!("{duration:.1}"), "Agent completed");
            }

            Ok(AgentResult {
                success: true,
                duration_seconds: duration,
                error: None,
                usage,
                raw_response: Some(stdout),
            })
        }
    }
}

pub(crate) fn extract_result_from_json(stdout: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(stdout).ok()?;
    parsed.get("result").and_then(|v| v.as_str()).map(String::from)
}

pub(crate) fn extract_usage_from_json(stdout: &str) -> Option<AgentUsage> {
    let parsed: serde_json::Value = serde_json::from_str(stdout).ok()?;
    let usage = parsed.get("usage")?;

    Some(AgentUsage {
        input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_creation_tokens: usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_read_tokens: usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cost_usd: parsed.get("total_cost_usd").and_then(|v| v.as_f64()).unwrap_or(0.0),
    })
}

pub async fn test_cli_connectivity(cli_command: &str, cli_env: &std::collections::HashMap<String, String>) -> Result<bool> {
    let parts: Vec<&str> = cli_command.split_whitespace().collect();
    let (bin, prefix_args) = parts.split_first()
        .map(|(b, rest)| (*b, rest))
        .unwrap_or(("claude", &[]));

    let mut cmd = Command::new(bin);
    cmd.args(prefix_args)
        .arg("-p")
        .arg("Respond with exactly: OK")
        .arg("--output-format").arg("json")
        .arg("--max-turns").arg("1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    for (key, val) in cli_env {
        cmd.env(key, val);
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(crate::config::CONNECTIVITY_TIMEOUT_SECS),
        cmd.output(),
    )
    .await;

    match output {
        Err(_) => Ok(false),
        Ok(Err(_)) => Ok(false),
        Ok(Ok(o)) => Ok(o.status.success()),
    }
}

pub async fn get_auth_status(cli_command: &str, cli_env: &std::collections::HashMap<String, String>) -> Option<String> {
    let bin = cli_command.split_whitespace().next().unwrap_or("claude");

    let mut cmd = std::process::Command::new(bin);
    cmd.arg("auth").arg("status").arg("--text")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    for (key, val) in cli_env {
        cmd.env(key, val);
    }

    let output = cmd.output().ok().filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&output.stdout);

    // parse key fields from "Key: Value" lines
    let mut org = None;
    let mut email = None;
    for line in text.lines() {
        if let Some(val) = line.strip_prefix("Organization: ") {
            org = Some(val.trim());
        } else if let Some(val) = line.strip_prefix("Email: ") {
            email = Some(val.trim());
        }
    }

    match (org, email) {
        (Some(o), Some(e)) => Some(format!("{e} @ {o}")),
        (None, Some(e)) => Some(e.to_string()),
        (Some(o), None) => Some(o.to_string()),
        (None, None) => Some(text.lines().next().unwrap_or("authenticated").trim().to_string()),
    }
}

/// Check if the CLI binary is available (just the binary, not any prefix args).
pub fn is_cli_installed(cli_command: &str) -> bool {
    let bin = cli_command.split_whitespace().next().unwrap_or("claude");
    std::process::Command::new("which")
        .arg(bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn load_prompt(prompts_dir: &Path, filename: &str) -> Result<String> {
    let path = prompts_dir.join(filename);
    std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("Failed to read prompt {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_result_field_from_valid_json() {
        let json = r#"{"result": "hello world", "other": 42}"#;
        assert_eq!(extract_result_from_json(json), Some("hello world".to_string()));
    }

    #[test]
    fn returns_none_for_missing_result_field() {
        let json = r#"{"output": "no result key here"}"#;
        assert_eq!(extract_result_from_json(json), None);
    }

    #[test]
    fn returns_none_for_non_string_result() {
        let json = r#"{"result": 42}"#;
        assert_eq!(extract_result_from_json(json), None);
    }

    #[test]
    fn returns_none_for_invalid_json() {
        assert_eq!(extract_result_from_json("not json at all"), None);
    }

    #[test]
    fn returns_none_for_empty_string() {
        assert_eq!(extract_result_from_json(""), None);
    }

    #[test]
    fn research_config_has_web_tools() {
        let config = AgentConfig::research(
            "test_agent", "claude", &Default::default(), "prompt".into(), PathBuf::from("/tmp/out.md"), "sonnet", 25, 600,
        );
        assert_eq!(config.name, "test_agent");
        assert!(config.allowed_tools.contains(&"WebSearch".to_string()));
        assert!(config.allowed_tools.contains(&"WebFetch".to_string()));
        assert!(!config.allowed_tools.contains(&"Glob".to_string()));
    }

    #[test]
    fn synthesis_config_has_no_web_tools() {
        let config = AgentConfig::synthesis(
            "claude", &Default::default(), "prompt".into(), PathBuf::from("/tmp/out.md"), "sonnet", 25, 600,
        );
        assert_eq!(config.name, "synthesizer");
        assert!(!config.allowed_tools.contains(&"WebSearch".to_string()));
        assert!(config.allowed_tools.contains(&"Glob".to_string()));
    }

    #[test]
    fn validator_config_adds_web_tools_when_requested() {
        let with_web = AgentConfig::validator(
            "source_check", "claude", &Default::default(), "prompt".into(), PathBuf::from("/tmp/out.md"), "sonnet", 25, 600, true,
        );
        let without_web = AgentConfig::validator(
            "bias_check", "claude", &Default::default(), "prompt".into(), PathBuf::from("/tmp/out.md"), "sonnet", 25, 600, false,
        );
        assert!(with_web.allowed_tools.contains(&"WebSearch".to_string()));
        assert!(!without_web.allowed_tools.contains(&"WebSearch".to_string()));
    }

    #[test]
    fn revision_config_has_all_tools() {
        let config = AgentConfig::revision(
            "claude", &Default::default(), "prompt".into(), PathBuf::from("/tmp/out.md"), "opus", 25, 900,
        );
        assert_eq!(config.name, "revision");
        assert_eq!(config.model, "opus");
        assert!(config.allowed_tools.contains(&"WebSearch".to_string()));
        assert!(config.allowed_tools.contains(&"Glob".to_string()));
    }

    #[test]
    fn loads_existing_prompt_file() {
        let dir = tempfile::tempdir().unwrap();
        let content = "# Test Prompt\nDo the thing with {topic}";
        std::fs::write(dir.path().join("test.md"), content).unwrap();
        let loaded = load_prompt(dir.path(), "test.md").unwrap();
        assert_eq!(loaded, content);
    }

    #[test]
    fn returns_error_for_missing_prompt() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_prompt(dir.path(), "nonexistent.md").is_err());
    }

    // --- extract_usage_from_json ---

    #[test]
    fn extracts_usage_from_real_claude_response() {
        let json = r#"{
            "type": "result",
            "result": "OK",
            "total_cost_usd": 0.107,
            "usage": {
                "input_tokens": 3,
                "output_tokens": 4,
                "cache_creation_input_tokens": 17142,
                "cache_read_input_tokens": 500
            }
        }"#;

        let usage = extract_usage_from_json(json).unwrap();
        assert_eq!(usage.input_tokens, 3);
        assert_eq!(usage.output_tokens, 4);
        assert_eq!(usage.cache_creation_tokens, 17142);
        assert_eq!(usage.cache_read_tokens, 500);
        assert!((usage.cost_usd - 0.107).abs() < 0.001);
    }

    #[test]
    fn usage_returns_none_for_missing_usage_field() {
        let json = r#"{"result": "OK"}"#;
        assert!(extract_usage_from_json(json).is_none());
    }

    #[test]
    fn usage_returns_none_for_invalid_json() {
        assert!(extract_usage_from_json("not json").is_none());
    }

    #[test]
    fn usage_handles_partial_fields() {
        let json = r#"{
            "usage": { "input_tokens": 100 },
            "total_cost_usd": 0.05
        }"#;
        let usage = extract_usage_from_json(json).unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.cache_creation_tokens, 0);
        assert!((usage.cost_usd - 0.05).abs() < 0.001);
    }
}
