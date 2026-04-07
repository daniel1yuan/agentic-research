# Prompt Architecture

## Overview

The quality of the research output comes from the prompts. The code is plumbing. The prompts are the product.

There are 9 prompt files in `prompts/`, one per agent. They're embedded in the binary via `include_str!` and extracted to disk on `init`. You can edit them freely after init. `init --force` resets them to defaults.

## Pipeline flow and prompt dependencies

```
research_academic.md  ──┐
research_expert.md    ──┼──> synthesize.md ──> validate_bias.md        ──┐
research_general.md   ──┘                  ──> validate_sources.md     ──┼──> revise.md
                                           ──> validate_claims.md      ──┤
                                           ──> validate_completeness.md──┘
```

Each prompt receives the topic text via `{topic}` placeholder substitution. Later prompts also receive file paths to read their inputs: `{research_dir}`, `{synthesis_path}`, `{validation_dir}`.

## Research prompts

Three agents search for different kinds of sources. They run in parallel with no knowledge of each other.

### `research_academic.md`
Searches for peer-reviewed papers, systematic reviews, meta-analyses. Outputs structured entries with: authors, year, publication, URL, type, credibility tier, key findings, methodology, relevance, limitations.

Key instructions:
- Follow citation trails (both directions)
- Minimum 8 sources
- Verify URLs with WebFetch
- Note conflicts of interest

### `research_expert.md`
Searches for expert opinions, institutional positions, authoritative commentary. Outputs: credentials, affiliation, URL, date, credibility tier, potential conflicts, position, key arguments, areas of agreement/disagreement.

Key instructions:
- Verify credentials (don't present non-experts as experts)
- Separate expertise from celebrity
- Track how positions have evolved
- Minimum 6 sources

### `research_general.md`
Searches for journalism, community perspectives, practitioner experience. Outputs: author background, platform, URL, date, type, credibility tier, author expertise level, summary, why the source matters, caveats.

Key instructions:
- Include lived experience (labeled as experiential, not scientific)
- Flag popular misconceptions
- Include minority perspectives
- Minimum 6 sources

### Output format differences

The three agents have different output structures because they're capturing different kinds of information. Academic sources need methodology descriptions. Expert sources need credential verification. General sources need author expertise assessment.

The synthesis prompt explicitly accounts for this: it tells the agent to normalize as it synthesizes, extracting claim + evidence quality + source identity from each format and citing consistently with `[Author/Source, Year]`.

## Source credibility tiers

Defined in `synthesize.md` and repeated as a reference block in all four `validate_*.md` files:

- **Tier 1**: Peer-reviewed papers, systematic reviews, meta-analyses
- **Tier 2**: Expert commentary, institutional reports, recognized domain authorities
- **Tier 3**: Quality journalism, industry reports, well-sourced blog posts
- **Tier 4**: Forum discussions, opinion pieces, social media

Non-expert opinions are valid data points. They're just not presented as expert opinions. Tier 4 sources are included but clearly labeled.

## Synthesis prompt

`synthesize.md` reads all three research files and produces a structured overview document with 8 sections:

1. Executive Summary
2. Background and Context
3. What the Evidence Says
4. Expert Perspectives
5. Practical Considerations
6. Controversies and Debates
7. Gaps and Limitations
8. Source Summary Table

Key design choices in this prompt:

- **Organized by claim, not by source.** The reader shouldn't have to mentally cross-reference three separate source lists.
- **Conflict resolution guidance.** When sources disagree, the prompt instructs the agent to name which tier says what, explain plausible reasons for disagreement, and be direct about evidence strength.
- **Input normalization guidance.** The prompt acknowledges the three research files have different formats and tells the agent how to normalize citations.

## Validation prompts

Four agents check the synthesis from orthogonal angles. They run in parallel, each reading the synthesis and the original research files.

### `validate_bias.md`
Checks for: selection bias, framing bias, source imbalance, false balance, omission bias. Outputs an overall rating (Low bias / Some concerns / Significant bias) with specific findings.

### `validate_sources.md`
Verifies that cited sources actually exist and are accurately represented. Uses WebSearch and WebFetch to check. Distinguishes between "confirmed fake" and "unable to verify." Checks credibility tier accuracy.

### `validate_claims.md`
Extracts key claims from the synthesis and traces each one to its source. Flags: unsupported claims, overstated/understated claims, misattributed claims, logical leaps, cherry-picking.

### `validate_completeness.md`
Assesses coverage gaps: missing sub-topics, missing stakeholder perspectives, temporal gaps, geographic blind spots, unused research findings.

## Revision prompt

`revise.md` reads the original synthesis and all four validation reports. Produces a final revised synthesis that addresses validator findings.

Key instructions:
- Prioritize major issues first (fabricated sources > claim accuracy > bias > completeness)
- Don't over-correct (mild framing issue doesn't need a full rewrite)
- Preserve what works (validators also note strengths)
- Be transparent when a gap can't be fully addressed
- Revision Notes section at the end documenting what changed and why

## Resource constraints

All three research prompts include a resource constraints section that tells the agent to map the landscape first (breadth), then go deeper on what matters most (depth). The intent is both breadth and depth, but breadth comes first so agents don't burn all their turns deep-diving one thread before they've seen the full picture.

The actual turn limit is set via config (`max_turns`, default 25). The prompt doesn't mention the exact number because it's configurable per agent. If agents are hitting the turn limit before producing thorough output, increase `max_turns` globally or per-agent in config.

## Editing prompts

The prompts are extracted to `prompts/` on `init`. You can edit them freely. Common customizations:

- **Change minimum source counts** to be more or less thorough
- **Add domain-specific search guidance** (e.g., "for medical topics, prioritize Cochrane reviews")
- **Adjust the synthesis structure** (add/remove sections, change the output format)
- **Tune validator strictness** (e.g., relax the source verification for topics with few online sources)

The placeholder variables available in each prompt:

| Variable | Available in | Resolves to |
|---|---|---|
| `{topic}` | All prompts | The topic's `input` text from queue.yaml |
| `{research_dir}` | synthesize.md | Path to the `research/` directory |
| `{synthesis_path}` | validate_*.md, revise.md | Path to `overview.md` |
| `{research_dir}` | validate_*.md | Path to the `research/` directory |
| `{validation_dir}` | revise.md | Path to the `validation/` directory |
