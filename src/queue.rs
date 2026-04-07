use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

/// Write content to a file atomically: write to .tmp sibling, then rename.
/// Prevents corrupt files if the process crashes mid-write.
pub(crate) fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let mut tmp_name = path.file_name()
        .unwrap_or_default()
        .to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = path.with_file_name(tmp_name);
    std::fs::write(&tmp_path, contents)
        .with_context(|| format!("Failed to write tmp file: {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("Failed to rename {} -> {}", tmp_path.display(), path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topic {
    pub id: String,
    pub input: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct QueueFile {
    #[serde(default)]
    pub topics: Vec<Topic>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TopicMeta {
    pub id: String,
    pub input: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub agents: std::collections::BTreeMap<String, AgentMeta>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AgentMeta {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

pub struct QueueManager {
    queue_path: PathBuf,
    output_dir: PathBuf,
}

impl QueueManager {
    pub fn new(queue_path: PathBuf, output_dir: PathBuf) -> Self {
        Self { queue_path, output_dir }
    }

    pub fn get_pending_topics(&self) -> Result<Vec<Topic>> {
        self.with_queue_lock(|queue| Ok(queue.topics.clone()))
    }

    pub fn add_topic(&mut self, id: &str, input: &str) -> Result<()> {
        self.with_queue_lock_mut(|queue| {
            if queue.topics.iter().any(|t| t.id == id) {
                bail!("Topic '{}' already exists in queue", id);
            }

            queue.topics.push(Topic {
                id: id.to_string(),
                input: input.to_string(),
            });

            Ok(())
        })
    }

    pub fn remove_topic(&mut self, id: &str) -> Result<()> {
        self.with_queue_lock_mut(|queue| {
            let original_len = queue.topics.len();
            queue.topics.retain(|t| t.id != id);

            if queue.topics.len() == original_len {
                bail!("Topic '{}' not found in queue", id);
            }

            Ok(())
        })
    }

    pub fn claim_topic(&mut self, topic: &Topic) -> Result<()> {
        let meta = TopicMeta {
            id: topic.id.clone(),
            input: topic.input.clone(),
            status: "researching".to_string(),
            started_at: Some(chrono::Utc::now().to_rfc3339()),
            completed_at: None,
            error: None,
            agents: Default::default(),
        };

        self.write_meta(topic, &meta)
    }

    pub fn update_status(&mut self, topic: &Topic, status: &str) -> Result<()> {
        let mut meta = self.read_meta(topic)?;
        meta.status = status.to_string();

        if status == "done" {
            meta.completed_at = Some(chrono::Utc::now().to_rfc3339());
        }

        self.write_meta(topic, &meta)
    }

    pub fn record_agent_result(
        &mut self,
        topic: &Topic,
        agent_name: &str,
        status: &str,
        duration_seconds: f64,
        error: Option<&str>,
        usage: Option<&crate::agent::AgentUsage>,
    ) -> Result<()> {
        let mut meta = self.read_meta(topic)?;

        let (input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, cost_usd) =
            if let Some(u) = usage {
                (Some(u.input_tokens), Some(u.output_tokens),
                 Some(u.cache_creation_tokens), Some(u.cache_read_tokens),
                 Some(u.cost_usd))
            } else {
                (None, None, None, None, None)
            };

        meta.agents.insert(
            agent_name.to_string(),
            AgentMeta {
                status: status.to_string(),
                duration: Some(format!("{duration_seconds:.1}s")),
                error: error.map(String::from),
                input_tokens,
                output_tokens,
                cache_creation_tokens,
                cache_read_tokens,
                cost_usd,
            },
        );

        self.write_meta(topic, &meta)
    }

    pub fn complete_topic(&mut self, topic: &Topic) -> Result<()> {
        self.update_status(topic, "done")?;
        self.with_queue_lock_mut(|queue| {
            queue.topics.retain(|t| t.id != topic.id);
            Ok(())
        })?;
        tracing::info!(topic_id = %topic.id, "Topic completed");
        Ok(())
    }

    pub fn recover_failed(&mut self) -> Result<Vec<String>> {
        let mut recovered = Vec::new();

        if !self.output_dir.exists() {
            return Ok(recovered);
        }

        for entry in std::fs::read_dir(&self.output_dir)? {
            let entry = entry?;
            let meta_path = entry.path().join("meta.yaml");
            if !meta_path.exists() {
                continue;
            }

            let contents = std::fs::read_to_string(&meta_path)?;
            let meta: TopicMeta = serde_yaml::from_str(&contents)?;

            if meta.status == "failed" || meta.status == "researching"
                || meta.status == "synthesizing" || meta.status == "validating"
                || meta.status == "revising"
            {
                // reset meta to allow resume
                let reset_meta = TopicMeta {
                    id: meta.id.clone(),
                    input: meta.input.clone(),
                    status: "pending".to_string(),
                    started_at: None,
                    completed_at: None,
                    error: None,
                    agents: meta.agents,
                };
                let yaml = serde_yaml::to_string(&reset_meta)?;
                atomic_write(&meta_path, &yaml)?;

                // re-add to queue if not already there
                let topic_id = meta.id.clone();
                let topic_input = meta.input.clone();
                self.with_queue_lock_mut(|queue| {
                    if !queue.topics.iter().any(|t| t.id == topic_id) {
                        queue.topics.push(Topic {
                            id: topic_id.clone(),
                            input: topic_input.clone(),
                        });
                    }
                    Ok(())
                })?;

                recovered.push(meta.id);
            }
        }

        Ok(recovered)
    }

    pub fn fail_topic(&mut self, topic: &Topic, error: &str) -> Result<()> {
        let mut meta = self.read_meta(topic)?;
        meta.status = "failed".to_string();
        meta.error = Some(error.to_string());
        self.write_meta(topic, &meta)?;
        // remove from queue so it doesn't auto-retry. use `recover` to re-queue.
        self.with_queue_lock_mut(|queue| {
            queue.topics.retain(|t| t.id != topic.id);
            Ok(())
        })?;
        tracing::error!(topic_id = %topic.id, error, "Topic failed");
        Ok(())
    }

    pub fn is_already_processed(&self, topic: &Topic) -> bool {
        let meta_path = self.meta_path(topic);
        if !meta_path.exists() {
            return false;
        }
        self.read_meta(topic)
            .map(|m| m.status == "done" || m.status == "failed")
            .unwrap_or(false)
    }

    pub(crate) fn meta_path(&self, topic: &Topic) -> PathBuf {
        self.output_dir.join(&topic.id).join("meta.yaml")
    }

    pub(crate) fn read_meta(&self, topic: &Topic) -> Result<TopicMeta> {
        let path = self.meta_path(topic);
        if !path.exists() {
            return Ok(TopicMeta {
                id: topic.id.clone(),
                input: topic.input.clone(),
                status: "unknown".to_string(),
                ..Default::default()
            });
        }
        let contents = std::fs::read_to_string(&path)?;
        Ok(serde_yaml::from_str(&contents)?)
    }

    fn write_meta(&self, topic: &Topic, meta: &TopicMeta) -> Result<()> {
        let path = self.meta_path(topic);
        std::fs::create_dir_all(path.parent().expect("meta path always has a parent directory"))?;
        let contents = serde_yaml::to_string(meta)?;
        atomic_write(&path, &contents)
    }

    fn with_queue_lock<T>(&self, f: impl FnOnce(&QueueFile) -> Result<T>) -> Result<T> {
        if !self.queue_path.exists() {
            return f(&QueueFile::default());
        }

        let file = OpenOptions::new()
            .read(true)
            .open(&self.queue_path)
            .with_context(|| format!("Failed to open queue file: {}", self.queue_path.display()))?;

        file.lock_shared()
            .with_context(|| "Failed to acquire shared lock on queue file")?;

        let contents = std::fs::read_to_string(&self.queue_path)?;
        let queue: QueueFile = serde_yaml::from_str(&contents)
            .with_context(|| format!("Failed to parse queue file: {}", self.queue_path.display()))?;
        let result = f(&queue);

        // unlock errors are non-fatal; the OS reclaims the lock when the file handle drops
        file.unlock().ok();
        result
    }

    fn with_queue_lock_mut<T>(&self, f: impl FnOnce(&mut QueueFile) -> Result<T>) -> Result<T> {
        if !self.queue_path.exists() {
            if let Some(parent) = self.queue_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&self.queue_path, "topics: []\n")?;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.queue_path)
            .with_context(|| format!("Failed to open queue file: {}", self.queue_path.display()))?;

        file.lock_exclusive()
            .with_context(|| "Failed to acquire exclusive lock on queue file")?;

        let contents = std::fs::read_to_string(&self.queue_path)?;
        let mut queue: QueueFile = serde_yaml::from_str(&contents)
            .with_context(|| format!("Failed to parse queue file: {}", self.queue_path.display()))?;

        let result = f(&mut queue);

        // only persist if the closure succeeded (don't write partial state)
        if result.is_ok() {
            let yaml = serde_yaml::to_string(&queue)?;
            atomic_write(&self.queue_path, &yaml)?;
        }

        // unlock errors are non-fatal; the OS reclaims the lock when the file handle drops
        file.unlock().ok();
        result
    }

    pub fn get_all_statuses(&self) -> Result<StatusReport> {
        let pending = self.get_pending_topics()?;

        let mut in_progress = Vec::new();
        let mut done = Vec::new();
        let mut failed = Vec::new();

        if self.output_dir.exists() {
            for entry in std::fs::read_dir(&self.output_dir)? {
                let entry = entry?;
                let meta_path = entry.path().join("meta.yaml");
                if !meta_path.exists() {
                    continue;
                }

                let contents = std::fs::read_to_string(&meta_path)?;
                let meta: TopicMeta = serde_yaml::from_str(&contents)?;

                match meta.status.as_str() {
                    "done" => done.push(meta),
                    "failed" => failed.push(meta),
                    "researching" | "synthesizing" | "validating" | "revising" => {
                        in_progress.push(meta);
                    }
                    _ => {}
                }
            }
        }

        // topics in queue that don't have a meta yet (truly pending)
        let processing_ids: std::collections::HashSet<_> = in_progress.iter()
            .chain(done.iter())
            .chain(failed.iter())
            .map(|m| m.id.clone())
            .collect();

        let truly_pending: Vec<_> = pending.into_iter()
            .filter(|t| !processing_ids.contains(&t.id))
            .collect();

        Ok(StatusReport { pending: truly_pending, in_progress, done, failed })
    }
}

pub struct StatusReport {
    pub pending: Vec<Topic>,
    pub in_progress: Vec<TopicMeta>,
    pub done: Vec<TopicMeta>,
    pub failed: Vec<TopicMeta>,
}

pub fn validate_queue(path: &Path) -> Result<Vec<String>> {
    let mut issues = Vec::new();

    if !path.exists() {
        issues.push("Queue file does not exist".to_string());
        return Ok(issues);
    }

    let contents = std::fs::read_to_string(path)?;
    let queue: QueueFile = match serde_yaml::from_str(&contents) {
        Ok(q) => q,
        Err(e) => {
            issues.push(format!("Queue file parse error: {e}"));
            return Ok(issues);
        }
    };

    // check for duplicate IDs
    let mut seen_ids = std::collections::HashSet::new();
    for topic in &queue.topics {
        if topic.id.is_empty() {
            issues.push("Topic found with empty id".to_string());
        }
        if topic.input.trim().is_empty() {
            issues.push(format!("Topic '{}' has empty input", topic.id));
        }
        if !seen_ids.insert(&topic.id) {
            issues.push(format!("Duplicate topic id: '{}'", topic.id));
        }
    }

    Ok(issues)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_topic(id: &str, input: &str) -> Topic {
        Topic { id: id.to_string(), input: input.to_string() }
    }

    fn setup() -> (tempfile::TempDir, QueueManager) {
        let dir = tempfile::tempdir().unwrap();
        let queue_path = dir.path().join("queue.yaml");
        let output_dir = dir.path().join("output");
        std::fs::create_dir_all(&output_dir).unwrap();
        // write an empty but valid queue file
        std::fs::write(&queue_path, "topics: []\n").unwrap();
        let qm = QueueManager::new(queue_path, output_dir);
        (dir, qm)
    }

    // --- get_pending_topics ---

    #[test]
    fn empty_queue_returns_no_topics() {
        let (_dir, qm) = setup();
        let topics = qm.get_pending_topics().unwrap();
        assert!(topics.is_empty());
    }

    #[test]
    fn missing_queue_file_returns_no_topics() {
        let dir = tempfile::tempdir().unwrap();
        let qm = QueueManager::new(dir.path().join("nope.yaml"), dir.path().join("output"));
        let topics = qm.get_pending_topics().unwrap();
        assert!(topics.is_empty());
    }

    // --- add_topic ---

    #[test]
    fn add_topic_appears_in_pending() {
        let (_dir, mut qm) = setup();

        qm.add_topic("fasting", "Intermittent fasting").unwrap();

        let topics = qm.get_pending_topics().unwrap();
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].id, "fasting");
        assert_eq!(topics[0].input, "Intermittent fasting");
    }

    #[test]
    fn add_multiple_topics_preserves_order() {
        let (_dir, mut qm) = setup();

        qm.add_topic("alpha", "Topic A").unwrap();
        qm.add_topic("beta", "Topic B").unwrap();
        qm.add_topic("gamma", "Topic C").unwrap();

        let topics = qm.get_pending_topics().unwrap();
        assert_eq!(topics.len(), 3);
        assert_eq!(topics[0].id, "alpha");
        assert_eq!(topics[1].id, "beta");
        assert_eq!(topics[2].id, "gamma");
    }

    #[test]
    fn add_duplicate_id_returns_error() {
        let (_dir, mut qm) = setup();

        qm.add_topic("fasting", "First version").unwrap();
        let result = qm.add_topic("fasting", "Second version");

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    // --- remove_topic ---

    #[test]
    fn remove_topic_disappears_from_pending() {
        let (_dir, mut qm) = setup();

        qm.add_topic("a", "Topic A").unwrap();
        qm.add_topic("b", "Topic B").unwrap();
        qm.remove_topic("a").unwrap();

        let topics = qm.get_pending_topics().unwrap();
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].id, "b");
    }

    #[test]
    fn remove_nonexistent_topic_returns_error() {
        let (_dir, mut qm) = setup();
        let result = qm.remove_topic("ghost");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    // --- claim_topic ---

    #[test]
    fn claim_creates_meta_yaml_with_researching_status() {
        let (dir, mut qm) = setup();
        let topic = make_topic("fasting", "Intermittent fasting");

        qm.claim_topic(&topic).unwrap();

        let meta_path = dir.path().join("output/fasting/meta.yaml");
        assert!(meta_path.exists());

        let contents = std::fs::read_to_string(&meta_path).unwrap();
        let meta: TopicMeta = serde_yaml::from_str(&contents).unwrap();
        assert_eq!(meta.id, "fasting");
        assert_eq!(meta.status, "researching");
        assert!(meta.started_at.is_some());
        assert!(meta.completed_at.is_none());
    }

    // --- update_status ---

    #[test]
    fn update_status_changes_meta() {
        let (dir, mut qm) = setup();
        let topic = make_topic("fasting", "Intermittent fasting");

        qm.claim_topic(&topic).unwrap();
        qm.update_status(&topic, "synthesizing").unwrap();

        let meta_path = dir.path().join("output/fasting/meta.yaml");
        let contents = std::fs::read_to_string(&meta_path).unwrap();
        let meta: TopicMeta = serde_yaml::from_str(&contents).unwrap();
        assert_eq!(meta.status, "synthesizing");
    }

    #[test]
    fn update_status_to_done_sets_completed_at() {
        let (_dir, mut qm) = setup();
        let topic = make_topic("fasting", "Intermittent fasting");

        qm.claim_topic(&topic).unwrap();
        qm.update_status(&topic, "done").unwrap();

        let meta = qm.read_meta(&topic).unwrap();
        assert_eq!(meta.status, "done");
        assert!(meta.completed_at.is_some());
    }

    // --- record_agent_result ---

    #[test]
    fn record_agent_result_appends_to_meta() {
        let (_dir, mut qm) = setup();
        let topic = make_topic("fasting", "Intermittent fasting");

        qm.claim_topic(&topic).unwrap();
        qm.record_agent_result(&topic, "research_academic", "done", 123.4, None, None).unwrap();
        qm.record_agent_result(&topic, "research_expert", "failed", 45.0, Some("timed out"), None).unwrap();

        let meta = qm.read_meta(&topic).unwrap();
        assert_eq!(meta.agents.len(), 2);

        let academic = &meta.agents["research_academic"];
        assert_eq!(academic.status, "done");
        assert_eq!(academic.duration.as_deref(), Some("123.4s"));
        assert!(academic.error.is_none());

        let expert = &meta.agents["research_expert"];
        assert_eq!(expert.status, "failed");
        assert_eq!(expert.error.as_deref(), Some("timed out"));
    }

    // --- complete_topic ---

    #[test]
    fn complete_topic_sets_done_and_removes_from_queue() {
        let (_dir, mut qm) = setup();

        qm.add_topic("fasting", "Intermittent fasting").unwrap();
        let topic = make_topic("fasting", "Intermittent fasting");

        qm.claim_topic(&topic).unwrap();
        qm.complete_topic(&topic).unwrap();

        // removed from queue
        let pending = qm.get_pending_topics().unwrap();
        assert!(pending.is_empty());

        // meta says done
        let meta = qm.read_meta(&topic).unwrap();
        assert_eq!(meta.status, "done");
        assert!(meta.completed_at.is_some());
    }

    // --- fail_topic ---

    #[test]
    fn fail_topic_records_error_and_removes_from_queue() {
        let (_dir, mut qm) = setup();

        qm.add_topic("fasting", "Intermittent fasting").unwrap();
        let topic = make_topic("fasting", "Intermittent fasting");

        qm.claim_topic(&topic).unwrap();
        qm.fail_topic(&topic, "research agents all timed out").unwrap();

        // removed from queue (use `recover` to re-queue)
        let pending = qm.get_pending_topics().unwrap();
        assert!(pending.is_empty());

        // meta records failure
        let meta = qm.read_meta(&topic).unwrap();
        assert_eq!(meta.status, "failed");
        assert_eq!(meta.error.as_deref(), Some("research agents all timed out"));
    }

    // --- is_already_processed ---

    #[test]
    fn not_processed_when_no_meta_exists() {
        let (_dir, qm) = setup();
        let topic = make_topic("fasting", "Intermittent fasting");
        assert!(!qm.is_already_processed(&topic));
    }

    #[test]
    fn failed_topic_is_already_processed() {
        let (_dir, mut qm) = setup();
        let topic = make_topic("fasting", "Intermittent fasting");

        qm.add_topic("fasting", "Intermittent fasting").unwrap();
        qm.claim_topic(&topic).unwrap();
        qm.fail_topic(&topic, "something broke").unwrap();

        // failed topics are "already processed" so they don't auto-retry
        assert!(qm.is_already_processed(&topic));
    }

    #[test]
    fn processed_when_status_is_done() {
        let (_dir, mut qm) = setup();
        let topic = make_topic("fasting", "Intermittent fasting");

        qm.add_topic("fasting", "Intermittent fasting").unwrap();
        qm.claim_topic(&topic).unwrap();
        qm.complete_topic(&topic).unwrap();

        assert!(qm.is_already_processed(&topic));
    }

    // --- full lifecycle ---

    #[test]
    fn full_lifecycle_add_claim_record_complete() {
        let (_dir, mut qm) = setup();

        // add two topics
        qm.add_topic("alpha", "Topic A").unwrap();
        qm.add_topic("beta", "Topic B").unwrap();
        assert_eq!(qm.get_pending_topics().unwrap().len(), 2);

        // process alpha
        let alpha = make_topic("alpha", "Topic A");
        qm.claim_topic(&alpha).unwrap();
        qm.update_status(&alpha, "researching").unwrap();
        qm.record_agent_result(&alpha, "research_academic", "done", 100.0, None, None).unwrap();
        qm.update_status(&alpha, "synthesizing").unwrap();
        qm.update_status(&alpha, "validating").unwrap();
        qm.complete_topic(&alpha).unwrap();

        // alpha gone from queue, beta remains
        let pending = qm.get_pending_topics().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "beta");

        // alpha is marked done
        assert!(qm.is_already_processed(&alpha));
    }

    // --- validate_queue ---

    #[test]
    fn validate_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let issues = validate_queue(&dir.path().join("nope.yaml")).unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("does not exist"));
    }

    #[test]
    fn validate_valid_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.yaml");
        std::fs::write(&path, "topics:\n- id: a\n  input: Topic A\n").unwrap();

        let issues = validate_queue(&path).unwrap();
        assert!(issues.is_empty());
    }

    #[test]
    fn validate_empty_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.yaml");
        std::fs::write(&path, "topics:\n- id: \"\"\n  input: Something\n").unwrap();

        let issues = validate_queue(&path).unwrap();
        assert!(issues.iter().any(|i| i.contains("empty id")));
    }

    #[test]
    fn validate_empty_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.yaml");
        std::fs::write(&path, "topics:\n- id: test\n  input: \"  \"\n").unwrap();

        let issues = validate_queue(&path).unwrap();
        assert!(issues.iter().any(|i| i.contains("empty input")));
    }

    #[test]
    fn validate_duplicate_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.yaml");
        std::fs::write(&path, "topics:\n- id: dup\n  input: First\n- id: dup\n  input: Second\n").unwrap();

        let issues = validate_queue(&path).unwrap();
        assert!(issues.iter().any(|i| i.contains("Duplicate")));
    }

    #[test]
    fn validate_invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.yaml");
        std::fs::write(&path, "{{{{not valid").unwrap();

        let issues = validate_queue(&path).unwrap();
        assert!(issues.iter().any(|i| i.contains("parse error")));
    }

    #[test]
    fn validate_empty_file_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.yaml");
        std::fs::write(&path, "").unwrap();

        let issues = validate_queue(&path).unwrap();
        assert!(issues.is_empty());
    }

    // --- serialization round-trip ---

    #[test]
    fn topic_meta_survives_yaml_round_trip() {
        let meta = TopicMeta {
            id: "test".to_string(),
            input: "A multiline\ntopic input".to_string(),
            status: "done".to_string(),
            started_at: Some("2026-04-07T12:00:00Z".to_string()),
            completed_at: Some("2026-04-07T12:15:00Z".to_string()),
            error: None,
            agents: {
                let mut map = std::collections::BTreeMap::new();
                map.insert("research_academic".to_string(), AgentMeta {
                    status: "done".to_string(),
                    duration: Some("120.5s".to_string()),
                    error: None,
                    ..Default::default()
                });
                map.insert("synthesis".to_string(), AgentMeta {
                    status: "failed".to_string(),
                    duration: Some("30.0s".to_string()),
                    error: Some("timed out".to_string()),
                    ..Default::default()
                });
                map
            },
        };

        let yaml = serde_yaml::to_string(&meta).unwrap();
        let deserialized: TopicMeta = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(deserialized.id, "test");
        assert_eq!(deserialized.status, "done");
        assert_eq!(deserialized.agents.len(), 2);
        assert_eq!(deserialized.agents["synthesis"].error.as_deref(), Some("timed out"));
    }

    #[test]
    fn queue_file_survives_yaml_round_trip() {
        let queue = QueueFile {
            topics: vec![
                make_topic("a", "Topic A"),
                make_topic("b", "Multiline\ntopic\ninput"),
            ],
        };

        let yaml = serde_yaml::to_string(&queue).unwrap();
        let deserialized: QueueFile = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(deserialized.topics.len(), 2);
        assert_eq!(deserialized.topics[1].input, "Multiline\ntopic\ninput");
    }

    // --- recover_failed ---

    #[test]
    fn recover_requeues_failed_topics() {
        let (_dir, mut qm) = setup();

        qm.add_topic("fasting", "Intermittent fasting").unwrap();
        let topic = make_topic("fasting", "Intermittent fasting");

        qm.claim_topic(&topic).unwrap();
        qm.record_agent_result(&topic, "research_academic", "done", 100.0, None, None).unwrap();
        qm.fail_topic(&topic, "synthesis timed out").unwrap();

        // fail_topic removes from queue
        assert!(qm.get_pending_topics().unwrap().is_empty());

        // recover should re-add it
        let recovered = qm.recover_failed().unwrap();
        assert_eq!(recovered, vec!["fasting"]);

        // back in the queue
        let pending = qm.get_pending_topics().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "fasting");

        // meta reset to pending but agent results preserved
        let meta = qm.read_meta(&topic).unwrap();
        assert_eq!(meta.status, "pending");
        assert!(meta.error.is_none());
        assert!(meta.started_at.is_none());
        assert_eq!(meta.agents.len(), 1);
        assert_eq!(meta.agents["research_academic"].status, "done");
    }

    #[test]
    fn recover_requeues_interrupted_topics() {
        let (_dir, mut qm) = setup();
        let topic = make_topic("fasting", "Intermittent fasting");

        // simulate a topic that was mid-pipeline when the process died
        qm.claim_topic(&topic).unwrap();
        qm.update_status(&topic, "synthesizing").unwrap();

        let recovered = qm.recover_failed().unwrap();
        assert_eq!(recovered, vec!["fasting"]);

        let meta = qm.read_meta(&topic).unwrap();
        assert_eq!(meta.status, "pending");
    }

    #[test]
    fn recover_skips_done_topics() {
        let (_dir, mut qm) = setup();

        qm.add_topic("fasting", "Intermittent fasting").unwrap();
        let topic = make_topic("fasting", "Intermittent fasting");

        qm.claim_topic(&topic).unwrap();
        qm.complete_topic(&topic).unwrap();

        let recovered = qm.recover_failed().unwrap();
        assert!(recovered.is_empty());
    }

    #[test]
    fn recover_does_not_duplicate_queue_entries() {
        let (_dir, mut qm) = setup();

        qm.add_topic("fasting", "Intermittent fasting").unwrap();
        let topic = make_topic("fasting", "Intermittent fasting");

        qm.claim_topic(&topic).unwrap();
        qm.fail_topic(&topic, "broke").unwrap();

        // fail_topic removes from queue
        assert!(qm.get_pending_topics().unwrap().is_empty());

        // recover adds it back
        let recovered = qm.recover_failed().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(qm.get_pending_topics().unwrap().len(), 1);

        // recovering again should not add a second entry
        let recovered_again = qm.recover_failed().unwrap();
        assert!(recovered_again.is_empty());
        assert_eq!(qm.get_pending_topics().unwrap().len(), 1);
    }

    #[test]
    fn recover_returns_empty_when_no_output_dir() {
        let dir = tempfile::tempdir().unwrap();
        let queue_path = dir.path().join("queue.yaml");
        std::fs::write(&queue_path, "topics: []\n").unwrap();
        let mut qm = QueueManager::new(queue_path, dir.path().join("nonexistent-output"));

        let recovered = qm.recover_failed().unwrap();
        assert!(recovered.is_empty());
    }

    // --- get_all_statuses ---

    #[test]
    fn status_report_categorizes_correctly() {
        let (_dir, mut qm) = setup();

        // pending topic (in queue, no meta)
        qm.add_topic("pending-one", "Pending topic").unwrap();

        // in-progress topic
        let in_progress = make_topic("in-progress", "In progress topic");
        qm.claim_topic(&in_progress).unwrap();
        qm.update_status(&in_progress, "synthesizing").unwrap();

        // done topic
        qm.add_topic("done-one", "Done topic").unwrap();
        let done_topic = make_topic("done-one", "Done topic");
        qm.claim_topic(&done_topic).unwrap();
        qm.complete_topic(&done_topic).unwrap();

        // failed topic
        qm.add_topic("failed-one", "Failed topic").unwrap();
        let failed = make_topic("failed-one", "Failed topic");
        qm.claim_topic(&failed).unwrap();
        qm.fail_topic(&failed, "something broke").unwrap();

        let report = qm.get_all_statuses().unwrap();

        assert_eq!(report.pending.len(), 1);
        assert_eq!(report.pending[0].id, "pending-one");

        assert_eq!(report.in_progress.len(), 1);
        assert_eq!(report.in_progress[0].id, "in-progress");
        assert_eq!(report.in_progress[0].status, "synthesizing");

        assert_eq!(report.done.len(), 1);
        assert_eq!(report.done[0].id, "done-one");

        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].id, "failed-one");
        assert_eq!(report.failed[0].error.as_deref(), Some("something broke"));
    }

    #[test]
    fn status_report_empty_when_nothing_exists() {
        let (_dir, qm) = setup();
        let report = qm.get_all_statuses().unwrap();

        assert!(report.pending.is_empty());
        assert!(report.in_progress.is_empty());
        assert!(report.done.is_empty());
        assert!(report.failed.is_empty());
    }

    #[test]
    fn status_report_does_not_double_count_in_progress_as_pending() {
        let (_dir, mut qm) = setup();

        // topic is in queue AND has a meta (in-progress)
        qm.add_topic("fasting", "Intermittent fasting").unwrap();
        let topic = make_topic("fasting", "Intermittent fasting");
        qm.claim_topic(&topic).unwrap();
        qm.update_status(&topic, "researching").unwrap();

        let report = qm.get_all_statuses().unwrap();

        // should be in_progress, NOT pending
        assert!(report.pending.is_empty());
        assert_eq!(report.in_progress.len(), 1);
    }

    // --- file locking ---

    #[test]
    fn concurrent_adds_do_not_lose_data() {
        let (_dir, mut qm) = setup();

        // simulate rapid sequential adds (same process, tests lock reentrance)
        for i in 0..20 {
            qm.add_topic(&format!("topic-{i}"), &format!("Topic number {i}")).unwrap();
        }

        let topics = qm.get_pending_topics().unwrap();
        assert_eq!(topics.len(), 20);

        // verify all IDs are present
        for i in 0..20 {
            assert!(topics.iter().any(|t| t.id == format!("topic-{i}")),
                "Missing topic-{i}");
        }
    }

    #[test]
    fn lock_mut_does_not_write_on_failure() {
        let (_dir, mut qm) = setup();

        qm.add_topic("existing", "Existing topic").unwrap();

        // this should fail (duplicate) and NOT modify the file
        let result = qm.add_topic("existing", "Duplicate");
        assert!(result.is_err());

        // original still intact
        let topics = qm.get_pending_topics().unwrap();
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].input, "Existing topic");
    }

    #[test]
    fn queue_file_created_on_first_write_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let queue_path = dir.path().join("new-queue.yaml");
        let output_dir = dir.path().join("output");
        std::fs::create_dir_all(&output_dir).unwrap();
        let mut qm = QueueManager::new(queue_path.clone(), output_dir);

        // file doesn't exist yet
        assert!(!queue_path.exists());

        // adding a topic should create it
        qm.add_topic("first", "First topic").unwrap();
        assert!(queue_path.exists());

        let topics = qm.get_pending_topics().unwrap();
        assert_eq!(topics.len(), 1);
    }
}
