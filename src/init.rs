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
    ("triage.md", include_str!("../prompts/triage.md")),
    ("revise.md", include_str!("../prompts/revise.md")),
    ("verify.md", include_str!("../prompts/verify.md")),
];

const DEFAULT_CONFIG: &str = "\
# CLI command. Supports args, e.g., \"ccs production\" or just \"claude\".
cli_command: \"claude\"

# Environment variables for agent subprocesses.
# If you use ccs account profiles, set CLAUDE_CONFIG_DIR to route auth:
#   cli_env:
#     CLAUDE_CONFIG_DIR: \"/Users/you/.ccs/instances/your-profile\"
cli_env: {}

# Maximum number of topics to research concurrently
max_concurrent_topics: 2

# Default timeout per agent invocation in seconds
agent_timeout: 3600

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

# Per-agent overrides. Each agent supports:
#   model, max_turns, timeout, max_web_tool_calls
# Unset fields fall back to the global defaults above.
#
# The pipeline has 11 agents across 6 phases. Synthesis and triage are the
# hardest reasoning; validate_completeness and verify are mechanical and can
# run on Haiku. validate_sources has a tighter timeout and a web tool cap
# because it fetches external sources.
agents:
  synthesizer:
    model: \"opus\"
  validate_sources:
    max_turns: 35
    timeout: 900
    max_web_tool_calls: 25
  validate_completeness:
    model: \"haiku\"
  triage:
    model: \"opus\"
  verify:
    model: \"haiku\"

# All agent names for reference:
#   research_academic, research_expert, research_general,
#   synthesizer,
#   validate_bias, validate_sources, validate_claims, validate_completeness,
#   triage, revision, verify
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
