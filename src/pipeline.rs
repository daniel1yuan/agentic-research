use anyhow::Result;
use futures::future::join_all;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::agent::{self, AgentConfig, AgentRunner, ThrottledRunner};
use crate::config::Config;
use crate::progress;
use crate::queue::{AgentStatus, QueueManager, Topic, TopicStatus};
use crate::roster;

/// A previous run is considered complete for this output only if both the output
/// file has content AND a sidecar `.done` marker exists next to it. The marker is
/// written after the agent reports success, so a partial write (agent crashed or
/// timed out mid-stream, leaving a truncated fragment on disk) will be re-run on
/// resume instead of being silently accepted as cache.
pub(crate) fn agent_output_exists(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > 0 => sidecar_path(path).exists(),
        _ => false,
    }
}

/// The completion sidecar for an agent output file. For `academic.md` this is
/// `academic.md.done`. Empty file — its existence is the signal.
pub(crate) fn sidecar_path(output_path: &Path) -> PathBuf {
    let mut s = output_path.as_os_str().to_os_string();
    s.push(".done");
    PathBuf::from(s)
}

/// Write the `.done` sidecar next to a freshly-completed output file. Failures to
/// write the sidecar are logged but not fatal — the worst case is that the output
/// gets re-run on the next resume, which is the safer failure mode.
fn mark_output_complete(output_path: &Path) {
    let sidecar = sidecar_path(output_path);
    if let Err(e) = std::fs::write(&sidecar, "") {
        tracing::warn!(
            path = %sidecar.display(),
            error = %e,
            "Failed to write completion sidecar marker"
        );
    }
}

/// Terminal state of a pipeline run. `Done` means the verifier passed all checks
/// and the topic can be marked complete. `NeedsReview` means the pipeline reached
/// verify cleanly but the verifier flagged the final document — the topic is marked
/// `needs_review` and removed from the queue, and `recover` can re-run it.
enum PipelineOutcome {
    Done,
    NeedsReview(String),
}

/// The subset of the verify agent's YAML frontmatter we actually parse. We deliberately
/// do not parse the per-check metrics — the `overall` verdict and `failed_checks` list
/// are what drive topic-level routing. Everything else is for human consumption in the
/// markdown body.
#[derive(Debug, Deserialize)]
struct VerifyReport {
    #[serde(default)]
    overall: String,
    #[serde(default)]
    failed_checks: Vec<String>,
}

/// Extract the YAML frontmatter block from a markdown file. Returns None if the file
/// does not start with `---\n` or if no closing `---\n` is found.
fn extract_yaml_frontmatter(content: &str) -> Option<&str> {
    // Accept either LF or CRLF after the opening marker.
    let rest = content.strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;
    // Find the closing `---` on its own line.
    let end = rest.find("\n---\n")
        .or_else(|| rest.find("\n---\r\n"))?;
    Some(&rest[..end])
}

/// Parse verify.md and return a verdict. Any failure mode (missing file, missing
/// frontmatter, malformed YAML, `overall: fail`) produces `NeedsReview` — per the
/// contract, a broken verifier must not silently pass topics.
fn parse_verify_report(path: &Path) -> PipelineOutcome {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => return PipelineOutcome::NeedsReview(format!("could not read verify.md: {e}")),
    };
    let yaml = match extract_yaml_frontmatter(&content) {
        Some(y) => y,
        None => return PipelineOutcome::NeedsReview("verify.md is missing YAML frontmatter".to_string()),
    };
    let report: VerifyReport = match serde_yaml::from_str(yaml) {
        Ok(r) => r,
        Err(e) => return PipelineOutcome::NeedsReview(format!("malformed verify frontmatter: {e}")),
    };
    if report.overall == "pass" {
        PipelineOutcome::Done
    } else {
        let checks = if report.failed_checks.is_empty() {
            "unspecified".to_string()
        } else {
            report.failed_checks.join(", ")
        };
        PipelineOutcome::NeedsReview(format!("verify failed: {checks}"))
    }
}

struct TopicPipeline {
    topic: Topic,
    topic_dir: PathBuf,
    research_dir: PathBuf,
    validation_dir: PathBuf,
    config: Arc<Config>,
    runner: Arc<dyn AgentRunner>,
    queue_manager: Arc<Mutex<QueueManager>>,
}

impl TopicPipeline {
    fn new(
        topic: Topic,
        output_dir: &Path,
        config: Arc<Config>,
        runner: Arc<dyn AgentRunner>,
        queue_manager: Arc<Mutex<QueueManager>>,
    ) -> Self {
        let topic_dir = output_dir.join(&topic.id);
        let research_dir = topic_dir.join("research");
        let validation_dir = topic_dir.join("validation");

        Self {
            topic,
            topic_dir,
            research_dir,
            validation_dir,
            config,
            runner,
            queue_manager,
        }
    }

    fn prompts_dir(&self) -> PathBuf {
        // config.prompts_dir is relative, but pipeline resolves it the same way main.rs does
        // since WorkerPool already resolved it to an absolute path stored in config
        PathBuf::from(&self.config.prompts_dir)
    }

    async fn check_cost_limit(&self) -> Result<()> {
        if self.config.max_cost_per_topic <= 0.0 {
            return Ok(());
        }
        let meta = self.queue_manager.lock().await.read_meta(&self.topic)?;
        let total_cost: f64 = meta.agents.values().filter_map(|a| a.cost_usd).sum();
        if total_cost > self.config.max_cost_per_topic {
            anyhow::bail!(
                "Cost limit exceeded: ${:.4} spent, limit is ${:.4}",
                total_cost, self.config.max_cost_per_topic
            );
        }
        Ok(())
    }

    async fn run(&self) -> bool {
        match self.run_inner().await {
            Ok(PipelineOutcome::Done) => {
                if let Err(e) = self.queue_manager.lock().await.complete_topic(&self.topic) {
                    tracing::error!(topic = %self.topic.id, error = %e, "Failed to mark topic complete");
                    return false;
                }
                let cost = self.total_cost().await;
                progress::topic_done(&self.topic.id, cost);
                true
            }
            Ok(PipelineOutcome::NeedsReview(reason)) => {
                if let Err(e) = self.queue_manager.lock().await.mark_needs_review(&self.topic, &reason) {
                    tracing::error!(topic = %self.topic.id, error = %e, "Failed to mark topic as needs_review");
                    return false;
                }
                let cost = self.total_cost().await;
                progress::topic_needs_review(&self.topic.id, &reason, cost);
                // Pipeline reached a clean terminal state. Return true so the worker
                // pool counts this as "not errored"; the needs_review state is recorded
                // in meta.yaml and the topic is out of the queue regardless.
                true
            }
            Err(e) => {
                tracing::error!(topic = %self.topic.id, error = %e, "Pipeline failed");
                progress::topic_failed(&self.topic.id, &e.to_string());
                if let Err(write_err) = self.queue_manager.lock().await.fail_topic(&self.topic, &e.to_string()) {
                    tracing::error!(topic = %self.topic.id, error = %write_err, "Failed to record topic failure in queue");
                }
                false
            }
        }
    }

    async fn total_cost(&self) -> f64 {
        self.queue_manager.lock().await.read_meta(&self.topic)
            .map(|m| m.agents.values().filter_map(|a| a.cost_usd).sum())
            .unwrap_or(0.0)
    }

    async fn run_inner(&self) -> Result<PipelineOutcome> {
        self.setup_directories()?;
        self.queue_manager.lock().await.claim_topic(&self.topic)?;

        progress::phase(&self.topic.id, "Researching...");
        let research_ok = self.run_research_phase().await?;
        if !research_ok {
            anyhow::bail!("Research phase failed: no agents succeeded and no prior results to use");
        }
        self.check_cost_limit().await?;

        progress::phase(&self.topic.id, "Synthesizing...");
        self.queue_manager.lock().await.update_status(&self.topic, TopicStatus::Synthesizing)?;
        let synthesis_ok = self.run_synthesis_phase().await?;
        if !synthesis_ok {
            anyhow::bail!("Synthesis phase failed");
        }
        self.check_cost_limit().await?;

        progress::phase(&self.topic.id, "Validating...");
        self.queue_manager.lock().await.update_status(&self.topic, TopicStatus::Validating)?;
        let (validators_passed, validators_total) = self.run_validation_phase().await?;
        // Contract: require at least 2 of 4 validators to succeed. Running triage on
        // a single validator's findings produces low-quality action lists and makes
        // the downstream FIX/REJECT/DEFER signal meaningless.
        if validators_total > 0 && validators_passed < 2 {
            anyhow::bail!(
                "Validation phase failed ({}/{} validators succeeded, need at least 2)",
                validators_passed, validators_total
            );
        }
        if validators_passed < validators_total {
            tracing::warn!(
                topic = %self.topic.id,
                passed = validators_passed,
                total = validators_total,
                "Some validators failed, proceeding with partial validation"
            );
        }

        self.check_cost_limit().await?;

        progress::phase(&self.topic.id, "Triaging...");
        self.queue_manager.lock().await.update_status(&self.topic, TopicStatus::Triaging)?;
        self.run_triage_phase().await?;
        self.check_cost_limit().await?;

        progress::phase(&self.topic.id, "Revising...");
        self.queue_manager.lock().await.update_status(&self.topic, TopicStatus::Revising)?;
        self.run_revision_phase().await?;
        self.check_cost_limit().await?;

        progress::phase(&self.topic.id, "Verifying...");
        self.queue_manager.lock().await.update_status(&self.topic, TopicStatus::Verifying)?;
        self.run_verify_phase().await?;

        // Verify phase produced verify.md; parse its frontmatter to decide the outcome.
        // complete_topic / mark_needs_review is called by `run()`, not here.
        Ok(parse_verify_report(&self.topic_dir.join("verify.md")))
    }

    fn setup_directories(&self) -> Result<()> {
        std::fs::create_dir_all(&self.research_dir)?;
        std::fs::create_dir_all(self.topic_dir.join("sources"))?;
        std::fs::create_dir_all(&self.validation_dir)?;
        std::fs::create_dir_all(self.responses_dir())?;
        Ok(())
    }

    fn responses_dir(&self) -> PathBuf {
        self.topic_dir.join("responses")
    }

    fn save_raw_response(&self, agent_name: &str, result: &crate::agent::AgentResult) {
        if let Some(ref raw) = result.raw_response {
            let path = self.responses_dir().join(format!("{agent_name}.json"));
            if let Err(e) = crate::queue::atomic_write(&path, raw) {
                tracing::warn!(agent = %agent_name, error = %e, "Failed to save raw response");
            }
        }
    }

    async fn run_research_phase(&self) -> Result<bool> {
        let mut already_done = 0;
        let mut agent_names = Vec::new();
        let mut agent_output_paths: Vec<PathBuf> = Vec::new();
        let mut agent_futures = Vec::new();

        for def in roster::RESEARCH_AGENTS {
            let output_path = self.research_dir.join(def.output_file);
            let name = def.name;

            if agent_output_exists(&output_path) {
                progress::agent_cached(name);
                self.queue_manager.lock().await.record_agent_result(
                    &self.topic, name, AgentStatus::DoneCached, 0.0, None, None,
                )?;
                already_done += 1;
                continue;
            }

            let prompt_template = agent::load_prompt(&self.prompts_dir(), def.prompt_file)?;
            let prompt = prompt_template.replace("{topic}", &self.topic.input);
            let agent_config = AgentConfig::new(
                name, &self.config, prompt, output_path.clone(), def.allowed_tools,
            );
            agent_names.push(name);
            agent_output_paths.push(output_path);
            agent_futures.push(self.runner.run_agent(agent_config));
        }

        if !agent_names.is_empty() {
            progress::agents_starting(&agent_names);
        }
        let heartbeat = progress::start_heartbeat(30);
        let results = join_all(agent_futures).await;
        heartbeat.stop();

        let mut newly_succeeded = 0;
        for ((name, output_path), result) in agent_names.iter().zip(agent_output_paths.iter()).zip(results) {
            let mut qm = self.queue_manager.lock().await;
            match result {
                Ok(result) => {
                    progress::agent_done(name, &result);
                    self.save_raw_response(name, &result);
                    let agent_status = if result.success { AgentStatus::Done } else { AgentStatus::Failed };
                    qm.record_agent_result(
                        &self.topic, name, agent_status, result.duration_seconds,
                        result.error.as_deref(), result.usage.as_ref(),
                    )?;
                    if result.success {
                        newly_succeeded += 1;
                        drop(qm);
                        mark_output_complete(output_path);
                    }
                }
                Err(e) => {
                    progress::agent_error(name, &e.to_string());
                    qm.record_agent_result(
                        &self.topic, name, AgentStatus::Failed, 0.0, Some(&e.to_string()), None,
                    )?;
                }
            }
        }

        Ok((already_done + newly_succeeded) > 0)
    }

    async fn run_synthesis_phase(&self) -> Result<bool> {
        let output_path = self.topic_dir.join("overview.md");

        if agent_output_exists(&output_path) {
            progress::agent_cached("synthesizer");
            self.queue_manager.lock().await.record_agent_result(&self.topic, "synthesis", AgentStatus::DoneCached, 0.0, None, None)?;
            return Ok(true);
        }

        let prompt_template = agent::load_prompt(&self.prompts_dir(), roster::SYNTHESIS_PROMPT)?;
        let prompt = prompt_template
            .replace("{topic}", &self.topic.input)
            .replace("{research_dir}", &self.research_dir.to_string_lossy());

        let agent_config = AgentConfig::new(
            "synthesizer", &self.config, prompt, output_path.clone(), roster::SYNTHESIS_TOOLS,
        );

        progress::agents_starting(&["synthesizer"]);
        let heartbeat = progress::start_heartbeat(30);
        let result = self.runner.run_agent(agent_config).await?;
        heartbeat.stop();

        progress::agent_done("synthesizer", &result);
        self.save_raw_response("synthesis", &result);

        let agent_status = if result.success { AgentStatus::Done } else { AgentStatus::Failed };
        self.queue_manager.lock().await.record_agent_result(
            &self.topic, "synthesis", agent_status, result.duration_seconds,
            result.error.as_deref(), result.usage.as_ref(),
        )?;

        if result.success {
            mark_output_complete(&output_path);
        }

        Ok(result.success)
    }

    /// Returns (succeeded_count, total_count).
    async fn run_validation_phase(&self) -> Result<(usize, usize)> {
        let synthesis_str = self.topic_dir.join("overview.md").to_string_lossy().to_string();
        let research_str = self.research_dir.to_string_lossy().to_string();
        let validation_str = self.validation_dir.to_string_lossy().to_string();

        let total = roster::VALIDATION_AGENTS.len();
        let mut succeeded = 0;
        let mut agent_names = Vec::new();
        let mut agent_output_paths: Vec<PathBuf> = Vec::new();
        let mut agent_futures = Vec::new();

        for def in roster::VALIDATION_AGENTS {
            let output_path = self.validation_dir.join(def.output_file);
            let name = def.name;

            if agent_output_exists(&output_path) {
                progress::agent_cached(name);
                self.queue_manager.lock().await.record_agent_result(
                    &self.topic, name, AgentStatus::DoneCached, 0.0, None, None,
                )?;
                succeeded += 1;
                continue;
            }

            let prompt_template = agent::load_prompt(&self.prompts_dir(), def.prompt_file)?;
            let max_web_tool_calls = self.config.max_web_tool_calls_for(name).to_string();
            let prompt = prompt_template
                .replace("{topic}", &self.topic.input)
                .replace("{synthesis_path}", &synthesis_str)
                .replace("{research_dir}", &research_str)
                .replace("{validation_dir}", &validation_str)
                .replace("{max_web_tool_calls}", &max_web_tool_calls);

            let agent_config = AgentConfig::new(
                name, &self.config, prompt, output_path.clone(), def.allowed_tools,
            );
            agent_names.push(name);
            agent_output_paths.push(output_path);
            agent_futures.push(self.runner.run_agent(agent_config));
        }

        if !agent_names.is_empty() {
            progress::agents_starting(&agent_names);
        }
        let heartbeat = progress::start_heartbeat(30);
        let results = join_all(agent_futures).await;
        heartbeat.stop();

        for ((name, output_path), result) in agent_names.iter().zip(agent_output_paths.iter()).zip(results) {
            let mut qm = self.queue_manager.lock().await;
            match result {
                Ok(result) => {
                    progress::agent_done(name, &result);
                    self.save_raw_response(name, &result);
                    let agent_status = if result.success { AgentStatus::Done } else { AgentStatus::Failed };
                    qm.record_agent_result(
                        &self.topic, name, agent_status, result.duration_seconds,
                        result.error.as_deref(), result.usage.as_ref(),
                    )?;
                    if result.success {
                        succeeded += 1;
                        drop(qm);
                        mark_output_complete(output_path);
                    }
                }
                Err(e) => {
                    progress::agent_error(name, &e.to_string());
                    qm.record_agent_result(
                        &self.topic, name, AgentStatus::Failed, 0.0, Some(&e.to_string()), None,
                    )?;
                }
            }
        }

        Ok((succeeded, total))
    }

    async fn run_triage_phase(&self) -> Result<()> {
        let output_path = self.topic_dir.join("triage.md");

        if agent_output_exists(&output_path) {
            progress::agent_cached("triage");
            self.queue_manager.lock().await.record_agent_result(&self.topic, "triage", AgentStatus::DoneCached, 0.0, None, None)?;
            return Ok(());
        }

        let prompt_template = agent::load_prompt(&self.prompts_dir(), roster::TRIAGE_PROMPT)?;
        let prompt = prompt_template
            .replace("{validation_dir}", &self.validation_dir.to_string_lossy());

        let agent_config = AgentConfig::new(
            "triage", &self.config, prompt, output_path.clone(), roster::TRIAGE_TOOLS,
        );

        progress::agents_starting(&["triage"]);
        let heartbeat = progress::start_heartbeat(30);
        let result = self.runner.run_agent(agent_config).await?;
        heartbeat.stop();

        progress::agent_done("triage", &result);
        self.save_raw_response("triage", &result);

        let agent_status = if result.success { AgentStatus::Done } else { AgentStatus::Failed };
        self.queue_manager.lock().await.record_agent_result(
            &self.topic, "triage", agent_status, result.duration_seconds,
            result.error.as_deref(), result.usage.as_ref(),
        )?;

        if !result.success {
            anyhow::bail!("Triage phase failed");
        }

        mark_output_complete(&output_path);
        Ok(())
    }

    async fn run_revision_phase(&self) -> Result<()> {
        let output_path = self.topic_dir.join("overview_final.md");

        if agent_output_exists(&output_path) {
            progress::agent_cached("revision");
            self.queue_manager.lock().await.record_agent_result(&self.topic, "revision", AgentStatus::DoneCached, 0.0, None, None)?;
            return Ok(());
        }

        let prompt_template = agent::load_prompt(&self.prompts_dir(), roster::REVISION_PROMPT)?;
        let prompt = prompt_template
            .replace("{synthesis_path}", &self.topic_dir.join("overview.md").to_string_lossy())
            .replace("{triage_path}", &self.topic_dir.join("triage.md").to_string_lossy())
            .replace("{final_path}", &output_path.to_string_lossy());

        let agent_config = AgentConfig::new(
            "revision", &self.config, prompt, output_path.clone(), roster::REVISION_TOOLS,
        );

        progress::agents_starting(&["revision"]);
        let heartbeat = progress::start_heartbeat(30);
        let result = self.runner.run_agent(agent_config).await?;
        heartbeat.stop();

        progress::agent_done("revision", &result);
        self.save_raw_response("revision", &result);

        let agent_status = if result.success { AgentStatus::Done } else { AgentStatus::Failed };
        self.queue_manager.lock().await.record_agent_result(
            &self.topic, "revision", agent_status, result.duration_seconds,
            result.error.as_deref(), result.usage.as_ref(),
        )?;

        if !result.success {
            anyhow::bail!("Revision phase failed");
        }

        mark_output_complete(&output_path);
        Ok(())
    }

    async fn run_verify_phase(&self) -> Result<()> {
        let output_path = self.topic_dir.join("verify.md");

        if agent_output_exists(&output_path) {
            progress::agent_cached("verify");
            self.queue_manager.lock().await.record_agent_result(&self.topic, "verify", AgentStatus::DoneCached, 0.0, None, None)?;
            return Ok(());
        }

        let prompt_template = agent::load_prompt(&self.prompts_dir(), roster::VERIFY_PROMPT)?;
        let prompt = prompt_template
            .replace("{final_path}", &self.topic_dir.join("overview_final.md").to_string_lossy())
            .replace("{triage_path}", &self.topic_dir.join("triage.md").to_string_lossy());

        let agent_config = AgentConfig::new(
            "verify", &self.config, prompt, output_path.clone(), roster::VERIFY_TOOLS,
        );

        progress::agents_starting(&["verify"]);
        let heartbeat = progress::start_heartbeat(30);
        let result = self.runner.run_agent(agent_config).await?;
        heartbeat.stop();

        progress::agent_done("verify", &result);
        self.save_raw_response("verify", &result);

        let agent_status = if result.success { AgentStatus::Done } else { AgentStatus::Failed };
        self.queue_manager.lock().await.record_agent_result(
            &self.topic, "verify", agent_status, result.duration_seconds,
            result.error.as_deref(), result.usage.as_ref(),
        )?;

        if !result.success {
            anyhow::bail!("Verify phase failed");
        }

        mark_output_complete(&output_path);
        Ok(())
    }
}

pub struct WorkerPool {
    output_dir: PathBuf,
    config: Arc<Config>,
}

impl WorkerPool {
    pub fn new(output_dir: PathBuf, config: Arc<Config>) -> Self {
        Self { output_dir, config }
    }

    pub async fn process_all(
        &self,
        topics: &[Topic],
        queue_manager: QueueManager,
    ) -> Vec<(String, bool)> {
        let inner: Arc<dyn AgentRunner> = Arc::new(agent::ClaudeRunner);
        let runner: Arc<dyn AgentRunner> = Arc::new(ThrottledRunner::new(
            inner,
            self.config.max_concurrent_agents,
            &self.config.model_concurrency,
        ));
        self.process_all_with_runner(topics, queue_manager, runner).await
    }

    pub(crate) async fn process_all_with_runner(
        &self,
        topics: &[Topic],
        queue_manager: QueueManager,
        runner: Arc<dyn AgentRunner>,
    ) -> Vec<(String, bool)> {
        let qm = Arc::new(Mutex::new(queue_manager));
        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.config.max_concurrent_topics));

        let mut handles = Vec::new();

        for topic in topics {
            let sem = semaphore.clone();
            let qm = qm.clone();
            let runner = runner.clone();
            let output_dir = self.output_dir.clone();
            let config = self.config.clone();
            let topic = topic.clone();

            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();
                tracing::info!(topic_id = %topic.id, "Worker acquired slot");

                let pipeline = TopicPipeline::new(
                    topic.clone(),
                    &output_dir,
                    config,
                    runner,
                    qm,
                );

                let success = pipeline.run().await;
                (topic.id.clone(), success)
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => {
                    tracing::error!(error = %e, "Topic task panicked");
                    results.push(("unknown".to_string(), false));
                }
            }
        }

        let succeeded = results.iter().filter(|(_, s)| *s).count();
        let failed = results.iter().filter(|(_, s)| !*s).count();
        tracing::info!(succeeded, failed, "All topics processed");

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentConfig, AgentResult, AgentRunner};
    use crate::queue::{AgentStatus, QueueManager, Topic, TopicStatus};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // --- agent_output_exists ---

    /// Test helper: write content and the completion sidecar together, simulating
    /// what a successful agent run produces.
    fn write_cached(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
        std::fs::write(sidecar_path(path), "").unwrap();
    }

    #[test]
    fn agent_output_exists_false_for_missing_file() {
        assert!(!agent_output_exists(Path::new("/tmp/definitely-not-a-real-file-xyz.md")));
    }

    #[test]
    fn agent_output_exists_false_for_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.md");
        std::fs::write(&path, "").unwrap();
        assert!(!agent_output_exists(&path));
    }

    #[test]
    fn agent_output_exists_false_without_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("research.md");
        // Content but no `.done` sidecar — simulates a partial write.
        std::fs::write(&path, "# Partial findings — crashed mid-stream").unwrap();
        assert!(
            !agent_output_exists(&path),
            "content without sidecar must not be treated as cached"
        );
    }

    #[test]
    fn agent_output_exists_true_with_content_and_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("research.md");
        write_cached(&path, "# Findings\nSome content here");
        assert!(agent_output_exists(&path));
    }

    #[test]
    fn agent_output_exists_false_with_only_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("research.md");
        std::fs::write(sidecar_path(&path), "").unwrap();
        // Sidecar exists but no content file at all.
        assert!(
            !agent_output_exists(&path),
            "sidecar without content must not be treated as cached"
        );
    }

    #[test]
    fn sidecar_path_appends_done_extension() {
        assert_eq!(
            sidecar_path(Path::new("/tmp/research/academic.md")),
            PathBuf::from("/tmp/research/academic.md.done")
        );
    }

    // --- Fake runners for pipeline tests ---

    struct FakeRunner {
        invocation_count: AtomicUsize,
        should_succeed: bool,
    }

    impl FakeRunner {
        fn succeeding() -> Self {
            Self { invocation_count: AtomicUsize::new(0), should_succeed: true }
        }

        fn failing() -> Self {
            Self { invocation_count: AtomicUsize::new(0), should_succeed: false }
        }

        fn invocations(&self) -> usize {
            self.invocation_count.load(Ordering::SeqCst)
        }
    }

    impl AgentRunner for FakeRunner {
        fn run_agent(
            &self,
            config: AgentConfig,
        ) -> Pin<Box<dyn Future<Output = Result<AgentResult>> + Send + '_>> {
            self.invocation_count.fetch_add(1, Ordering::SeqCst);
            let success = self.should_succeed;

            Box::pin(async move {
                if success {
                    if let Some(parent) = config.output_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let content = fake_output_for(&config.name);
                    std::fs::write(&config.output_path, content)?;
                }
                Ok(AgentResult {
                    success,
                    duration_seconds: 1.0,
                    error: if success { None } else { Some("fake failure".to_string()) },
                    usage: None,
                    raw_response: None,
                })
            })
        }
    }

    /// Content the fake runners write for each agent. Most agents get generic placeholder
    /// content; the verify agent gets a valid passing YAML frontmatter so the pipeline's
    /// frontmatter parser reads it as `overall: pass`. Tests that want NeedsReview or
    /// failing verify behavior use the dedicated verdict runners below.
    fn fake_output_for(name: &str) -> String {
        if name == "verify" {
            fake_verify_passing()
        } else {
            format!("# {name} output\nFake content for testing")
        }
    }

    fn fake_verify_passing() -> String {
        "---\n\
         overall: pass\n\
         needs_human_review: false\n\
         failed_checks: []\n\
         ---\n\
         \n\
         # Verification Report\n\
         \n\
         ## Overall: PASS\n\
         All fake checks pass.\n".to_string()
    }

    fn fake_verify_failing() -> String {
        "---\n\
         overall: fail\n\
         needs_human_review: true\n\
         failed_checks:\n\
           - fix_application\n\
           - origin_tags\n\
         ---\n\
         \n\
         # Verification Report\n\
         \n\
         ## Overall: FAIL\n\
         Fake failing verify report.\n".to_string()
    }

    fn fake_verify_no_frontmatter() -> String {
        "# Verification Report\n\nNo frontmatter at all.\n".to_string()
    }

    /// Fails only the agents whose names are in `failing_names`. Everyone else succeeds.
    struct SelectiveFakeRunner {
        invocation_count: AtomicUsize,
        failing_names: Vec<String>,
    }

    impl SelectiveFakeRunner {
        fn failing_agents(names: &[&str]) -> Self {
            Self {
                invocation_count: AtomicUsize::new(0),
                failing_names: names.iter().map(|s| s.to_string()).collect(),
            }
        }

        fn invocations(&self) -> usize {
            self.invocation_count.load(Ordering::SeqCst)
        }
    }

    impl AgentRunner for SelectiveFakeRunner {
        fn run_agent(
            &self,
            config: AgentConfig,
        ) -> Pin<Box<dyn Future<Output = Result<AgentResult>> + Send + '_>> {
            self.invocation_count.fetch_add(1, Ordering::SeqCst);
            let should_fail = self.failing_names.contains(&config.name);

            Box::pin(async move {
                let success = !should_fail;
                if success {
                    if let Some(parent) = config.output_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&config.output_path, fake_output_for(&config.name))?;
                }
                Ok(AgentResult {
                    success,
                    duration_seconds: 1.0,
                    error: if should_fail { Some(format!("{} failed", config.name)) } else { None },
                    usage: None,
                    raw_response: None,
                })
            })
        }
    }

    /// A runner that succeeds every agent but overrides the verify agent's output with
    /// a specific file content. Used to test the NeedsReview path without having to
    /// thread verdict logic through FakeRunner.
    struct FakeRunnerWithVerifyContent {
        invocation_count: AtomicUsize,
        verify_content: String,
    }

    impl FakeRunnerWithVerifyContent {
        fn new(verify_content: String) -> Self {
            Self {
                invocation_count: AtomicUsize::new(0),
                verify_content,
            }
        }

        fn invocations(&self) -> usize {
            self.invocation_count.load(Ordering::SeqCst)
        }
    }

    impl AgentRunner for FakeRunnerWithVerifyContent {
        fn run_agent(
            &self,
            config: AgentConfig,
        ) -> Pin<Box<dyn Future<Output = Result<AgentResult>> + Send + '_>> {
            self.invocation_count.fetch_add(1, Ordering::SeqCst);
            let verify_content = self.verify_content.clone();

            Box::pin(async move {
                if let Some(parent) = config.output_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let content = if config.name == "verify" {
                    verify_content
                } else {
                    fake_output_for(&config.name)
                };
                std::fs::write(&config.output_path, content)?;
                Ok(AgentResult {
                    success: true,
                    duration_seconds: 1.0,
                    error: None,
                    usage: None,
                    raw_response: None,
                })
            })
        }
    }

    fn make_topic(id: &str, input: &str) -> Topic {
        Topic { id: id.to_string(), input: input.to_string() }
    }

    /// Returns (tempdir, queue_path, output_dir, prompts_dir).
    /// Use `make_qm` to create QueueManager instances as needed.
    fn setup_pipeline_test() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let queue_path = dir.path().join("queue.yaml");
        let output_dir = dir.path().join("output");
        let prompts_dir = dir.path().join("prompts");

        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(&queue_path, "topics: []\n").unwrap();

        for filename in &roster::all_prompt_files() {
            std::fs::write(
                prompts_dir.join(filename),
                format!("# {filename}\nResearch {{topic}}\n{{research_dir}}\n{{synthesis_path}}\n{{validation_dir}}"),
            ).unwrap();
        }

        (dir, queue_path, output_dir, prompts_dir)
    }

    fn make_qm(queue_path: &Path, output_dir: &Path) -> QueueManager {
        QueueManager::new(queue_path.to_path_buf(), output_dir.to_path_buf())
    }

    fn test_config(prompts_dir: &Path) -> Arc<Config> {
        Arc::new(Config {
            cli_command: "claude".to_string(),
            cli_env: std::collections::HashMap::new(),
            max_concurrent_topics: 1,
            max_concurrent_agents: 10,
            model_concurrency: std::collections::HashMap::new(),
            agent_timeout: 600,
            model: "sonnet".to_string(),
            max_turns: crate::config::DEFAULT_MAX_TURNS,
            output_dir: String::new(),
            queue_file: String::new(),
            prompts_dir: prompts_dir.to_string_lossy().to_string(),
            max_cost_per_topic: 0.0,
            agents: std::collections::HashMap::new(),
        })
    }

    async fn run_pipeline(
        queue_path: &Path, output_dir: &Path, prompts_dir: &Path,
        runner: Arc<dyn AgentRunner>,
    ) -> (Vec<(String, bool)>, QueueManager) {
        let qm = make_qm(queue_path, output_dir);
        let topics = qm.get_pending_topics().unwrap();
        let pool = WorkerPool::new(output_dir.to_path_buf(), test_config(prompts_dir));
        let results = pool.process_all_with_runner(&topics, qm, runner).await;
        let qm_after = make_qm(queue_path, output_dir);
        (results, qm_after)
    }

    // --- Verify frontmatter parsing ---

    #[test]
    fn extract_yaml_frontmatter_basic() {
        let content = "---\noverall: pass\n---\n\n# Body\n";
        assert_eq!(extract_yaml_frontmatter(content), Some("overall: pass"));
    }

    #[test]
    fn extract_yaml_frontmatter_multiline() {
        let content = "---\noverall: fail\nfailed_checks:\n  - a\n  - b\n---\n\nbody";
        assert_eq!(
            extract_yaml_frontmatter(content),
            Some("overall: fail\nfailed_checks:\n  - a\n  - b")
        );
    }

    #[test]
    fn extract_yaml_frontmatter_missing_opening() {
        let content = "# Just a markdown file\nNo frontmatter here.";
        assert_eq!(extract_yaml_frontmatter(content), None);
    }

    #[test]
    fn extract_yaml_frontmatter_missing_closing() {
        let content = "---\noverall: pass\nno closing marker";
        assert_eq!(extract_yaml_frontmatter(content), None);
    }

    #[test]
    fn parse_verify_report_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verify.md");
        std::fs::write(&path, fake_verify_passing()).unwrap();
        match parse_verify_report(&path) {
            PipelineOutcome::Done => {}
            PipelineOutcome::NeedsReview(r) => panic!("expected Done, got NeedsReview({r})"),
        }
    }

    #[test]
    fn parse_verify_report_fail_lists_failed_checks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verify.md");
        std::fs::write(&path, fake_verify_failing()).unwrap();
        match parse_verify_report(&path) {
            PipelineOutcome::NeedsReview(r) => {
                assert!(r.contains("fix_application"), "reason should include failed check: {r}");
                assert!(r.contains("origin_tags"), "reason should include failed check: {r}");
            }
            PipelineOutcome::Done => panic!("expected NeedsReview, got Done"),
        }
    }

    #[test]
    fn parse_verify_report_missing_frontmatter_is_needs_review() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verify.md");
        std::fs::write(&path, fake_verify_no_frontmatter()).unwrap();
        match parse_verify_report(&path) {
            PipelineOutcome::NeedsReview(r) => {
                assert!(r.contains("frontmatter"), "reason should mention frontmatter: {r}");
            }
            PipelineOutcome::Done => panic!("missing frontmatter must fail to NeedsReview"),
        }
    }

    #[test]
    fn parse_verify_report_malformed_yaml_is_needs_review() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("verify.md");
        std::fs::write(
            &path,
            "---\n{{{{not valid yaml at all\n---\n\nbody",
        ).unwrap();
        match parse_verify_report(&path) {
            PipelineOutcome::NeedsReview(r) => {
                assert!(r.contains("malformed") || r.contains("frontmatter"),
                        "reason should flag malformed: {r}");
            }
            PipelineOutcome::Done => panic!("malformed yaml must fail to NeedsReview"),
        }
    }

    #[test]
    fn parse_verify_report_missing_file_is_needs_review() {
        match parse_verify_report(Path::new("/tmp/definitely-not-a-real-verify-file.md")) {
            PipelineOutcome::NeedsReview(_) => {}
            PipelineOutcome::Done => panic!("missing file must fail to NeedsReview"),
        }
    }

    // --- Pipeline outcome tests ---

    #[tokio::test]
    async fn pipeline_marks_needs_review_when_verify_fails() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner: Arc<dyn AgentRunner> = Arc::new(
            FakeRunnerWithVerifyContent::new(fake_verify_failing()),
        );

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();
        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner).await;

        assert!(results[0].1, "pipeline should return true (clean terminal state)");

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, TopicStatus::NeedsReview);
        assert!(meta.completed_at.is_some());
        let err = meta.error.as_ref().expect("needs_review should set error field");
        assert!(err.contains("fix_application"), "error should name failed check: {err}");

        // Topic should be out of the queue (mark_needs_review removes it)
        assert!(qm.get_pending_topics().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pipeline_marks_needs_review_when_verify_frontmatter_missing() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner: Arc<dyn AgentRunner> = Arc::new(
            FakeRunnerWithVerifyContent::new(fake_verify_no_frontmatter()),
        );

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();
        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner).await;

        assert!(results[0].1, "pipeline should return true (clean terminal state)");

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(
            meta.status,
            TopicStatus::NeedsReview,
            "missing frontmatter must become NeedsReview, not Done or Failed"
        );
    }

    #[tokio::test]
    async fn needs_review_topic_is_picked_up_by_recover() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner: Arc<dyn AgentRunner> = Arc::new(
            FakeRunnerWithVerifyContent::new(fake_verify_failing()),
        );

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();
        run_pipeline(&queue_path, &output_dir, &prompts_dir, runner).await;

        // Topic is in NeedsReview state and out of the queue.
        let mut qm = make_qm(&queue_path, &output_dir);
        assert!(qm.get_pending_topics().unwrap().is_empty());
        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, TopicStatus::NeedsReview);

        // recover_failed should pick it up and reset to Pending.
        let recovered = qm.recover_failed().unwrap();
        assert_eq!(recovered, vec!["test-topic".to_string()]);
        assert_eq!(qm.get_pending_topics().unwrap().len(), 1);
        let meta_after = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta_after.status, TopicStatus::Pending);
    }

    #[tokio::test]
    async fn partial_write_without_sidecar_is_rerun_not_cached() {
        // Simulate a partial write: content present, no sidecar. The pipeline must
        // re-run the research agent rather than treating the fragment as cached.
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner = Arc::new(FakeRunner::succeeding());

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();

        let research_dir = output_dir.join("test-topic/research");
        std::fs::create_dir_all(&research_dir).unwrap();
        // Write a "partial" academic.md: content, no sidecar.
        std::fs::write(
            research_dir.join("academic.md"),
            "# Partial findings — agent timed out mid-write",
        ).unwrap();

        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner.clone()).await;

        assert!(results[0].1, "pipeline should succeed");
        // All 11 agents run because no output has a sidecar.
        assert_eq!(runner.invocations(), 11);

        // The partial file was overwritten by the fresh run; now it has a sidecar too.
        assert!(sidecar_path(&research_dir.join("academic.md")).exists());

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(
            meta.agents["research_academic"].status,
            AgentStatus::Done,
            "partial-write file should trigger a fresh run, not count as cached"
        );
    }

    #[tokio::test]
    async fn full_pipeline_succeeds_with_fake_runner() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner = Arc::new(FakeRunner::succeeding());

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic for pipeline").unwrap();

        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner.clone()).await;

        assert_eq!(results.len(), 1);
        assert!(results[0].1, "pipeline should succeed");
        // 3 research + 1 synthesis + 4 validation + 1 triage + 1 revision + 1 verify = 11
        assert_eq!(runner.invocations(), 11);
        assert!(qm.get_pending_topics().unwrap().is_empty());

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, TopicStatus::Done);
        assert!(meta.completed_at.is_some());

        assert!(output_dir.join("test-topic/research/academic.md").exists());
        assert!(output_dir.join("test-topic/research/expert.md").exists());
        assert!(output_dir.join("test-topic/research/general.md").exists());
        assert!(output_dir.join("test-topic/overview.md").exists());
        assert!(output_dir.join("test-topic/validation/bias.md").exists());
        assert!(output_dir.join("test-topic/triage.md").exists());
        assert!(output_dir.join("test-topic/overview_final.md").exists());
        assert!(output_dir.join("test-topic/verify.md").exists());
    }

    #[tokio::test]
    async fn pipeline_skips_existing_research_outputs() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner = Arc::new(FakeRunner::succeeding());

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();

        let research_dir = output_dir.join("test-topic/research");
        std::fs::create_dir_all(&research_dir).unwrap();
        write_cached(&research_dir.join("academic.md"), "# Cached academic findings");
        write_cached(&research_dir.join("expert.md"), "# Cached expert findings");

        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner.clone()).await;

        assert!(results[0].1, "pipeline should succeed");
        // 1 research (2 cached) + 1 synthesis + 4 validation + 1 triage + 1 revision + 1 verify = 9
        assert_eq!(runner.invocations(), 9);

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.agents["research_academic"].status, AgentStatus::DoneCached);
        assert_eq!(meta.agents["research_expert"].status, AgentStatus::DoneCached);
        assert_eq!(meta.agents["research_general"].status, AgentStatus::Done);
    }

    #[tokio::test]
    async fn pipeline_skips_all_phases_when_fully_cached() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner = Arc::new(FakeRunner::succeeding());

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();

        let topic_dir = output_dir.join("test-topic");
        let research_dir = topic_dir.join("research");
        let validation_dir = topic_dir.join("validation");
        std::fs::create_dir_all(&research_dir).unwrap();
        std::fs::create_dir_all(&validation_dir).unwrap();
        std::fs::create_dir_all(topic_dir.join("sources")).unwrap();

        for f in &["academic.md", "expert.md", "general.md"] {
            write_cached(&research_dir.join(f), "cached");
        }
        write_cached(&topic_dir.join("overview.md"), "cached synthesis");
        for f in &["bias.md", "sources.md", "claims.md", "completeness.md"] {
            write_cached(&validation_dir.join(f), "cached");
        }
        write_cached(&topic_dir.join("triage.md"), "cached triage");
        write_cached(&topic_dir.join("overview_final.md"), "cached");
        // Cached verify.md must have a valid passing frontmatter so the parser reads
        // it as a pass and the topic is marked Done, not NeedsReview.
        write_cached(&topic_dir.join("verify.md"), &fake_verify_passing());

        let (results, _qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner.clone()).await;

        assert!(results[0].1);
        assert_eq!(runner.invocations(), 0);
    }

    #[tokio::test]
    async fn pipeline_fails_when_all_research_agents_fail_and_no_cache() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner = Arc::new(FakeRunner::failing());

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();

        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner.clone()).await;

        assert!(!results[0].1, "pipeline should fail");
        assert_eq!(runner.invocations(), 3);

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, TopicStatus::Failed);
        assert!(meta.error.as_ref().unwrap().contains("Research phase failed"));
    }

    #[tokio::test]
    async fn pipeline_succeeds_when_research_fails_but_cache_exists() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner = Arc::new(FakeRunner::failing());

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();

        let research_dir = output_dir.join("test-topic/research");
        write_cached(&research_dir.join("academic.md"), "# Cached findings");

        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner.clone()).await;

        assert!(!results[0].1);
        assert_eq!(runner.invocations(), 3);

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, TopicStatus::Failed);
        assert!(meta.error.as_ref().unwrap().contains("Synthesis phase failed"));
    }

    #[tokio::test]
    async fn pipeline_processes_multiple_topics() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner = Arc::new(FakeRunner::succeeding());

        {
            let mut qm = make_qm(&queue_path, &output_dir);
            qm.add_topic("topic-a", "Topic A").unwrap();
            qm.add_topic("topic-b", "Topic B").unwrap();
        }

        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner.clone()).await;

        assert_eq!(results.len(), 2);
        assert!(results[0].1);
        assert!(results[1].1);
        // 2 topics * 11 agents each
        assert_eq!(runner.invocations(), 22);
        assert!(qm.get_pending_topics().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resume_after_synthesis_failure_skips_completed_research() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();

        // first run: succeed everything, then manually fail the topic
        // to simulate a partial run where research succeeded but synthesis didn't
        {
            let runner: Arc<dyn AgentRunner> = Arc::new(FakeRunner::succeeding());
            let qm = make_qm(&queue_path, &output_dir);
            let topics = qm.get_pending_topics().unwrap();
            let topic = &topics[0];
            let qm_arc = Arc::new(Mutex::new(qm));

            let pipeline = TopicPipeline::new(
                topic.clone(), &output_dir,
                test_config(&prompts_dir), runner.clone(), qm_arc.clone(),
            );
            pipeline.setup_directories().unwrap();
            qm_arc.lock().await.claim_topic(topic).unwrap();
            let research_ok = pipeline.run_research_phase().await.unwrap();
            assert!(research_ok);

            qm_arc.lock().await.fail_topic(topic, "Synthesis phase failed").unwrap();
        }

        make_qm(&queue_path, &output_dir).recover_failed().unwrap();

        // second run: all research should be cached
        let runner = Arc::new(FakeRunner::succeeding());
        let (results, _qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner.clone()).await;
        assert!(results[0].1);
        // 0 research (all cached) + 1 synthesis + 4 validation + 1 triage + 1 revision + 1 verify = 8
        assert_eq!(runner.invocations(), 8);
    }

    // --- Selective failure tests ---

    #[tokio::test]
    async fn all_validators_fail_marks_topic_failed() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner: Arc<dyn AgentRunner> = Arc::new(SelectiveFakeRunner::failing_agents(&[
            "validate_bias", "validate_sources", "validate_claims", "validate_completeness",
        ]));

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();
        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner).await;

        assert!(!results[0].1, "pipeline should fail when all validators fail");

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, TopicStatus::Failed);
        assert!(meta.error.as_ref().unwrap().contains("Validation phase failed"));
    }

    #[tokio::test]
    async fn revision_failure_marks_topic_failed() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner: Arc<dyn AgentRunner> = Arc::new(SelectiveFakeRunner::failing_agents(&["revision"]));

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();
        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner).await;

        assert!(!results[0].1, "pipeline should fail when revision fails");

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, TopicStatus::Failed);
        assert!(meta.error.as_ref().unwrap().contains("Revision phase failed"));
    }

    #[tokio::test]
    async fn partial_validator_failure_still_completes() {
        // 2 of 4 validators succeed — exactly at the new threshold. Pipeline proceeds.
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner: Arc<dyn AgentRunner> = Arc::new(SelectiveFakeRunner::failing_agents(&["validate_bias", "validate_sources"]));

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();
        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner).await;

        assert!(results[0].1, "pipeline should succeed with 2-of-4 validators passing");

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, TopicStatus::Done);
        assert_eq!(meta.agents["validate_bias"].status, AgentStatus::Failed);
        assert_eq!(meta.agents["validate_claims"].status, AgentStatus::Done);
    }

    #[tokio::test]
    async fn only_one_validator_passing_fails_pipeline() {
        // 1 of 4 validators succeeds — below the threshold. Pipeline bails at validation.
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner: Arc<dyn AgentRunner> = Arc::new(SelectiveFakeRunner::failing_agents(&[
            "validate_bias", "validate_sources", "validate_claims",
        ]));

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();
        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner).await;

        assert!(!results[0].1, "pipeline should fail with 1-of-4 validators passing");

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, TopicStatus::Failed);
        let err = meta.error.as_ref().expect("should record error");
        assert!(err.contains("Validation phase failed"), "error should cite validation: {err}");
        assert!(err.contains("1/4"), "error should report pass count: {err}");
    }

    #[tokio::test]
    async fn synthesis_failure_stops_pipeline_before_validation() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner: Arc<dyn AgentRunner> = Arc::new(SelectiveFakeRunner::failing_agents(&["synthesizer"]));

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();
        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner).await;

        assert!(!results[0].1);

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, TopicStatus::Failed);
        assert!(!meta.agents.contains_key("validate_bias"));
        assert_eq!(meta.agents["research_academic"].status, AgentStatus::Done);
    }
}
