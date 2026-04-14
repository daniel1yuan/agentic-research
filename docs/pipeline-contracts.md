# Pipeline Data Contracts

This document is the source of truth for what each phase of the pipeline consumes and produces. Every prompt must enforce its phase's output contract. Every phase trusts its input contract. A phase that cannot meet its output contract must fail — it must not produce a degraded-but-valid-looking output that slips past downstream agents.

If this document and a prompt disagree, the prompt is wrong.

## Principles

1. **Hard formats, not suggestions.** Each phase produces a document with a fixed structure that a downstream agent (or grep) can parse without judgment. "Include a section with findings" is not a contract. "Each finding is an H2 with the following required fields, in this order" is a contract.

2. **No information loss at phase boundaries.** Every validator finding must be accounted for by triage. Every triage action must be accounted for in the revision ledger. Every ledger FIX must be verifiable in the final document. Silent drops are defects.

3. **Each phase has one job.** Research finds sources. Synthesis combines them. Validation critiques. Triage merges and prioritizes. Revision applies fixes. Verify checks the fixes were applied. An agent that does two jobs is doing both badly.

4. **Tags over prose.** Where a field has a bounded set of values (severity, disposition, source tag), use an exact token, not a sentence. Downstream phases pattern-match on tokens.

5. **Match format strictness to consumer.** Outputs have three audiences, with three format styles:

   - **Machine-readable** (consumed by Rust code): strict YAML frontmatter parsed with `serde_yaml`. `verify.md` is the only file in this category — `pipeline.rs` reads its frontmatter to decide `done` vs. `needs_human_review`. Malformed frontmatter is treated as `needs_human_review: true` so a broken verifier can't silently pass topics.
   - **LLM-readable** (consumed by a downstream LLM agent): markdown with hard structural conventions (fixed section headers, required fields in fixed order, bounded-vocabulary tokens for categorical fields). LLMs parse fuzzy structure fine, but we still pin the structure so the *producer* can't drift. Research files, validator reports, triage, and the revision ledger are all in this category.
   - **Human-readable** (consumed by you): prose in a fixed section skeleton. This applies only to the synthesis body (`overview.md` sections 2-7) and the revised synthesis body in `overview_final.md`. Even here, the *skeleton* is machine-enforced (8 fixed section titles, origin tags on every claim) — only the prose inside sections is freeform.

   Files may be hybrid: `triage.md` and `verify.md` both carry a YAML frontmatter header *and* an LLM/human-readable body. In hybrid files, the frontmatter is the contract; the body is a restatement. If they disagree, frontmatter wins.

## Pipeline shape

```
research (3 ∥)
    → synthesis
    → validation (4 ∥)
    → triage
    → revision
    → verify
    → done
```

Six phases, eleven agent invocations per topic (3 + 1 + 4 + 1 + 1 + 1).

## Origin tags (used in synthesis and onward)

Every factual claim in synthesis, revision, and any document derived from them must carry an **origin tag**. There are exactly four:

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

## Severity taxonomy (used in validation, triage, revision)

Findings are categorized into exactly one of these buckets. Prompts must use these tokens verbatim.

| Severity | Definition | Example |
|---|---|---|
| `factual_error` | The synthesis contradicts the source, or cites a study that doesn't exist, or misstates a study's finding. | "Lusardi 2014 found one-third got compound interest right" — actually one-third got all three questions right. |
| `attribution_gap` | The claim is in the research files but cited incorrectly or to the wrong source. | Attributing a dividend-policy argument to "Fama & French 1993" when it's Felix's application. |
| `scope_gap` | A topic the user asked about is missing or thinly covered. | User asked about "personal AI tooling" — synthesis omits the tooling section. |
| `inference_unmarked` | The synthesis presents a reasoning-based claim as a sourced fact (no `[inference: ...]` tag). | "Subjects were in their first weeks of training" — not stated in research, presented without tag. |
| `framing` | Wording subtly favors one side, even though the underlying claim is accurate. | "Proponents argue" vs. "critics concede" for symmetric positions. |
| `non_actionable` | The validator has a concern but it has no specific fix. Treated as noise downstream. | "The synthesis could be more rigorous overall." |

Triage may downgrade `non_actionable` findings to discarded. Revision must reject any `non_actionable` finding that survives to its input.

---

## Phase 1: Research

**Agents:** `research_academic`, `research_expert`, `research_general` (run in parallel)

**Inputs:**
- `{topic}` text

**Outputs:**
- `research/academic.md`
- `research/expert.md`
- `research/general.md`

**Output contract:**

Each file has:

1. An H1 title naming the topic and the source category (`# Academic Research: <topic>`).
2. A `## Sources` section containing one `### Source <N>` subsection per source.
3. Each source subsection has these required fields, as a bulleted list, in this order:
   - `**type**:` (for academic: peer-reviewed paper | meta-analysis | systematic review | preprint; for expert: institution | individual expert | industry body; for general: journalism | practitioner writing | community discussion)
   - `**tier**:` 1 | 2 | 3 | 4
   - `**url**:` full URL (or "unavailable" with reason)
   - `**author**:` name(s) or institution
   - `**year**:` YYYY
   - `**finding**:` one-paragraph summary of what this source actually claims
   - `**methodology**:` (academic only) sample size, design, limitations
   - `**credentials**:` (expert only) why this person is an authority on this topic
   - `**author_background**:` (general only) who the author is and how to weigh them
4. A `## Summary` section: 3-5 bullet points of the most important findings across all sources in this file.

**Minimum source counts:** academic ≥ 8, expert ≥ 6, general ≥ 6.

**Role differentiation (important):**

The three research files must be non-overlapping in what they contain. A source that fits multiple categories goes in exactly one — the most stringent applicable category:

- Academic file: peer-reviewed papers and meta-analyses only. Nothing else.
- Expert file: named practitioners, institutions, or industry bodies with explicit credentials. May cite academic work when characterizing an expert's position, but the source itself is the expert commentary, not the paper.
- General file: journalism, practitioner blogs, community discussions, case studies. Everything that isn't academic or credentialed expert.

If a source appears in two files, that is a defect.

**Failure policy:** if any one research agent fails, the topic fails. Synthesis requires all three.

---

## Phase 2: Synthesis

**Agent:** `synthesizer` (Opus)

**Inputs:**
- `research/academic.md`
- `research/expert.md`
- `research/general.md`

**Output:** `overview.md`

**Output contract:**

Exactly these 8 H2 sections, in this order, with these exact titles:

1. `## 1. Executive Summary`
2. `## 2. Background and Context`
3. `## 3. What the Evidence Says`
4. `## 4. Expert Perspectives`
5. `## 5. Practical Considerations`
6. `## 6. Controversies and Debates`
7. `## 7. Gaps and Limitations`
8. `## 8. Source Summary Table`

Sections 1-7 are prose. Section 8 is a markdown table with columns: `Source | Type | Tier | Key Contribution | Year`.

**Tagging rules:**
- Every factual claim in sections 2-7 carries a citation `[Author/Source, Year]` immediately followed by an origin tag from the four above.
- Section 1 (Executive Summary) is exempt from per-sentence tagging but every claim in it must be traceable to a tagged claim later in the document.
- Section 8 (Source Summary Table) is exempt — the table itself is the citation record.

**Structural requirements:**
- No additional H2 sections. Sub-sections (H3+) are allowed within any of the 8.
- A "Revision Notes" section is NOT produced here (it's a revision-phase artifact).

**Failure policy:** if synthesis doesn't meet the structural contract (wrong number of sections, missing tags on claims), the verifier will catch it downstream — but ideally the synthesizer's prompt prevents it.

---

## Phase 3: Validation

**Agents:** `validate_bias`, `validate_sources`, `validate_claims`, `validate_completeness` (parallel)

**Inputs:**
- `overview.md`
- `research/academic.md`, `research/expert.md`, `research/general.md`

**Outputs:**
- `validation/bias.md`
- `validation/sources.md`
- `validation/claims.md`
- `validation/completeness.md`

**Output contract (same for all four files):**

```markdown
# <Validator Name> Report

## Summary
- Total findings: <N>
- By severity: factual_error: <n>, attribution_gap: <n>, scope_gap: <n>, inference_unmarked: <n>, framing: <n>, non_actionable: <n>

## Findings

### <validator>-1
- **severity**: factual_error
- **location**: Section 3, paragraph 2
- **quoted_text**: "<exact string from overview.md, ≤200 chars>"
- **issue**: <one sentence>
- **evidence**: <what the research file actually says, with file:section reference>
- **proposed_fix**: "<exact replacement text>" OR "N/A" OR "REMOVE"

### <validator>-2
...
```

**Required fields per finding:**

| Field | Required | Notes |
|---|---|---|
| `severity` | yes | One of the seven severity tokens, verbatim |
| `location` | yes | `Section <n>, paragraph <m>` or `Section 8, row <n>` — must be precise enough for grep |
| `quoted_text` | yes | Exact substring of `overview.md`, ≤200 chars, no ellipses |
| `issue` | yes | One sentence |
| `evidence` | yes | What the research files say, with `<file>:<section>` reference |
| `proposed_fix` | yes | Exact replacement text in quotes, OR `N/A` for non-actionable, OR `REMOVE` to strike the sentence |

**Finding IDs:** `<validator-name>-<N>`, starting at 1 within each file. So `bias-1`, `bias-2`, `claims-1`, `sources-1`, etc. IDs are stable within a run and referenced by triage.

**Hard constraints on validators:**

- Validators are **read-only auditors**. The prompt must not contain words like "fix", "edit", "correct", "rewrite" — only "identify", "flag", "report".
- `validate_sources` may use `WebSearch` and `WebFetch`, capped at `max_web_tool_calls` combined calls per run. This is a **configurable per-agent limit** in `config.yaml` (default 25). The cap is a hard budget, not a sample size — the validator must prioritize rather than randomly sample. See the Per-validator focus section below for prioritization criteria.
- `validate_sources` runs with a tighter `timeout` than the global default (recommended 900s / 15 min) so a hang fails fast instead of burning the full 3600s budget. The root cause of prior hangs on a long-running topic is unknown; the timeout is a mitigation.
- `validate_sources` receives `max_turns: 35` by recommendation — higher than the 25 default — because prioritization logic plus web tool calls needs more room.
- No other validator has web tool access. `validate_claims`, `validate_bias`, and `validate_completeness` get `Read` + `Grep` only.
- If a validator cannot produce a finding in the required format, it should produce zero findings rather than a malformed one. Downstream phases will mark missing validators as a topic-level failure.
- Validators must not propose fixes that would require new research. `proposed_fix` can only use information already in the research files or a direct edit of existing text.

**Per-validator focus:**

- `validate_bias`: selection bias, framing bias, source imbalance, false balance, omission bias. Emits `framing` primarily; emits `scope_gap` only when an omission has a clear directional effect.
- `validate_sources`: source existence, attribution accuracy (author/year/publication), tier assignment accuracy, **tier inflation** (flagging when the synthesis upgrades a source's tier above what the research files assigned), origin tag correctness (verifying that `[academic]` sources live in `research/academic.md`, `[expert]` in `research/expert.md`, `[general]` in `research/general.md`). Prioritizes web tool calls on: (1) sources cited more than once, (2) sources supporting Executive Summary or central Section 3 claims, (3) sources whose tier assignment looks aggressive, (4) sources with any red flag in how they're described in the research files. Random sampling is forbidden.
- `validate_claims`: claim-to-source faithfulness, overstatement/understatement, cherry-picking, unmarked inference (claims that should carry an `[inference: ...]` tag but don't). Assumes sources exist — that's `validate_sources`'s job.
- `validate_completeness`: neutral coverage gaps — findings in the research files that didn't make it into the synthesis, sub-topics of the query that weren't addressed, thin sections. Emits `scope_gap` without regard to direction.

**Failure policy:** if fewer than 2 of 4 validators succeed, topic fails. 2 or 3 succeeding is a warning but the pipeline proceeds.

---

## Phase 4: Triage

**Agent:** `triage` (Opus)

**Inputs:**
- `validation/bias.md`
- `validation/sources.md`
- `validation/claims.md`
- `validation/completeness.md`

**Inputs it does NOT receive:** the synthesis, the research files, the topic text. Triage operates only on validator reports. This is deliberate — it prevents triage from second-guessing the validators or doing its own validation.

**Output:** `triage.md`

**Output contract:**

The file begins with a YAML frontmatter block containing count totals, followed by the action list body. The frontmatter exists so the verifier can cheaply cross-check that the body matches its own headers — full action list parsing stays on the LLM side.

```markdown
---
validator_findings_consumed: 18
actions_produced: 14
discarded: 4
by_severity:
  factual_error: 3
  attribution_gap: 5
  scope_gap: 2
  inference_unmarked: 3
  framing: 1
  non_actionable: 0
---

# Triage Action List

## Actions

### triage-1
- **severity**: factual_error
- **sources**: [claims-2, sources-4]
- **location**: Section 3, paragraph 2
- **quoted_text**: "<exact string from overview.md>"
- **issue**: <merged one-sentence description>
- **proposed_fix**: "<exact replacement text>"
- **rationale**: <why merged/kept/reassigned severity — one sentence>

### triage-2
...

## Discarded

### claims-7
- **reason**: non_actionable — validator said "could be more rigorous" with no specific change proposed
- **original_severity**: non_actionable

### bias-3
- **reason**: contradicted by sources-5 which disputes the same framing from the opposite direction; neither is clearly correct
- **original_severity**: framing
```

**Triage responsibilities:**

1. **Merge duplicates.** If two validators flag the same issue (same `quoted_text` or overlapping `location` and `issue`), produce one action with both validator IDs in `sources`.
2. **Resolve contradictions.** If two validators contradict each other, either pick one with explicit rationale, or discard both.
3. **Reassign severity** if a validator mislabeled. Note the change in `rationale`.
4. **Discard non-actionable findings** and any finding without a concrete `proposed_fix`.
5. **Account for every validator finding.** Every `<validator>-<N>` ID from the four input files must appear either in Actions or in Discarded. No silent drops.

**Failure policy:** if triage cannot account for every validator finding, the pipeline fails. This is enforceable by the verifier (which sees the triage file).

---

## Phase 5: Revision

**Agent:** `revision` (Sonnet)

**Inputs:**
- `overview.md` (the synthesis)
- `triage.md` (the action list)

**Inputs it does NOT receive:** the validator reports. Revision works only from the triaged action list. If it needs to see the original finding, it's a triage bug.

**Output:** `overview_final.md`

**Output contract:**

The file contains the revised synthesis followed by a revision ledger:

```markdown
<same 8 sections as overview.md, with fixes applied>

## Revision Ledger

### triage-1
- **disposition**: FIX
- **action**: Replaced "only one-third of respondents can correctly answer basic questions about compound interest" with "only one-third of respondents correctly answered all three basic financial literacy questions (compound interest, inflation, and risk diversification)"

### triage-2
- **disposition**: REJECT
- **reason**: Validator misread the synthesis; the qualifier "predates the commercial relationship" is in Section 8 row 4, which the validator did not cite.

### triage-3
- **disposition**: DEFER
- **reason**: Would require verifying a paywalled primary source, which revision cannot do within its tool set.
```

**Required fields per ledger entry:**

| Field | Required | Notes |
|---|---|---|
| `disposition` | yes | `FIX`, `REJECT`, or `DEFER` — exact token |
| `action` | FIX only | The exact change made, with before/after text |
| `reason` | REJECT, DEFER only | One sentence |

**Structural requirements:**

- The 8-section structure of the synthesis must be preserved exactly. Section count, section titles, section order. No additions, no removals, no renames.
- Origin tags must be preserved. A revised claim keeps its original tag unless the fix changes the source basis, in which case the new tag is required.
- Total word count must be within ±10% of `overview.md`. If revision hits this bound, it is rewriting rather than revising — it should reject or defer low-priority fixes until it fits.
- Every `triage-<N>` ID from `triage.md` Actions section must appear in the ledger exactly once.
- The ledger is the last section of the document.

**Hard constraints on revision:**

- Revision's prompt must not contain the words "improve", "refine", "polish", "enhance", or "rewrite". Only "apply", "address", "fix", "reject", "defer".
- Revision may not invoke WebSearch or WebFetch. Its tool set is Read + Write only.
- Revision may not add new citations or new claims beyond what a specific FIX requires.

**Failure policy:** if revision's ledger does not account for every triage action, the verifier will fail the topic.

---

## Phase 6: Verify

**Agent:** `verify` (Haiku)

**Inputs:**
- `overview_final.md`
- `triage.md`

**Output:** `verify.md`

**Output contract:**

Verify's output is consumed by `pipeline.rs` to decide whether a topic is `done` or `needs_human_review`. It therefore uses the YAML-frontmatter-plus-markdown-body pattern (see Principle 5). The verifier agent writes both blocks directly; `pipeline.rs` parses only the frontmatter.

```markdown
---
overall: fail
needs_human_review: true
failed_checks:
  - fix_application
checks:
  ledger_completeness:
    status: pass
    triage_actions: 14
    ledger_entries: 14
    missing: []
  fix_application:
    status: fail
    total_fixes: 9
    applied: 7
    missing:
      - triage-3
      - triage-8
  origin_tags:
    status: pass
    claims_scanned: 47
    untagged: 0
  inference_rationales:
    status: pass
    inference_tags: 6
    empty_rationales: 0
  structure:
    status: pass
    expected_sections: 8
    found_sections: 8
---

# Verification Report

**Overall: FAIL** (1 of 5 checks failed)

## check_1_ledger_completeness: PASS
All 14 triage actions are accounted for in the ledger.

## check_2_fix_application: FAIL
2 of 9 FIX entries were not applied to overview_final.md:

- **triage-3**: Ledger claims replacement text "only one-third of respondents correctly answered all three basic financial literacy questions" but this string is not present in the body. The original flagged text also isn't — the sentence appears to have been rewritten a third way.
- **triage-8**: Ledger claims a Poterba et al. citation was added in Section 3, but no new `[Poterba et al., 2002]` reference appears there.

## check_3_origin_tags: PASS
All 47 factual claims in sections 2-7 carry a valid origin tag.

## check_4_inference_rationales: PASS
All 6 `[inference: ...]` tags have non-empty rationales.

## check_5_structure: PASS
All 8 expected section titles present, in order.
```

**Frontmatter schema (strict — `pipeline.rs` depends on this):**

| Field | Type | Values |
|---|---|---|
| `overall` | string | `pass` \| `fail` |
| `needs_human_review` | bool | `true` iff `overall: fail` |
| `failed_checks` | list[string] | subset of the five check names |
| `checks.<name>.status` | string | `pass` \| `fail` |
| `checks.<name>.*` | scalars | per-check metrics (see example) |

Check names are exactly: `ledger_completeness`, `fix_application`, `origin_tags`, `inference_rationales`, `structure`. No other keys under `checks`.

**Body format:** one H2 section per check, titled `## check_<n>_<name>: PASS | FAIL`, with a short human-readable explanation. The body exists for debugging — if `pipeline.rs` flags a topic as needing review, you cat `verify.md` and immediately see what broke.

**Body–frontmatter consistency:** if they disagree, frontmatter wins. The verifier's prompt must make this clear: the frontmatter is the contract, the body is a human-readable restatement.

**Check definitions (all are mechanical):**

1. **ledger_completeness:** every `triage-<N>` in `triage.md` under `## Actions` appears exactly once under `## Revision Ledger` in `overview_final.md`. Grep-based.
2. **fix_application:** for each ledger entry with `disposition: FIX`, the `action` field's replacement text appears as a substring of the body of `overview_final.md` (i.e. before the `## Revision Ledger` section). Grep-based.
3. **origin_tags:** every sentence in sections 2-7 of `overview_final.md` that contains a `[Author, Year]` style citation is followed by exactly one origin tag (`[academic]`, `[expert]`, `[general]`, or `[inference: ...]`). Regex-based.
4. **inference_rationales:** every `[inference: ...]` tag has a non-empty rationale inside the brackets. Regex-based. (Verify does **not** judge whether the rationale is *defensible* — that's out of scope for mechanical verification.)
5. **structure:** the 8 expected H2 section titles appear in order in the body (before the ledger). String match.

(Length is intentionally not enforced. The Foundations section may be long or short depending on source depth, and revision may grow or shrink the doc as triage actions require — a length cap was a proxy metric that fought the goal.)

**Hard constraints on verify:**

- Verify runs on Haiku. Its prompt must not ask for any judgment — only mechanical checks with defined pass/fail criteria.
- Verify's tool set is Read only. No Write to anything except its own output file. No WebFetch, no Grep-the-web.
- Verify must not modify `overview_final.md` under any circumstances, even if it finds trivially fixable problems.

**Failure policy:**

- If `Overall: FAIL`, the topic is marked `needs_human_review: true` in `meta.yaml` with a list of which checks failed.
- The pipeline does NOT automatically re-run revision. Failed verification surfaces to the user; re-running is a manual `recover` decision.
- `Overall: PASS` marks the topic as `done` and removes it from the queue.

---

## Updated file layout

```
output/{topic-id}/
  meta.yaml                  # state: status, per-agent results, needs_human_review flag
  overview.md                # synthesis output
  triage.md                  # triage action list (NEW)
  overview_final.md          # revised synthesis + ledger
  verify.md                  # verification report (NEW)
  research/
    academic.md
    expert.md
    general.md
  validation/
    bias.md
    sources.md
    claims.md
    completeness.md
  responses/
    research_academic.json
    research_expert.json
    research_general.json
    synthesis.json
    validate_bias.json
    validate_sources.json
    validate_claims.json
    validate_completeness.json
    triage.json              # NEW
    revision.json
    verify.json               # NEW
  sources/                   # reserved
```

## Model assignments (recommended)

| Phase | Agent | Model | max_turns | timeout | Other | Rationale |
|---|---|---|---|---|---|---|
| Research | research_academic | sonnet | 25 | 3600 | — | Breadth + web search |
| Research | research_expert | sonnet | 25 | 3600 | — | Breadth + web search |
| Research | research_general | sonnet | 25 | 3600 | — | Breadth + web search |
| Synthesis | synthesizer | opus | 25 | 3600 | — | Cross-source reasoning |
| Validation | validate_bias | sonnet | 25 | 3600 | — | Judgment-heavy |
| Validation | validate_claims | sonnet | 25 | 3600 | — | Judgment-heavy |
| Validation | validate_sources | sonnet | 35 | 900 | `max_web_tool_calls: 25` | Prioritized web verification with fail-fast hang mitigation |
| Validation | validate_completeness | haiku | 25 | 3600 | — | Mechanical inventory |
| Triage | triage | opus | 25 | 3600 | — | Hardest reasoning in the pipeline |
| Revision | revision | sonnet | 25 | 3600 | — | Mechanical application of triage |
| Verify | verify | haiku | 25 | 3600 | — | Pattern matching only |

These are recommendations for `config.yaml`, not hard-coded defaults. Any agent can be overridden per-run.

**New config field:** `max_web_tool_calls` is a per-agent integer cap on the combined number of `WebSearch` + `WebFetch` invocations. It lives in `AgentOverride` alongside `model`, `max_turns`, and `timeout`. Default is 25 if unset. Only `validate_sources` uses it today, but the mechanism is general.

**Prompt placeholder:** `{max_web_tool_calls}` is substituted into `validate_sources.md` at prompt load time. If a validator prompt uses web tools and does not include this placeholder, that's a defect — the cap must be visible to the agent.
