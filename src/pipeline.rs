use anyhow::Result;
use futures::future::join_all;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::agent::{self, AgentConfig, AgentRunner, ThrottledRunner};
use crate::config::Config;
use crate::progress;
use crate::queue::{QueueManager, Topic};
use crate::roster;

/// Non-empty file means a previous run already produced this output.
pub(crate) fn agent_output_exists(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.len() > 0,
        Err(_) => false,
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
        if let Err(e) = self.run_inner().await {
            tracing::error!(topic = %self.topic.id, error = %e, "Pipeline failed");
            progress::topic_failed(&self.topic.id, &e.to_string());
            if let Err(write_err) = self.queue_manager.lock().await.fail_topic(&self.topic, &e.to_string()) {
                tracing::error!(topic = %self.topic.id, error = %write_err, "Failed to record topic failure in queue");
            }
            false
        } else {
            let cost = self.total_cost().await;
            progress::topic_done(&self.topic.id, cost);
            true
        }
    }

    async fn total_cost(&self) -> f64 {
        self.queue_manager.lock().await.read_meta(&self.topic)
            .map(|m| m.agents.values().filter_map(|a| a.cost_usd).sum())
            .unwrap_or(0.0)
    }

    async fn run_inner(&self) -> Result<()> {
        self.setup_directories()?;
        self.queue_manager.lock().await.claim_topic(&self.topic)?;

        progress::phase(&self.topic.id, "Researching...");
        let research_ok = self.run_research_phase().await?;
        if !research_ok {
            anyhow::bail!("Research phase failed: no agents succeeded and no prior results to use");
        }
        self.check_cost_limit().await?;

        progress::phase(&self.topic.id, "Synthesizing...");
        self.queue_manager.lock().await.update_status(&self.topic, "synthesizing")?;
        let synthesis_ok = self.run_synthesis_phase().await?;
        if !synthesis_ok {
            anyhow::bail!("Synthesis phase failed");
        }
        self.check_cost_limit().await?;

        progress::phase(&self.topic.id, "Validating...");
        self.queue_manager.lock().await.update_status(&self.topic, "validating")?;
        let (validators_passed, validators_total) = self.run_validation_phase().await?;
        if validators_passed == 0 && validators_total > 0 {
            anyhow::bail!(
                "Validation phase failed (0/{} validators succeeded)",
                validators_total
            );
        }
        if validators_passed < validators_total {
            tracing::warn!(
                topic = %self.topic.id,
                passed = validators_passed,
                total = validators_total,
                "Some validators failed, proceeding to revision with partial validation"
            );
        }

        self.check_cost_limit().await?;

        progress::phase(&self.topic.id, "Revising...");
        self.queue_manager.lock().await.update_status(&self.topic, "revising")?;
        self.run_revision_phase().await?;

        self.queue_manager.lock().await.complete_topic(&self.topic)?;
        Ok(())
    }

    fn setup_directories(&self) -> Result<()> {
        std::fs::create_dir_all(&self.research_dir)?;
        std::fs::create_dir_all(&self.topic_dir.join("sources"))?;
        std::fs::create_dir_all(&self.validation_dir)?;
        std::fs::create_dir_all(&self.responses_dir())?;
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
        let mut agent_futures = Vec::new();

        for def in roster::RESEARCH_AGENTS {
            let output_path = self.research_dir.join(def.output_file);
            let name = def.name;

            if agent_output_exists(&output_path) {
                progress::agent_cached(name);
                self.queue_manager.lock().await.record_agent_result(
                    &self.topic, name, "done (cached)", 0.0, None, None,
                )?;
                already_done += 1;
                continue;
            }

            let prompt_template = agent::load_prompt(&self.prompts_dir(), def.prompt_file)?;
            let prompt = prompt_template.replace("{topic}", &self.topic.input);
            let config = AgentConfig::research(
                name, &self.config.cli_command, &self.config.cli_env, prompt, output_path,
                self.config.model_for(name),
                self.config.max_turns_for(name),
                self.config.timeout_for(name),
            );
            agent_names.push(name);
            agent_futures.push(self.runner.run_agent(config));
        }

        if !agent_names.is_empty() {
            progress::agents_starting(&agent_names);
        }
        let heartbeat = progress::start_heartbeat(30);
        let results = join_all(agent_futures).await;
        heartbeat.stop();

        let mut newly_succeeded = 0;
        for (name, result) in agent_names.iter().zip(results) {
            let mut qm = self.queue_manager.lock().await;
            match result {
                Ok(result) => {
                    progress::agent_done(name, &result);
                    self.save_raw_response(name, &result);
                    let status = if result.success { "done" } else { "failed" };
                    qm.record_agent_result(
                        &self.topic, name, status, result.duration_seconds,
                        result.error.as_deref(), result.usage.as_ref(),
                    )?;
                    if result.success { newly_succeeded += 1; }
                }
                Err(e) => {
                    progress::agent_error(name, &e.to_string());
                    qm.record_agent_result(
                        &self.topic, name, "failed", 0.0, Some(&e.to_string()), None,
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
            self.queue_manager.lock().await.record_agent_result(&self.topic, "synthesis", "done (cached)", 0.0, None, None)?;
            return Ok(true);
        }

        let prompt_template = agent::load_prompt(&self.prompts_dir(), roster::SYNTHESIS_PROMPT)?;
        let prompt = prompt_template
            .replace("{topic}", &self.topic.input)
            .replace("{research_dir}", &self.research_dir.to_string_lossy());

        let config = AgentConfig::synthesis(
            &self.config.cli_command, &self.config.cli_env, prompt, output_path,
            self.config.model_for("synthesizer"),
            self.config.max_turns_for("synthesizer"),
            self.config.timeout_for("synthesizer"),
        );

        progress::agents_starting(&["synthesizer"]);
        let heartbeat = progress::start_heartbeat(30);
        let result = self.runner.run_agent(config).await?;
        heartbeat.stop();

        progress::agent_done("synthesizer", &result);
        self.save_raw_response("synthesis", &result);

        let status = if result.success { "done" } else { "failed" };
        self.queue_manager.lock().await.record_agent_result(
            &self.topic, "synthesis", status, result.duration_seconds,
            result.error.as_deref(), result.usage.as_ref(),
        )?;

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
        let mut agent_futures = Vec::new();

        for def in roster::VALIDATION_AGENTS {
            let output_path = self.validation_dir.join(def.output_file);
            let name = def.name;

            if agent_output_exists(&output_path) {
                progress::agent_cached(name);
                self.queue_manager.lock().await.record_agent_result(
                    &self.topic, name, "done (cached)", 0.0, None, None,
                )?;
                succeeded += 1;
                continue;
            }

            let prompt_template = agent::load_prompt(&self.prompts_dir(), def.prompt_file)?;
            let prompt = prompt_template
                .replace("{topic}", &self.topic.input)
                .replace("{synthesis_path}", &synthesis_str)
                .replace("{research_dir}", &research_str)
                .replace("{validation_dir}", &validation_str);

            let config = AgentConfig::validator(
                name, &self.config.cli_command, &self.config.cli_env, prompt, output_path,
                self.config.model_for(name),
                self.config.max_turns_for(name),
                self.config.timeout_for(name),
                def.needs_web,
            );
            agent_names.push(name);
            agent_futures.push(self.runner.run_agent(config));
        }

        if !agent_names.is_empty() {
            progress::agents_starting(&agent_names);
        }
        let heartbeat = progress::start_heartbeat(30);
        let results = join_all(agent_futures).await;
        heartbeat.stop();

        for (name, result) in agent_names.iter().zip(results) {
            let mut qm = self.queue_manager.lock().await;
            match result {
                Ok(result) => {
                    progress::agent_done(name, &result);
                    self.save_raw_response(name, &result);
                    let status = if result.success { "done" } else { "failed" };
                    qm.record_agent_result(
                        &self.topic, name, status, result.duration_seconds,
                        result.error.as_deref(), result.usage.as_ref(),
                    )?;
                    if result.success { succeeded += 1; }
                }
                Err(e) => {
                    progress::agent_error(name, &e.to_string());
                    qm.record_agent_result(
                        &self.topic, name, "failed", 0.0, Some(&e.to_string()), None,
                    )?;
                }
            }
        }

        Ok((succeeded, total))
    }

    async fn run_revision_phase(&self) -> Result<()> {
        let output_path = self.topic_dir.join("overview_final.md");

        if agent_output_exists(&output_path) {
            progress::agent_cached("revision");
            self.queue_manager.lock().await.record_agent_result(&self.topic, "revision", "done (cached)", 0.0, None, None)?;
            return Ok(());
        }

        let prompt_template = agent::load_prompt(&self.prompts_dir(), roster::REVISION_PROMPT)?;
        let prompt = prompt_template
            .replace("{topic}", &self.topic.input)
            .replace("{synthesis_path}", &self.topic_dir.join("overview.md").to_string_lossy())
            .replace("{validation_dir}", &self.validation_dir.to_string_lossy());

        let config = AgentConfig::revision(
            &self.config.cli_command, &self.config.cli_env, prompt, output_path,
            self.config.model_for("revision"),
            self.config.max_turns_for("revision"),
            self.config.timeout_for("revision"),
        );

        progress::agents_starting(&["revision"]);
        let heartbeat = progress::start_heartbeat(30);
        let result = self.runner.run_agent(config).await?;
        heartbeat.stop();

        progress::agent_done("revision", &result);
        self.save_raw_response("revision", &result);

        let status = if result.success { "done" } else { "failed" };
        self.queue_manager.lock().await.record_agent_result(
            &self.topic, "revision", status, result.duration_seconds,
            result.error.as_deref(), result.usage.as_ref(),
        )?;

        if !result.success {
            anyhow::bail!("Revision phase failed");
        }

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
    use crate::queue::{QueueManager, Topic};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // --- agent_output_exists ---

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
    fn agent_output_exists_true_for_file_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("research.md");
        std::fs::write(&path, "# Findings\nSome content here").unwrap();
        assert!(agent_output_exists(&path));
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
                    std::fs::write(&config.output_path, format!("# {} output\nFake content for testing", config.name))?;
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
                    std::fs::write(&config.output_path, format!("# {} output\nFake content", config.name))?;
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

    #[tokio::test]
    async fn full_pipeline_succeeds_with_fake_runner() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner = Arc::new(FakeRunner::succeeding());

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic for pipeline").unwrap();

        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner.clone()).await;

        assert_eq!(results.len(), 1);
        assert!(results[0].1, "pipeline should succeed");
        assert_eq!(runner.invocations(), 9);
        assert!(qm.get_pending_topics().unwrap().is_empty());

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, "done");
        assert!(meta.completed_at.is_some());

        assert!(output_dir.join("test-topic/research/academic.md").exists());
        assert!(output_dir.join("test-topic/research/expert.md").exists());
        assert!(output_dir.join("test-topic/research/general.md").exists());
        assert!(output_dir.join("test-topic/overview.md").exists());
        assert!(output_dir.join("test-topic/validation/bias.md").exists());
        assert!(output_dir.join("test-topic/overview_final.md").exists());
    }

    #[tokio::test]
    async fn pipeline_skips_existing_research_outputs() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner = Arc::new(FakeRunner::succeeding());

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();

        let research_dir = output_dir.join("test-topic/research");
        std::fs::create_dir_all(&research_dir).unwrap();
        std::fs::write(research_dir.join("academic.md"), "# Cached academic findings").unwrap();
        std::fs::write(research_dir.join("expert.md"), "# Cached expert findings").unwrap();

        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner.clone()).await;

        assert!(results[0].1, "pipeline should succeed");
        assert_eq!(runner.invocations(), 7);

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.agents["research_academic"].status, "done (cached)");
        assert_eq!(meta.agents["research_expert"].status, "done (cached)");
        assert_eq!(meta.agents["research_general"].status, "done");
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
            std::fs::write(research_dir.join(f), "cached").unwrap();
        }
        std::fs::write(topic_dir.join("overview.md"), "cached synthesis").unwrap();
        for f in &["bias.md", "sources.md", "claims.md", "completeness.md"] {
            std::fs::write(validation_dir.join(f), "cached").unwrap();
        }
        std::fs::write(topic_dir.join("overview_final.md"), "cached").unwrap();

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
        assert_eq!(meta.status, "failed");
        assert!(meta.error.as_ref().unwrap().contains("Research phase failed"));
    }

    #[tokio::test]
    async fn pipeline_succeeds_when_research_fails_but_cache_exists() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner = Arc::new(FakeRunner::failing());

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();

        let research_dir = output_dir.join("test-topic/research");
        std::fs::create_dir_all(&research_dir).unwrap();
        std::fs::write(research_dir.join("academic.md"), "# Cached findings").unwrap();

        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner.clone()).await;

        assert!(!results[0].1);
        assert_eq!(runner.invocations(), 3);

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, "failed");
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
        assert_eq!(runner.invocations(), 18);
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
        // 0 research (all cached) + 1 synthesis + 4 validation + 1 revision = 6
        assert_eq!(runner.invocations(), 6);
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
        assert_eq!(meta.status, "failed");
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
        assert_eq!(meta.status, "failed");
        assert!(meta.error.as_ref().unwrap().contains("Revision phase failed"));
    }

    #[tokio::test]
    async fn partial_validator_failure_still_completes() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner: Arc<dyn AgentRunner> = Arc::new(SelectiveFakeRunner::failing_agents(&["validate_bias", "validate_sources"]));

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();
        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner).await;

        assert!(results[0].1, "pipeline should succeed with partial validation");

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, "done");
        assert_eq!(meta.agents["validate_bias"].status, "failed");
        assert_eq!(meta.agents["validate_claims"].status, "done");
    }

    #[tokio::test]
    async fn synthesis_failure_stops_pipeline_before_validation() {
        let (_dir, queue_path, output_dir, prompts_dir) = setup_pipeline_test();
        let runner: Arc<dyn AgentRunner> = Arc::new(SelectiveFakeRunner::failing_agents(&["synthesizer"]));

        make_qm(&queue_path, &output_dir).add_topic("test-topic", "Test topic").unwrap();
        let (results, qm) = run_pipeline(&queue_path, &output_dir, &prompts_dir, runner).await;

        assert!(!results[0].1);

        let meta = qm.read_meta(&make_topic("test-topic", "")).unwrap();
        assert_eq!(meta.status, "failed");
        assert!(!meta.agents.contains_key("validate_bias"));
        assert_eq!(meta.agents["research_academic"].status, "done");
    }
}
