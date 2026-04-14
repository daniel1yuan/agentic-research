use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::agent::AgentResult;

/// Print a phase header for a topic.
pub fn phase(topic_id: &str, phase_name: &str) {
    println!("[{topic_id}] {phase_name}");
}

/// Print that an agent was skipped (cached output exists).
pub fn agent_cached(name: &str) {
    println!("  - {name} (cached)");
}

/// Print that agents are starting (parallel batch).
pub fn agents_starting(names: &[&str]) {
    if names.is_empty() { return; }
    let list = names.join(", ");
    println!("  running: {list}");
}

/// Print the result of a single agent invocation.
pub fn agent_done(name: &str, result: &AgentResult) {
    if result.success {
        let cost = result.usage.as_ref()
            .map(|u| format!(" ${:.4}", u.cost_usd))
            .unwrap_or_default();
        println!("  + {name} ({:.0}s{cost})", result.duration_seconds);
    } else {
        let err = result.error.as_deref().unwrap_or("unknown error");
        let preview: String = err.chars().take(80).collect();
        println!("  x {name}: {preview}");
    }
}

/// Print an agent error (when the runner itself fails, not the agent).
pub fn agent_error(name: &str, error: &str) {
    let preview: String = error.chars().take(80).collect();
    println!("  x {name}: {preview}");
}

/// Print topic completion.
pub fn topic_done(topic_id: &str, total_cost: f64) {
    if total_cost > 0.0 {
        println!("[{topic_id}] Done (${total_cost:.4} total)\n");
    } else {
        println!("[{topic_id}] Done\n");
    }
}

/// Print topic failure.
pub fn topic_failed(topic_id: &str, error: &str) {
    let preview: String = error.chars().take(120).collect();
    println!("[{topic_id}] Failed: {preview}\n");
}

/// Print that a topic reached verify but the verifier flagged it for human review.
/// Not a failure — the pipeline completed cleanly — but the final doc needs attention.
pub fn topic_needs_review(topic_id: &str, reason: &str, total_cost: f64) {
    let cost_part = if total_cost > 0.0 {
        format!(" (${total_cost:.4} total)")
    } else {
        String::new()
    };
    let preview: String = reason.chars().take(120).collect();
    println!("[{topic_id}] Needs human review{cost_part}: {preview}\n");
}

/// Spawn a background heartbeat that prints a dot every `interval` seconds
/// while agents are running. Returns a handle to stop it.
pub fn start_heartbeat(interval_secs: u64) -> HeartbeatHandle {
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();

    let handle = tokio::spawn(async move {
        let mut elapsed = 0u64;
        while running_clone.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
            if running_clone.load(Ordering::SeqCst) {
                elapsed += interval_secs;
                print!("  ... {elapsed}s\r");
                let _ = std::io::stdout().flush();
            }
        }
    });

    HeartbeatHandle { running, _handle: handle }
}

pub struct HeartbeatHandle {
    running: Arc<AtomicBool>,
    _handle: tokio::task::JoinHandle<()>,
}

impl HeartbeatHandle {
    pub fn stop(self) {
        self.running.store(false, Ordering::SeqCst);
        // clear the heartbeat line
        print!("              \r");
        let _ = std::io::stdout().flush();
    }
}
