# Prompt Architecture

## Overview

The quality of the research output comes from the prompts. The code is plumbing. The prompts are the product.

There are 11 prompt files in `prompts/`, one per agent. They're embedded in the binary via `include_str!` and extracted to disk on `init`. You can edit them freely after init. `init --force` resets them to defaults.

Every prompt's job is to enforce the output contract for its phase. Contracts are defined in [`pipeline-contracts.md`](pipeline-contracts.md). If you edit a prompt, make sure it still produces output that matches the contract — downstream phases depend on it.

## Pipeline flow and prompt dependencies

```
research_academic.md  ──┐
research_expert.md    ──┼──> synthesize.md ──> validate_bias.md        ──┐
research_general.md   ──┘                  ──> validate_sources.md     ──┤
                                           ──> validate_claims.md      ──┼──> triage.md ──> revise.md ──> verify.md
                                           ──> validate_completeness.md──┘
```

Each prompt receives the topic text via `{topic}` placeholder substitution. Later prompts also receive file paths to read their inputs: `{research_dir}`, `{synthesis_path}`, `{validation_dir}`, `{triage_path}`, `{final_path}`.

**Information flow is strict.** Each phase sees only what its contract specifies:

- `triage.md` sees the 4 validator reports and nothing else — no synthesis, no research files, no topic text. This prevents triage from second-guessing validators.
- `revise.md` sees the synthesis and the triage action list — not the original validator reports. Revision is pure application.
- `verify.md` sees the final document and the triage action list — nothing else. Verify is mechanical checking, not judgment.

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

Defined in `synthesize.md` and used as a reference in `validate_sources.md` (the only validator that judges tier assignment). Other validators no longer repeat the definitions — they were removed during the rewrite to stop the tier block from drifting across files.

- **Tier 1**: Peer-reviewed papers, systematic reviews, meta-analyses
- **Tier 2**: Expert commentary, institutional reports, recognized domain authorities
- **Tier 3**: Quality journalism, industry reports, well-sourced blog posts
- **Tier 4**: Forum discussions, opinion pieces, social media

Non-expert opinions are valid data points. They're just not presented as expert opinions. Tier 4 sources are included but clearly labeled.

## Origin tags

Every factual claim in the synthesis and revision carries an **origin tag** indicating where the claim comes from. Tags are mandatory and are enforced by the verifier phase. There are exactly four:

| Tag | Meaning |
|---|---|
| `[academic]` | Claim is directly stated in `research/academic.md` |
| `[expert]` | Claim is directly stated in `research/expert.md` |
| `[general]` | Claim is directly stated in `research/general.md` |
| `[inference: <rationale>]` | Claim is the synthesizer's move — combining, extrapolating, or contextualizing. The rationale names what was combined from where. |

Tags appear inline, immediately after the citation:

> `Meta-analyses find a 0.5 kg hypertrophy advantage for high-volume training [Smith 2022] [academic].`
>
> `The factor model does not itself test dividend policy; this application is Felix's [inference: Fama & French 1993 provides the framework, Felix applies it in his 2019 video].`

A claim without an origin tag is a defect. "Reasonable inference" without an `[inference: ...]` tag is hallucination.

The full contract is in [`pipeline-contracts.md`](pipeline-contracts.md) under "Origin tags".

## Severity taxonomy

Findings produced by validators (and merged/reassigned by triage) are categorized into exactly one of seven severity tokens. Prompts use these tokens verbatim so downstream phases can pattern-match.

| Severity | Definition | Who emits it |
|---|---|---|
| `factual_error` | The synthesis contradicts the source, cites a nonexistent study, or misstates a finding. | `validate_claims`, `validate_sources` |
| `attribution_gap` | The source exists but is described inaccurately (wrong author/year/venue/tier), or a claim is attributed to the wrong source or wrong origin tag. | `validate_sources` (tier, attribution), `validate_claims` (claim-level mis-tag) |
| `scope_gap` | Missing content — either neutral (flagged by completeness) or directionally unfair (flagged by bias). | `validate_completeness` (neutral), `validate_bias` (directional) |
| `inference_unmarked` | A synthesis claim presented as sourced fact without an `[inference: ...]` tag, when it should have one. | `validate_claims` |
| `inference_review` | A claim is properly tagged as inference but the rationale is weak, the leap is large, or the claim is load-bearing. **Informational only** — routes to `## Inference Notes` in triage, not to actions. Surfaces inferences worth scrutinizing without asserting they are wrong. | `validate_claims` |
| `framing` | Wording, emphasis, source imbalance, or asymmetric charity. | `validate_bias` |
| `non_actionable` | Vague concern with no specific change. Triage discards these. | Rarely emitted; usually collapsed to a real severity or dropped. |

Each validator owns a specific subset of severities. The division is documented in [`pipeline-contracts.md`](pipeline-contracts.md) under "Per-validator focus". In short:

- `validate_sources` owns the source object (does it exist, is it attributed correctly, is its tier defensible).
- `validate_claims` owns the claim-to-source fit (does the synthesis's use of the source match what the source says).
- `validate_bias` owns directional unfairness (framing, directional omission).
- `validate_completeness` owns neutral coverage (what's missing, regardless of direction).

Overlaps are intentional — bias and completeness can both flag the same omission from different angles. The triage phase merges duplicates.

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

## Triage prompt

`triage.md` reads the four validator reports (and nothing else) and produces a single structured action list. Triage is where the hard cross-report reasoning happens: merging duplicate findings across validators, resolving contradictions between validators, reassigning severity when a validator mislabeled, and discarding non-actionable concerns.

Triage must account for every validator finding. Every `<validator>-<N>` ID from the four input files appears either in `## Actions` or in `## Discarded`. No silent drops. This is the contract revision downstream depends on.

Triage runs on Opus — it's the most reasoning-heavy phase in the pipeline.

## Revision prompt

`revise.md` reads the synthesis (`overview.md`) and the triage action list (`triage.md`) and nothing else. It deliberately does not see the validator reports — that separation is how the pipeline enforces "one job per agent." Triage already did the thinking; revision's job is disciplined application.

For every triage action, revision must produce a ledger entry with one of three dispositions: `FIX` (with the exact change made), `REJECT` (with reason), or `DEFER` (with reason). The ledger is the primary artifact — it's what the verifier checks downstream.

Hard constraints:
- Preserve the 8-section structure of the synthesis exactly (no additions, removals, or renames)
- Stay within ±10% of the synthesis word count
- No new web searches, no new claims beyond what a specific FIX requires
- Every origin tag on an unchanged claim stays; a changed claim needs the appropriate new tag

Revision runs on Sonnet — it's execution, not reasoning.

## Verify prompt

`verify.md` is a mechanical check that runs after revision. It reads `overview_final.md` and `triage.md` and produces a pass/fail report against five specific checks: ledger completeness, fix application, origin tags present, inference rationales non-empty, and 8-section structure intact.

Verify is deliberately not allowed to judge. It does not evaluate whether an inference rationale is *defensible* — only whether it exists. It does not evaluate whether a fix was *correct* — only whether the replacement text is present in the final document. Judgment is out of scope; that's what makes Haiku sufficient.

On `FAIL`, the topic is marked `needs_human_review: true` in `meta.yaml`. The pipeline does not auto-retry — failed verification is a signal for the user to inspect.

Verify runs on Haiku. Its tool set is Read only.

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
| `{topic}` | research_*, synthesize, validate_* | The topic's `input` text from queue.yaml |
| `{research_dir}` | synthesize, validate_* | Path to the `research/` directory |
| `{synthesis_path}` | validate_*, verify | Path to `overview.md` |
| `{validation_dir}` | triage | Path to the `validation/` directory |
| `{triage_path}` | revise, verify | Path to `triage.md` |
| `{final_path}` | verify | Path to `overview_final.md` |

Note that `triage.md`, `revise.md`, and `verify.md` deliberately do not receive `{topic}` or other upstream context beyond their specified inputs. Each phase is scoped to exactly what its contract requires.
