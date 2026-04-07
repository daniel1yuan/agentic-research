use anyhow::{bail, Result};
use std::path::Path;

use crate::agent;
use crate::config::Config;
use crate::queue;
use crate::roster;

pub async fn run_checks(config: &Config, queue_path: &Path, output_dir: &Path, prompts_dir: &Path) -> Result<()> {
    println!("Running preflight checks...\n");
    let mut all_passed = true;

    let cli = &config.cli_command;

    print!("  {} CLI installed ... ", cli);
    if agent::is_cli_installed(cli) {
        println!("ok");
    } else {
        println!("FAIL");
        println!("    '{}' not found in PATH. Install from https://claude.ai/download", cli);
        println!("    If your binary is named differently, set cli_command in config.yaml");
        all_passed = false;
    }

    print!("  {} auth working ... ", cli);
    match agent::test_cli_connectivity(cli).await {
        Ok(true) => println!("ok"),
        Ok(false) => {
            println!("FAIL");
            println!("    '{}' returned an error. Check your authentication.", cli);
            all_passed = false;
        }
        Err(e) => {
            println!("FAIL");
            println!("    Error testing connectivity: {e}");
            all_passed = false;
        }
    }

    print!("  config valid ... ");
    println!("ok (model: {}, timeout: {}s, concurrency: {})",
        config.model, config.agent_timeout, config.max_concurrent_topics);

    print!("  queue file ... ");
    if !queue_path.exists() {
        println!("FAIL");
        println!("    Queue file not found: {}", queue_path.display());
        all_passed = false;
    } else {
        let issues = queue::validate_queue(queue_path)?;
        if issues.is_empty() {
            let topics = queue::QueueManager::new(queue_path.to_path_buf(), output_dir.to_path_buf())
                .get_pending_topics()?;
            println!("ok ({} topic(s) queued)", topics.len());
        } else {
            println!("FAIL");
            for issue in &issues {
                println!("    {issue}");
            }
            all_passed = false;
        }
    }

    print!("  prompts directory ... ");
    let required_prompts = roster::all_prompt_files();
    let missing: Vec<_> = required_prompts
        .iter()
        .filter(|p| !prompts_dir.join(p).exists())
        .collect();

    if missing.is_empty() {
        println!("ok ({} prompts found)", required_prompts.len());
    } else {
        println!("FAIL");
        for m in &missing {
            println!("    Missing: {m}");
        }
        println!("    Run `agentic-research init` to create them.");
        all_passed = false;
    }

    print!("  output directory ... ");
    if let Err(e) = std::fs::create_dir_all(output_dir) {
        println!("FAIL");
        println!("    Cannot create output directory: {e}");
        all_passed = false;
    } else {
        println!("ok ({})", output_dir.display());
    }

    if !all_passed {
        println!();
        bail!("Preflight checks failed. Fix the issues above before running.");
    }

    Ok(())
}
