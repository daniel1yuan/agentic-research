use anyhow::Result;
use std::path::Path;

const PROMPTS: &[(&str, &str)] = &[
    ("research_academic.md", include_str!("../prompts/research_academic.md")),
    ("research_expert.md", include_str!("../prompts/research_expert.md")),
    ("research_general.md", include_str!("../prompts/research_general.md")),
    ("synthesize.md", include_str!("../prompts/synthesize.md")),
    ("validate_bias.md", include_str!("../prompts/validate_bias.md")),
    ("validate_sources.md", include_str!("../prompts/validate_sources.md")),
    ("validate_claims.md", include_str!("../prompts/validate_claims.md")),
    ("validate_completeness.md", include_str!("../prompts/validate_completeness.md")),
    ("revise.md", include_str!("../prompts/revise.md")),
];

const DEFAULT_CONFIG: &str = "\
# CLI command. Supports args, e.g., \"ccs production\" or just \"claude\".
cli_command: \"claude\"

# Maximum number of topics to research concurrently
max_concurrent_topics: 2

# Default timeout per agent invocation in seconds
agent_timeout: 600

# Default claude model for all agents
# Aliases: opus, sonnet, haiku. Full names also work (e.g., claude-sonnet-4-6).
model: \"sonnet\"

# Default max conversation turns per agent
max_turns: 25

# Max USD per topic before the pipeline bails. 0 = no limit.
max_cost_per_topic: 0

# Output directory (relative to project root)
output_dir: \"output\"

# Queue file path (relative to project root)
queue_file: \"queue.yaml\"

# Per-agent overrides. Synthesis and revision benefit from a stronger model
# since they need to reason across multiple sources.
agents:
  synthesizer:
    model: \"opus\"
  revision:
    model: \"opus\"

# All agent names for reference:
#   research_academic, research_expert, research_general,
#   synthesizer, validate_bias, validate_sources,
#   validate_claims, validate_completeness, revision
#
# Each supports: model, max_turns, timeout
";

const DEFAULT_QUEUE: &str = "topics: []\n";

pub fn run_init(project_dir: &Path, force: bool) -> Result<()> {
    let prompts_dir = project_dir.join("prompts");
    let config_path = project_dir.join("config.yaml");
    let queue_path = project_dir.join("queue.yaml");
    let output_dir = project_dir.join("output");

    // prompts
    std::fs::create_dir_all(&prompts_dir)?;
    for (filename, content) in PROMPTS {
        let path = prompts_dir.join(filename);
        if path.exists() && !force {
            println!("  skip  prompts/{filename} (already exists)");
        } else {
            std::fs::write(&path, content)?;
            println!("  write prompts/{filename}");
        }
    }

    // config
    if config_path.exists() && !force {
        println!("  skip  config.yaml (already exists)");
    } else {
        std::fs::write(&config_path, DEFAULT_CONFIG)?;
        println!("  write config.yaml");
    }

    // queue
    if queue_path.exists() && !force {
        println!("  skip  queue.yaml (already exists)");
    } else {
        std::fs::write(&queue_path, DEFAULT_QUEUE)?;
        println!("  write queue.yaml");
    }

    // output dir
    std::fs::create_dir_all(&output_dir)?;
    println!("  mkdir output/");

    Ok(())
}
