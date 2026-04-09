mod agent;
mod config;
mod init;
mod pipeline;
mod preflight;
mod progress;
mod queue;
mod roster;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "agentic-research", about = "Agentic deep research pipeline")]
struct Cli {
    /// Config file path
    #[arg(long, default_value = "config.yaml")]
    config: PathBuf,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Process all pending topics in the queue
    Run {
        /// Override model (e.g., opus, sonnet, haiku)
        #[arg(long)]
        model: Option<String>,

        /// Skip preflight checks
        #[arg(long)]
        skip_preflight: bool,
    },

    /// Add a topic to the queue
    Add {
        /// Topic description
        topic: String,

        /// Custom topic ID (auto-generated from topic if omitted)
        #[arg(long)]
        id: Option<String>,
    },

    /// Show status of all topics (queued, in-progress, done, failed)
    Status,

    /// Remove a topic from the queue
    Remove {
        /// Topic ID to remove
        id: String,
    },

    /// Reset a topic: wipe all outputs and re-queue for a fresh run
    Reset {
        /// Topic ID to reset
        id: String,
    },

    /// Re-queue failed and interrupted topics for retry
    Recover,

    /// Run preflight checks without processing
    Preflight,

    /// Set up a new project directory (prompts, config, queue)
    Init {
        /// Overwrite existing files
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = if cli.verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("info")
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let project_root = std::env::current_dir()?;

    // init runs before config loading (config.yaml may not exist yet)
    if let Command::Init { force } = &cli.command {
        println!("Initializing project in {}\n", project_root.display());
        init::run_init(&project_root, *force)?;
        println!("\nDone. Next steps:");
        println!("  1. Edit config.yaml if you want to change defaults");
        println!("  2. Add topics:  agentic-research add \"your topic here\"");
        println!("  3. Run:         agentic-research run");
        return Ok(());
    }

    let config_path = project_root.join(&cli.config);
    let mut config = config::Config::load(&config_path)?;

    let queue_path = project_root.join(&config.queue_file);
    let output_dir = project_root.join(&config.output_dir);
    let prompts_dir = project_root.join(&config.prompts_dir);
    // store the resolved absolute path back so the pipeline can use it directly
    config.prompts_dir = prompts_dir.to_string_lossy().to_string();

    match cli.command {
        Command::Init { .. } => unreachable!(),
        Command::Run { model, skip_preflight } => {
            if !skip_preflight {
                preflight::run_checks(&config, &queue_path, &output_dir, &prompts_dir).await?;
            }

            // CLI --model flag overrides config
            if let Some(model) = model {
                config.model = model;
            }

            let queue_manager = queue::QueueManager::new(queue_path, output_dir.clone());
            let topics = queue_manager.get_pending_topics()?;

            let pending: Vec<_> = topics
                .into_iter()
                .filter(|t| !queue_manager.is_already_processed(t))
                .collect();

            if pending.is_empty() {
                println!("No pending topics in queue.");
                return Ok(());
            }

            println!("Found {} pending topic(s):", pending.len());
            for topic in &pending {
                let preview: String = topic.input.chars().take(config::topic_preview_len()).collect();
                println!("  - {}: {}", topic.id, preview);
            }
            println!();

            let pool = pipeline::WorkerPool::new(output_dir, Arc::new(config));

            let results = pool.process_all(&pending, queue_manager).await;

            println!("\nResults:");
            for (topic_id, success) in &results {
                let status = if *success { "done" } else { "FAILED" };
                println!("  {topic_id}: {status}");
            }
        }

        Command::Add { topic, id } => {
            let slug = slug::slugify(&topic);
            let topic_id = id.unwrap_or_else(|| {
                slug.chars().take(config::slug_max_len()).collect()
            });
            let mut queue_manager = queue::QueueManager::new(queue_path, output_dir);
            queue_manager.add_topic(&topic_id, &topic)?;
            println!("Added topic '{topic_id}' to queue.");
        }

        Command::Status => {
            let queue_manager = queue::QueueManager::new(queue_path, output_dir);
            let report = queue_manager.get_all_statuses()?;

            if !report.pending.is_empty() {
                println!("Pending ({}):", report.pending.len());
                for topic in &report.pending {
                    let preview: String = topic.input.chars().take(config::topic_preview_len()).collect();
                    println!("  - {}: {}", topic.id, preview);
                }
            }

            if !report.in_progress.is_empty() {
                println!("\nIn Progress ({}):", report.in_progress.len());
                for meta in &report.in_progress {
                    println!("  - {}: {} (started: {})",
                        meta.id, meta.status,
                        meta.started_at.as_deref().unwrap_or("?"));
                    if cli.verbose {
                        print_agent_details(&meta.agents);
                    }
                }
            }

            if !report.done.is_empty() {
                println!("\nDone ({}):", report.done.len());
                for meta in &report.done {
                    let topic_cost = topic_total_cost(&meta.agents);
                    let topic_tokens = topic_total_tokens(&meta.agents);
                    println!("  - {} (${:.4}, {}k tokens)", meta.id, topic_cost, topic_tokens / 1000);
                    if cli.verbose {
                        print_agent_details(&meta.agents);
                    }
                }
            }

            if !report.failed.is_empty() {
                println!("\nFailed ({}):", report.failed.len());
                for meta in &report.failed {
                    let error_preview = meta.error.as_deref().unwrap_or("unknown error");
                    println!("  - {}: {}", meta.id, error_preview);
                    if cli.verbose {
                        print_agent_details(&meta.agents);
                    }
                }
                println!("\n  Run `recover` to re-queue failed topics.");
            }

            if report.pending.is_empty() && report.in_progress.is_empty()
                && report.done.is_empty() && report.failed.is_empty()
            {
                println!("No topics found.");
            }
        }

        Command::Remove { id } => {
            let mut queue_manager = queue::QueueManager::new(queue_path, output_dir);
            queue_manager.remove_topic(&id)?;
            println!("Removed topic '{id}' from queue.");
        }

        Command::Reset { id } => {
            let mut queue_manager = queue::QueueManager::new(queue_path, output_dir);
            queue_manager.reset_topic(&id)?;
            println!("Reset topic '{id}'. All outputs wiped and re-queued for fresh run.");
        }

        Command::Recover => {
            let mut queue_manager = queue::QueueManager::new(queue_path, output_dir);
            let recovered = queue_manager.recover_failed()?;

            if recovered.is_empty() {
                println!("No failed or interrupted topics to recover.");
            } else {
                println!("Recovered {} topic(s):", recovered.len());
                for id in &recovered {
                    println!("  - {id}");
                }
                println!("\nRun `run` to process them. Completed agent outputs will be reused.");
            }
        }

        Command::Preflight => {
            preflight::run_checks(&config, &queue_path, &output_dir, &prompts_dir).await?;
            println!("\nAll preflight checks passed.");
        }
    }

    Ok(())
}

fn print_agent_details(agents: &std::collections::BTreeMap<String, queue::AgentMeta>) {
    for (name, info) in agents {
        let duration = info.duration.as_deref().unwrap_or("");
        print!("      {name}: {} ({duration})", info.status);
        if let Some(cost) = info.cost_usd {
            print!(" ${cost:.4}");
        }
        if let Some(input) = info.input_tokens {
            let output = info.output_tokens.unwrap_or(0);
            print!(" ({input}in/{output}out)");
        }
        if let Some(err) = &info.error {
            let preview: String = err.chars().take(config::status_error_preview_len()).collect();
            print!(" {preview}");
        }
        println!();
    }
}

fn topic_total_cost(agents: &std::collections::BTreeMap<String, queue::AgentMeta>) -> f64 {
    agents.values().filter_map(|a| a.cost_usd).sum()
}

fn topic_total_tokens(agents: &std::collections::BTreeMap<String, queue::AgentMeta>) -> u64 {
    agents.values().map(|a| {
        a.input_tokens.unwrap_or(0) + a.output_tokens.unwrap_or(0)
            + a.cache_creation_tokens.unwrap_or(0)
    }).sum()
}
