/// Central definition of all agents in the pipeline.
/// Used by pipeline (execution), preflight (validation), and init (scaffolding).

pub struct AgentDef {
    pub name: &'static str,
    pub prompt_file: &'static str,
    pub output_file: &'static str,
    pub needs_web: bool,
}

pub const RESEARCH_AGENTS: &[AgentDef] = &[
    AgentDef { name: "research_academic", prompt_file: "research_academic.md", output_file: "academic.md", needs_web: true },
    AgentDef { name: "research_expert", prompt_file: "research_expert.md", output_file: "expert.md", needs_web: true },
    AgentDef { name: "research_general", prompt_file: "research_general.md", output_file: "general.md", needs_web: true },
];

pub const VALIDATION_AGENTS: &[AgentDef] = &[
    AgentDef { name: "validate_bias", prompt_file: "validate_bias.md", output_file: "bias.md", needs_web: false },
    AgentDef { name: "validate_sources", prompt_file: "validate_sources.md", output_file: "sources.md", needs_web: true },
    AgentDef { name: "validate_claims", prompt_file: "validate_claims.md", output_file: "claims.md", needs_web: false },
    AgentDef { name: "validate_completeness", prompt_file: "validate_completeness.md", output_file: "completeness.md", needs_web: false },
];

pub const SYNTHESIS_PROMPT: &str = "synthesize.md";
pub const REVISION_PROMPT: &str = "revise.md";

/// All prompt files the pipeline needs (for preflight and init).
pub fn all_prompt_files() -> Vec<&'static str> {
    let mut files: Vec<&str> = Vec::new();
    for a in RESEARCH_AGENTS { files.push(a.prompt_file); }
    files.push(SYNTHESIS_PROMPT);
    for a in VALIDATION_AGENTS { files.push(a.prompt_file); }
    files.push(REVISION_PROMPT);
    files
}
