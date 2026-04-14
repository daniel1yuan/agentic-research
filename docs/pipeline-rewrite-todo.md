# Pipeline Rewrite TODO

Tracking work for the 4-phase → 6-phase pipeline rewrite. See [`pipeline-contracts.md`](pipeline-contracts.md) for the spec everything below is enforcing.

Context: the pipeline is being restructured from `research → synthesis → validation → revision` to `research → synthesis → validation → triage → revision → verify`. Motivation came from auditing real outputs in `~/Projects/research/output/`: revision was polishing rather than applying fixes, synthesis was presenting inference as fact, validators were producing unparseable free-form reports, and one validator hung for 3600s.

## Status snapshot

### Done

- [x] `docs/pipeline-contracts.md` — full spec for all 6 phases, origin tags, severity taxonomy, validator output format, triage/verify frontmatter, file layout, model recommendations
- [x] `docs/architecture.md` — updated to reflect 6 phases, new agent count, updated data flow, updated file layout
- [x] `docs/prompts.md` — updated pipeline diagram, new sections on triage/verify, information flow rules, placeholder table
- [x] **All 11 prompts drafted** (see `prompts/` directory)
- [x] **Chunk 1 — new phases wired into pipeline**
  - `src/queue.rs` — added `TopicStatus::Triaging` and `TopicStatus::Verifying` variants
  - `src/pipeline.rs` — `run_triage_phase()` and `run_verify_phase()` methods, wired into `run_inner()`, `{max_web_tool_calls}` placeholder substitution, `{triage_path}` / `{final_path}` placeholders
  - Tests updated: invocation counts now reflect 11 agents
- [x] **Chunk 2 — verify frontmatter parsing and NeedsReview routing**
  - `src/queue.rs` — new `TopicStatus::NeedsReview` variant, `mark_needs_review()` method, `recover_failed()` picks up NeedsReview, `is_already_processed()` recognizes NeedsReview
  - `src/progress.rs` — `topic_needs_review()` function
  - `src/pipeline.rs` — `PipelineOutcome` enum, `VerifyReport` deserialize struct, `extract_yaml_frontmatter()` / `parse_verify_report()` helpers, three-way match in `run()`
  - Tests: 12 new tests covering frontmatter parsing edge cases + pipeline outcome routing + recover flow
- [x] **Chunk 3 — cache robustness, validation threshold, init embeds**
  - `src/pipeline.rs` — `agent_output_exists()` now requires `.done` sidecar alongside content, `mark_output_complete()` writes sidecar after successful agent run, wired into all 6 phases (including the parallel research and validation phases)
  - `src/pipeline.rs` — validation threshold changed from 0-of-N bail to require-at-least-2 bail
  - `src/init.rs` — embeds `triage.md` and `verify.md` via `include_str!`, default config template documents all 11 agents and the new `max_web_tool_calls` field
  - Tests: new partial-write regression test, new "only 1 validator passing fails" test, new sidecar unit tests
- [x] **Docs cleanup**
  - `docs/failure-modes.md` — documented `needs_review` terminal state, the three-state taxonomy (done / needs_review / failed), sidecar-based partial-write recovery, verify failure recovery flow, `validate_sources` 900s backstop, 2-of-4 validation threshold, updated manual intervention commands to delete sidecars
  - `docs/configuration.md` — documented `max_web_tool_calls`, all 11 agents in the override table, per-override field explanations, recommended config block, per-agent timeout override rationale
  - `docs/prompts.md` — added Origin Tags section and Severity Taxonomy section with per-validator ownership; tier definitions section updated to reflect dedup
- [x] **Code polish round**
  - `src/agent.rs` — wrapper shrunk from three "use Write tool" reminders (~70 tokens) to a single trailing line with just the path (~20 tokens)
  - All 10 prompt files with trailing "Write the complete report..." redundancy cleaned up (synthesize.md didn't have one)
  - `src/roster.rs` — module doc comment fixed (outer → inner)
  - `src/pipeline.rs` — two clippy nits on unnecessary borrows
  - `cargo clippy` clean, zero warnings

**Test count: 93 passing, 0 failing, zero compiler warnings, zero clippy warnings.**

### Not done

Below. All code and prompt work for the core rewrite is complete; remaining items are config, docs cleanup, and deferred follow-ups.

---

## 1. Prompt rewrites

All prompt rewrites are complete — see "Done" list above. The cross-cutting "remove trailing Write tool reminder" cleanup still depends on `agent.rs`'s wrapper being shrunk first, and is tracked in the code section below.

---

## 2. Code changes

All core code changes and the polish round are complete. See the "Done" list above.

**Genuinely deferred** (not blocking an end-to-end run; revisit later):

- **Centralize `allowed_tools` into `roster::AgentDef`**: synthesis/triage/revision/verify tool sets are module-level `pub const` arrays rather than `AgentDef` entries. Research and validation agents already use `AgentDef`. Bringing the sequential-phase agents into the same pattern would finish the centralization noted in the original architecture.md "Known limitations" section. Not done because the current split (slice for parallel phases, consts for sequential) reflects how they're actually used in `pipeline.rs`, and forcing everything into `AgentDef` would add indirection without improving anything concrete.
- **Streaming stdout capture in `ClaudeRunner`**: the current runner collects subprocess output via `Command::output()` on completion. A hung agent leaves no partial transcript. Refactoring to `Command::spawn()` + tokio BufReader tasks would let a killed agent leave breadcrumbs, which would help diagnose hangs like the historical muscle-hypertrophy run. Deferred until a hang recurs under the new 900s timeout — no point refactoring on a single data point.

---

## 3. Config changes (user's `~/Projects/research/config.yaml`)

Update with recommended per-agent config:

```yaml
# existing fields stay

agents:
  research_academic:
    model: sonnet

  research_expert:
    model: sonnet

  research_general:
    model: sonnet

  synthesizer:
    model: opus

  validate_bias:
    model: sonnet
    max_turns: 25

  validate_claims:
    model: sonnet
    max_turns: 25

  validate_sources:
    model: sonnet
    max_turns: 35
    timeout: 900
    max_web_tool_calls: 25

  validate_completeness:
    model: haiku
    max_turns: 25

  triage:
    model: opus
    max_turns: 25

  revision:
    model: sonnet
    max_turns: 25

  verify:
    model: haiku
    max_turns: 25
```

---

## 4. Docs updates

All done — see the "Done" list above.

---

## 5. Open design questions (need user input)

1. **Bias vs completeness `scope_gap` split.** Does `validate_bias` emit `scope_gap` for directional omissions (Option A, my lean) or only `framing` (Option B, cleaner partitioning)? Blocking: `validate_bias.md` prompt and final form of the `Per-validator focus` section in contracts.md.

2. **Research agent non-overlap enforcement mechanism.** The contract requires the 3 research files to be non-overlapping. The prompts will say so. But is there a programmatic post-check? Options: (a) no check, trust the prompt (simplest), (b) add a completeness-validator-style sanity check that flags sources appearing in multiple files, (c) dedicated agent. My lean: (a) for now, revisit if non-overlap keeps failing in practice.

3. **Tier inflation: sources validator only, or also a post-synthesis automated check?** Tier comparisons are mechanical (compare source X's tier in research file vs in synthesis Section 8). We could do this in `verify.md` as a 7th check. I left it out to keep verify pure-mechanical-on-final-doc, but it's borderline. Worth revisiting after we see how often the sources validator actually catches tier inflation.

4. **Hang root cause on muscle-hypertrophy.** Unknown — no partial JSON was written. Current mitigation is the 900s timeout. Worth investigating if it recurs. Possibly add progress-to-disk writes so a hung agent leaves breadcrumbs.

5. **Should `triage.md` see the original topic text?** Current spec: no. Triage operates only on validator reports. But validators' `evidence` fields sometimes reference the topic ("the user asked about X, the synthesis omits X"), and triage may need the topic to judge scope. Counter-argument: if validators need the topic to be clear, they should put it in the finding themselves. Current spec stands, revisit if triage quality is poor.

---

## 6. Recommended ordering

When resuming implementation:

1. **Finish the validator prompts:** `validate_bias.md`, `validate_completeness.md`. Requires resolving open question 1 first.
2. **Write the new phase prompts:** `triage.md`, `verify.md`. These are structurally new.
3. **Shrink `revise.md`:** remove enumeration/triage steps now that `triage.md` exists.
4. **Update research prompts** for the structured format and non-overlap rules. Lowest priority of the prompt work — current research outputs are "good enough" and the sharpest pain is in validation → revision.
5. **Dedup tier definitions** across validator prompts (trivial).
6. **Code changes, in order:**
   a. `src/config.rs` — add `max_web_tool_calls` field and resolver (foundation)
   b. `src/roster.rs` — add new agents, update tool sets
   c. `src/agent.rs` — wrapper dedup + new agent constructors
   d. `src/pipeline.rs` — new phases, placeholder substitution, verify frontmatter parsing, cache sidecar marker, validation threshold update
   e. `src/init.rs` — embed new prompts, update config template
   f. `src/preflight.rs` — expect 11 prompts
7. **End-to-end test on one topic** against an existing cached run. Compare `overview_final.md` and `verify.md` against old `overview_final.md`. Iterate on prompts if quality drops.
8. **Update `~/Projects/research/config.yaml`** with recommended values.
9. **Docs cleanup:** `failure-modes.md`, `configuration.md`, `prompts.md` final passes.

---

## 7. Context for resuming

Key decisions already made (don't re-litigate):

- **6 phases, not 4.** Triage and verify are separate phases, not merged into revision.
- **Triage on Opus, verify on Haiku.** Triage is the hardest reasoning; verify is mechanical pattern-matching only.
- **Verify does not judge.** No inference-quality checks, no correctness checks. Only: did the fix land, do the tags exist, is the structure intact. (Length cap removed — was a proxy metric that fought the goal of variable-depth Foundations content.)
- **Verify failure → `needs_human_review`, no auto-retry.** Matches user preference for "built right > built fast."
- **Machine/LLM/human format tiers.** Verify output is machine-readable (YAML frontmatter). Triage/validators/ledger are LLM-readable (strict markdown). Synthesis body is human-readable (prose in a fixed skeleton).
- **Origin tags are mandatory in synthesis and onward.** `[academic]`, `[expert]`, `[general]`, `[inference: <rationale>]`. Missing tag = hallucination.
- **Severity taxonomy is exactly seven tokens.** `factual_error`, `attribution_gap`, `scope_gap`, `inference_unmarked`, `inference_review`, `framing`, `non_actionable`. No synonyms, no new categories. (`inference_review` is informational only — routes to `## Inference Notes` in triage, not to actions.)
- **`max_web_tool_calls` default: 25.** Based on observed max of 20 in real runs, plus headroom. Configurable per-agent.
- **`validate_sources` tool set: keep both `WebSearch` and `WebFetch`.** Data showed agent actually uses WebSearch, not WebFetch. Both kept, agent picks.
- **Triage sees only validator reports.** No synthesis, no research, no topic text. Enforces information scoping.
- **Revision sees only synthesis + triage action list.** No validator reports.

Evidence files (for re-grounding decisions):
- `~/Downloads/pipeline-improvements.md` — original cost/quality review that started this
- `~/Projects/research/output/*/` — actual pipeline runs, 6 topics, with raw responses in `responses/` subdirs
- `~/Projects/research/output/*/meta.yaml` — per-topic run metadata
- Earlier conversation turns include audits of validator behavior and `validate_sources.json` raw transcripts

## 8. Not doing (deliberately out of scope)

- Cross-topic synthesis
- Adaptive model selection based on cost
- CLI-level cache invalidation (`--invalidate-from synthesis`)
- Per-phase cost budgets
- Progress / cost projection UI
- Configurable phase thresholds (min research agents, min validators)
- Rate limit handling / retry logic
- Replacing the `include_str!` pattern in `init.rs`

These are from the original review file's "Longer-Term Ideas" section. Revisit after the core rewrite is working.
