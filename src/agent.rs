use anyhow::Result;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tokio::process::Command;
use tokio::sync::Semaphore;

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
    pub fn new(
        name: &str,
        config: &crate::config::Config,
        prompt: String,
        output_path: PathBuf,
        allowed_tools: &[&str],
    ) -> Self {
        Self {
            name: name.to_string(),
            cli_command: config.cli_command.clone(),
            cli_env: config.cli_env.clone(),
            prompt,
            output_path,
            model: config.model_for(name).to_string(),
            max_turns: config.max_turns_for(name),
            timeout_seconds: config.timeout_for(name),
            allowed_tools: allowed_tools.iter().map(|s| s.to_string()).collect(),
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

/// Wraps an AgentRunner with global and per-model concurrency limits.
pub struct ThrottledRunner {
    inner: Arc<dyn AgentRunner>,
    global: Arc<Semaphore>,
    per_model: HashMap<String, Arc<Semaphore>>,
}

impl ThrottledRunner {
    pub fn new(
        inner: Arc<dyn AgentRunner>,
        max_global: usize,
        model_limits: &HashMap<String, usize>,
    ) -> Self {
        let per_model = model_limits
            .iter()
            .map(|(model, &limit)| (model.clone(), Arc::new(Semaphore::new(limit))))
            .collect();
        Self {
            inner,
            global: Arc::new(Semaphore::new(max_global)),
            per_model,
        }
    }
}

impl AgentRunner for ThrottledRunner {
    fn run_agent(
        &self,
        config: AgentConfig,
    ) -> Pin<Box<dyn Future<Output = Result<AgentResult>> + Send + '_>> {
        let model_sem = self.per_model.get(&config.model).cloned();
        Box::pin(async move {
            let _global_permit = self.global.acquire().await.unwrap();
            let _model_permit = match &model_sem {
                Some(sem) => Some(sem.acquire().await.unwrap()),
                None => None,
            };
            tracing::debug!(
                agent = %config.name,
                model = %config.model,
                "Acquired concurrency permit"
            );
            self.inner.run_agent(config).await
        })
    }
}

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_SECS: u64 = 10;

/// Check if an error response from Claude Code is a transient API error
/// (overloaded, rate-limited) that should be retried.
fn is_transient_error(stdout: &str) -> bool {
    if let Some(result) = extract_result_from_json(stdout) {
        result.contains("overloaded_error")
            || result.contains("rate_limit")
            || result.contains("529")
            || result.contains("529 ")
    } else {
        false
    }
}

async fn invoke_claude(config: AgentConfig) -> Result<AgentResult> {
    let start = Instant::now();

    // The wrapper's only job is to inject the output path (which the prompt file
    // cannot know at authoring time). The prompt itself already tells the agent to
    // use the Write tool and not print findings as text — we do not need to repeat
    // those instructions. Single trailing line keeps the reminder at the recency end
    // of the context where it has the most effect.
    let full_prompt = format!(
        "{prompt}\n\n---\nWrite your complete output to {path} using the Write tool.",
        path = config.output_path.display(),
        prompt = config.prompt,
    );

    let tools_list = config.allowed_tools.join(",");

    let cli_parts: Vec<&str> = config.cli_command.split_whitespace().collect();
    let (bin, prefix_args) = cli_parts.split_first()
        .map(|(b, rest)| (*b, rest))
        .unwrap_or(("claude", &[]));

    if let Some(parent) = config.output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    tracing::info!(agent = %config.name, "Starting agent invocation");

    let mut last_error = None;

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let backoff = INITIAL_BACKOFF_SECS * 2u64.pow(attempt - 1);
            tracing::warn!(
                agent = %config.name,
                attempt = attempt + 1,
                backoff_secs = backoff,
                "Retrying after transient API error"
            );
            tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
        }

        let mut command = Command::new(bin);
        command
            .args(prefix_args)
            .arg("-p")
            .arg(&full_prompt)
            .arg("--output-format").arg("json")
            .arg("--model").arg(&config.model)
            .arg("--max-turns").arg(config.max_turns.to_string())
            .arg("--permission-mode").arg("dontAsk")
            .arg("--allowedTools").arg(&tools_list)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        for (key, val) in &config.cli_env {
            command.env(key, val);
        }

        if let Some(parent) = config.output_path.parent() {
            command.current_dir(parent);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                let duration = start.elapsed().as_secs_f64();
                tracing::error!(agent = %config.name, error = %e, "Agent process error");
                return Ok(AgentResult {
                    success: false,
                    duration_seconds: duration,
                    error: Some(e.to_string()),
                    usage: None,
                    raw_response: None,
                });
            }
        };

        // Take the pipes so we can read them after waiting, while keeping
        // the Child handle alive for kill-on-timeout / kill-on-drop.
        let child_stdout = child.stdout.take();
        let child_stderr = child.stderr.take();

        let wait_and_collect = async {
            let status = child.wait().await?;
            let mut stdout_bytes = Vec::new();
            if let Some(mut out) = child_stdout {
                tokio::io::AsyncReadExt::read_to_end(&mut out, &mut stdout_bytes).await?;
            }
            let mut stderr_bytes = Vec::new();
            if let Some(mut err) = child_stderr {
                tokio::io::AsyncReadExt::read_to_end(&mut err, &mut stderr_bytes).await?;
            }
            Ok::<std::process::Output, std::io::Error>(std::process::Output {
                status,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
            })
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(config.timeout_seconds),
            wait_and_collect,
        )
        .await;

        let duration = start.elapsed().as_secs_f64();

        match result {
            Err(_) => {
                // Timeout — kill_on_drop will also fire, but be explicit.
                let _ = child.kill().await;
                tracing::error!(agent = %config.name, timeout = config.timeout_seconds, "Agent timed out");
                return Ok(AgentResult {
                    success: false,
                    duration_seconds: duration,
                    error: Some(format!("Timed out after {}s", config.timeout_seconds)),
                    usage: None,
                    raw_response: None,
                });
            }
            Ok(Err(e)) => {
                tracing::error!(agent = %config.name, error = %e, "Agent process error");
                return Ok(AgentResult {
                    success: false,
                    duration_seconds: duration,
                    error: Some(e.to_string()),
                    usage: None,
                    raw_response: None,
                });
            }
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let usage = extract_usage_from_json(&stdout);

                if !output.status.success() {
                    // Retry on transient API errors (overloaded, rate-limited).
                    if attempt < MAX_RETRIES && is_transient_error(&stdout) {
                        last_error = Some(stdout);
                        continue;
                    }

                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let preview = if stderr.trim().is_empty() {
                        extract_result_from_json(&stdout)
                            .unwrap_or_else(|| {
                                stdout.chars().take(crate::config::error_preview_len()).collect()
                            })
                    } else {
                        stderr.chars().take(crate::config::error_preview_len()).collect()
                    };
                    tracing::error!(agent = %config.name, exit_code = ?output.status.code(), "Agent failed");
                    return Ok(AgentResult {
                        success: false,
                        duration_seconds: duration,
                        error: Some(format!("Exit code {:?}: {preview}", output.status.code())),
                        usage,
                        raw_response: Some(stdout),
                    });
                }

                // Check for permission denials — the agent ran but couldn't
                // use the tools it needed, so its output is unreliable.
                let denied = extract_permission_denials(&stdout);
                if !denied.is_empty() {
                    let tools = denied.join(", ");
                    tracing::error!(agent = %config.name, denied_tools = %tools, "Agent had permission denials");
                    return Ok(AgentResult {
                        success: false,
                        duration_seconds: duration,
                        error: Some(format!("Permission denied for tools: {tools}")),
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

                return Ok(AgentResult {
                    success: true,
                    duration_seconds: duration,
                    error: None,
                    usage,
                    raw_response: Some(stdout),
                });
            }
        }
    }

    // All retries exhausted (shouldn't normally reach here, but just in case).
    let duration = start.elapsed().as_secs_f64();
    let preview = last_error
        .as_deref()
        .and_then(extract_result_from_json)
        .unwrap_or_else(|| "transient API error".to_string());
    Ok(AgentResult {
        success: false,
        duration_seconds: duration,
        error: Some(format!("Failed after {} retries: {preview}", MAX_RETRIES)),
        usage: None,
        raw_response: last_error,
    })
}

/// Extract tool names that were denied by the permission system.
pub(crate) fn extract_permission_denials(stdout: &str) -> Vec<String> {
    let parsed: serde_json::Value = match serde_json::from_str(stdout) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let denials = match parsed.get("permission_denials").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return Vec::new(),
    };
    let mut tool_names: Vec<String> = denials
        .iter()
        .filter_map(|d| d.get("tool_name").and_then(|v| v.as_str()).map(String::from))
        .collect();
    tool_names.sort();
    tool_names.dedup();
    tool_names
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
    fn new_resolves_config_and_per_agent_overrides() {
        use crate::config::{Config, AgentOverride};

        let mut agents = HashMap::new();
        agents.insert("special_agent".to_string(), AgentOverride {
            model: Some("opus".to_string()),
            max_turns: Some(50),
            timeout: Some(1200),
            max_web_tool_calls: None,
        });

        let config = Config {
            cli_command: "my-claude".to_string(),
            cli_env: HashMap::new(),
            max_concurrent_topics: 1,
            max_concurrent_agents: 1,
            model_concurrency: HashMap::new(),
            agent_timeout: 600,
            model: "sonnet".to_string(),
            max_turns: 25,
            output_dir: String::new(),
            queue_file: String::new(),
            prompts_dir: String::new(),
            max_cost_per_topic: 0.0,
            agents,
        };

        // agent with per-agent overrides
        let special = AgentConfig::new(
            "special_agent", &config, "prompt".into(),
            PathBuf::from("/tmp/out.md"), &["Read", "Write"],
        );
        assert_eq!(special.model, "opus");
        assert_eq!(special.max_turns, 50);
        assert_eq!(special.timeout_seconds, 1200);
        assert_eq!(special.cli_command, "my-claude");
        assert_eq!(special.allowed_tools, vec!["Read", "Write"]);

        // agent without overrides falls back to global defaults
        let regular = AgentConfig::new(
            "regular_agent", &config, "prompt".into(),
            PathBuf::from("/tmp/out.md"), &["Read", "Write", "Glob"],
        );
        assert_eq!(regular.model, "sonnet");
        assert_eq!(regular.max_turns, 25);
        assert_eq!(regular.timeout_seconds, 600);
        assert_eq!(regular.allowed_tools, vec!["Read", "Write", "Glob"]);
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
