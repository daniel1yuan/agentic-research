use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

pub const DEFAULT_MAX_TURNS: u32 = 25;
pub const CONNECTIVITY_TIMEOUT_SECS: u64 = 30;
const TOPIC_PREVIEW_LEN: usize = 80;
const SLUG_MAX_LEN: usize = 50;
const ERROR_PREVIEW_LEN: usize = 500;
const STATUS_ERROR_PREVIEW_LEN: usize = 60;

pub const fn topic_preview_len() -> usize { TOPIC_PREVIEW_LEN }
pub const fn slug_max_len() -> usize { SLUG_MAX_LEN }
pub const fn error_preview_len() -> usize { ERROR_PREVIEW_LEN }
pub const fn status_error_preview_len() -> usize { STATUS_ERROR_PREVIEW_LEN }

#[derive(Debug, Deserialize)]
pub struct Config {
    /// CLI command. Can include args, e.g., "ccs production" or just "claude".
    #[serde(default = "default_cli_command")]
    pub cli_command: String,

    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_topics: usize,

    /// In seconds.
    #[serde(default = "default_timeout")]
    pub agent_timeout: u64,

    #[serde(default = "default_model")]
    pub model: String,

    #[serde(default = "default_max_turns")]
    pub max_turns: u32,

    /// Relative to project root.
    #[serde(default = "default_output_dir")]
    pub output_dir: String,

    /// Relative to project root.
    #[serde(default = "default_queue_file")]
    pub queue_file: String,

    /// Relative to project root.
    #[serde(default = "default_prompts_dir")]
    pub prompts_dir: String,

    /// Max USD per topic before the pipeline bails. 0 = no limit.
    #[serde(default)]
    pub max_cost_per_topic: f64,

    /// Per-agent overrides. Keys are agent names (e.g., "synthesizer", "research_academic").
    #[serde(default)]
    pub agents: HashMap<String, AgentOverride>,
}

/// Per-agent config overrides. Any field left unset falls back to the global default.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct AgentOverride {
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub timeout: Option<u64>,
}

impl Config {
    /// Resolve the effective model for a given agent name.
    pub fn model_for(&self, agent_name: &str) -> &str {
        self.agents.get(agent_name)
            .and_then(|o| o.model.as_deref())
            .unwrap_or(&self.model)
    }

    /// Resolve the effective max_turns for a given agent name.
    pub fn max_turns_for(&self, agent_name: &str) -> u32 {
        self.agents.get(agent_name)
            .and_then(|o| o.max_turns)
            .unwrap_or(self.max_turns)
    }

    /// Resolve the effective timeout for a given agent name.
    pub fn timeout_for(&self, agent_name: &str) -> u64 {
        self.agents.get(agent_name)
            .and_then(|o| o.timeout)
            .unwrap_or(self.agent_timeout)
    }

}

fn default_cli_command() -> String { "claude".to_string() }
fn default_max_concurrent() -> usize { 2 }
fn default_timeout() -> u64 { 600 }
fn default_model() -> String { "sonnet".to_string() }
fn default_max_turns() -> u32 { DEFAULT_MAX_TURNS }
fn default_output_dir() -> String { "output".to_string() }
fn default_queue_file() -> String { "queue.yaml".to_string() }
fn default_prompts_dir() -> String { "prompts".to_string() }

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            tracing::info!("No config file found at {}, using defaults", path.display());
            return Ok(Self {
                cli_command: default_cli_command(),
                max_concurrent_topics: default_max_concurrent(),
                agent_timeout: default_timeout(),
                model: default_model(),
                max_turns: default_max_turns(),
                output_dir: default_output_dir(),
                queue_file: default_queue_file(),
                prompts_dir: default_prompts_dir(),
                max_cost_per_topic: 0.0,
                agents: HashMap::new(),
            });
        }

        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        serde_yaml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn missing_file_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&dir.path().join("nonexistent.yaml")).unwrap();

        assert_eq!(config.max_concurrent_topics, 2);
        assert_eq!(config.agent_timeout, 600);
        assert_eq!(config.model, "sonnet");
        assert_eq!(config.output_dir, "output");
        assert_eq!(config.queue_file, "queue.yaml");
    }

    #[test]
    fn full_config_loads_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "max_concurrent_topics: 5").unwrap();
        writeln!(f, "agent_timeout: 300").unwrap();
        writeln!(f, "model: opus").unwrap();
        writeln!(f, "output_dir: results").unwrap();
        writeln!(f, "queue_file: topics.yaml").unwrap();

        let config = Config::load(&path).unwrap();

        assert_eq!(config.max_concurrent_topics, 5);
        assert_eq!(config.agent_timeout, 300);
        assert_eq!(config.model, "opus");
        assert_eq!(config.output_dir, "results");
        assert_eq!(config.queue_file, "topics.yaml");
    }

    #[test]
    fn partial_config_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "model: haiku\n").unwrap();

        let config = Config::load(&path).unwrap();

        assert_eq!(config.model, "haiku");
        assert_eq!(config.max_concurrent_topics, 2);
        assert_eq!(config.agent_timeout, 600);
    }

    #[test]
    fn empty_file_uses_all_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "").unwrap();

        let config = Config::load(&path).unwrap();

        assert_eq!(config.model, "sonnet");
        assert_eq!(config.max_concurrent_topics, 2);
    }

    #[test]
    fn invalid_yaml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "{{{{not yaml").unwrap();

        assert!(Config::load(&path).is_err());
    }
}
