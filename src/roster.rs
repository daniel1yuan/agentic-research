//! Central definition of all agents in the pipeline.
//! Used by pipeline (execution), preflight (validation), and init (scaffolding).

pub struct AgentDef {
    pub name: &'static str,
    pub prompt_file: &'static str,
    pub output_file: &'static str,
    pub allowed_tools: &'static [&'static str],
}

const RESEARCH_TOOLS: &[&str] = &["Read", "Write", "WebSearch", "WebFetch"];
const ANALYSIS_TOOLS: &[&str] = &["Read", "Write", "Glob", "Grep"];
const ANALYSIS_TOOLS_WITH_WEB: &[&str] = &["Read", "Write", "Glob", "Grep", "WebSearch", "WebFetch"];

pub const SYNTHESIS_TOOLS: &[&str] = ANALYSIS_TOOLS;
/// Triage reads the 4 validator reports and emits an action list. No web, no Edit.
pub const TRIAGE_TOOLS: &[&str] = ANALYSIS_TOOLS;
/// Revision reads synthesis + triage action list and applies fixes. Strictly no web —
/// the previous version had web tools but the contract now forbids them. Revision is
/// mechanical application, not research.
pub const REVISION_TOOLS: &[&str] = ANALYSIS_TOOLS;
/// Verify is a mechanical Haiku-shaped pattern-matcher. Read-only check against the
/// final doc + triage. No web, no Edit.
pub const VERIFY_TOOLS: &[&str] = ANALYSIS_TOOLS;

pub const RESEARCH_AGENTS: &[AgentDef] = &[
    AgentDef { name: "research_academic", prompt_file: "research_academic.md", output_file: "academic.md", allowed_tools: RESEARCH_TOOLS },
    AgentDef { name: "research_expert", prompt_file: "research_expert.md", output_file: "expert.md", allowed_tools: RESEARCH_TOOLS },
    AgentDef { name: "research_general", prompt_file: "research_general.md", output_file: "general.md", allowed_tools: RESEARCH_TOOLS },
];

pub const VALIDATION_AGENTS: &[AgentDef] = &[
    AgentDef { name: "validate_bias", prompt_file: "validate_bias.md", output_file: "bias.md", allowed_tools: ANALYSIS_TOOLS },
    AgentDef { name: "validate_sources", prompt_file: "validate_sources.md", output_file: "sources.md", allowed_tools: ANALYSIS_TOOLS_WITH_WEB },
    AgentDef { name: "validate_claims", prompt_file: "validate_claims.md", output_file: "claims.md", allowed_tools: ANALYSIS_TOOLS },
    AgentDef { name: "validate_completeness", prompt_file: "validate_completeness.md", output_file: "completeness.md", allowed_tools: ANALYSIS_TOOLS },
];

pub const SYNTHESIS_PROMPT: &str = "synthesize.md";
pub const TRIAGE_PROMPT: &str = "triage.md";
pub const REVISION_PROMPT: &str = "revise.md";
pub const VERIFY_PROMPT: &str = "verify.md";

/// All prompt files the pipeline needs (for preflight and init).
pub fn all_prompt_files() -> Vec<&'static str> {
    let mut files: Vec<&str> = Vec::new();
    for a in RESEARCH_AGENTS { files.push(a.prompt_file); }
    files.push(SYNTHESIS_PROMPT);
    for a in VALIDATION_AGENTS { files.push(a.prompt_file); }
    files.push(TRIAGE_PROMPT);
    files.push(REVISION_PROMPT);
    files.push(VERIFY_PROMPT);
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn research_agents_have_web_tools_but_not_glob() {
        for agent in RESEARCH_AGENTS {
            assert!(agent.allowed_tools.contains(&"WebSearch"), "{} should have WebSearch", agent.name);
            assert!(agent.allowed_tools.contains(&"WebFetch"), "{} should have WebFetch", agent.name);
            assert!(!agent.allowed_tools.contains(&"Glob"), "{} should not have Glob", agent.name);
        }
    }

    #[test]
    fn validation_agents_have_correct_tools() {
        for agent in VALIDATION_AGENTS {
            assert!(agent.allowed_tools.contains(&"Read"), "{} should have Read", agent.name);
            assert!(agent.allowed_tools.contains(&"Glob"), "{} should have Glob", agent.name);
        }
        let sources = VALIDATION_AGENTS.iter().find(|a| a.name == "validate_sources").unwrap();
        assert!(sources.allowed_tools.contains(&"WebSearch"));
        assert!(sources.allowed_tools.contains(&"WebFetch"));
        let bias = VALIDATION_AGENTS.iter().find(|a| a.name == "validate_bias").unwrap();
        assert!(!bias.allowed_tools.contains(&"WebSearch"));
        let claims = VALIDATION_AGENTS.iter().find(|a| a.name == "validate_claims").unwrap();
        assert!(!claims.allowed_tools.contains(&"WebSearch"));
        let completeness = VALIDATION_AGENTS.iter().find(|a| a.name == "validate_completeness").unwrap();
        assert!(!completeness.allowed_tools.contains(&"WebSearch"));
    }

    #[test]
    fn synthesis_tools_exclude_web() {
        assert!(!SYNTHESIS_TOOLS.contains(&"WebSearch"));
        assert!(SYNTHESIS_TOOLS.contains(&"Read"));
        assert!(SYNTHESIS_TOOLS.contains(&"Glob"));
    }

    #[test]
    fn revision_tools_exclude_web() {
        assert!(!REVISION_TOOLS.contains(&"WebSearch"), "revision must not have web access");
        assert!(!REVISION_TOOLS.contains(&"WebFetch"), "revision must not have web access");
        assert!(REVISION_TOOLS.contains(&"Read"));
        assert!(REVISION_TOOLS.contains(&"Write"));
    }

    #[test]
    fn triage_tools_exclude_web() {
        assert!(!TRIAGE_TOOLS.contains(&"WebSearch"));
        assert!(!TRIAGE_TOOLS.contains(&"WebFetch"));
        assert!(TRIAGE_TOOLS.contains(&"Read"));
    }

    #[test]
    fn verify_tools_exclude_web() {
        assert!(!VERIFY_TOOLS.contains(&"WebSearch"));
        assert!(!VERIFY_TOOLS.contains(&"WebFetch"));
        assert!(VERIFY_TOOLS.contains(&"Read"));
    }

    #[test]
    fn all_prompt_files_returns_eleven() {
        assert_eq!(all_prompt_files().len(), 11);
    }

    #[test]
    fn all_prompt_files_contains_new_phases() {
        let files = all_prompt_files();
        assert!(files.contains(&"triage.md"));
        assert!(files.contains(&"verify.md"));
    }
}
