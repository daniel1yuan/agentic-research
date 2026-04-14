# Failure Modes and Recovery

## The design principle

Re-running should always be safe. The pipeline is idempotent. If something has already completed cleanly on disk, we don't redo it. If something failed or needs human attention, we record what happened and let you decide when to retry.

## Terminal states

A topic ends in one of three states. Each is final until you decide to re-run it.

- **`done`** — All six phases completed, the verifier passed all six mechanical checks, and the topic is removed from the queue.
- **`needs_review`** — The pipeline reached the verify phase cleanly and the verifier flagged the final document as failing one or more checks (missing fixes, missing origin tags, structural drift, length drift, missing inference rationales). The topic is removed from the queue but is marked in `meta.yaml` with a human-readable reason in the `error` field. Re-running requires `recover` — the pipeline does NOT auto-retry, because verify failures often indicate prompt-level issues that a blind re-run won't fix.
- **`failed`** — An agent crashed, timed out, or a phase bailed (validation below the 2-of-4 threshold, synthesis failure, revision failure, etc.). The topic is removed from the queue. Re-running requires `recover`.

`needs_review` is distinct from `failed`. A failed topic is broken — an agent crashed, the network went down, auth expired. A needs-review topic completed cleanly but didn't pass quality checks — the final document exists and is readable, it just isn't certified correct.

## What can go wrong

### Agent times out

Each agent invocation has a configurable timeout (`agent_timeout` in config, per-agent override via `agents.{name}.timeout`). If an agent exceeds its timeout, the subprocess is killed and the result is recorded as failed in meta.yaml with the error "Timed out after Ns."

Recommended per-agent override: `validate_sources` uses `timeout: 900` (15 minutes) instead of the global default. Past runs showed this validator can hang indefinitely if its internal retry or web-fetch behavior stalls — failing fast at 15 minutes avoids burning an hour on one agent.

**Recovery:** The pipeline continues if at least one research agent succeeded (or was cached). For validation, partial failure is allowed as long as at least 2 of 4 validators succeed. For synthesis, triage, revision, and verify, a failure stops the pipeline and the topic is marked failed.

`recover` then `run` — agents that completed successfully are cached and won't re-run.

### Agent produces bad output (partial write)

Agent output files are written by the claude subprocess, not by our code (the agent uses the Write tool). If the subprocess dies mid-write, the output file might be a partial fragment.

The pipeline uses a **`.done` sidecar marker** to distinguish complete outputs from partial ones. After an agent reports success, the pipeline writes an empty `<output>.done` file next to the output. The `agent_output_exists()` check requires both content and the sidecar — a partial file without a sidecar is treated as missing and re-run on the next invocation.

**Recovery:** Automatic. On the next `run`, the pipeline sees the content file has no sidecar and re-runs the agent, overwriting the partial output. If you want to force a re-run of a completed agent, delete both the output and its `.done` sidecar.

### Agent produces bad output (wrong content)

Distinct from partial writes. The agent ran to completion, wrote a sidecar, but the content is wrong — malformed, shallow, off-topic, or just not following the prompt. The sidecar mechanism cannot catch this; the verifier phase will catch some of it (missing sections, untagged claims, missing fix application) but not semantic quality issues.

**Recovery:** Delete the specific output file you want to redo along with its `.done` sidecar (e.g., `rm output/my-topic/research/academic.md output/my-topic/research/academic.md.done`) and re-run. The pipeline will see it's missing and re-run that agent. Everything else stays cached.

### Verifier flags the final document for human review

After revision, the verify phase runs five mechanical checks against `overview_final.md`:
1. Every triage action appears exactly once in the revision ledger (`ledger_completeness`)
2. Every FIX-disposition ledger entry's replacement text is present in the body (`fix_application`)
3. Every citation in sections 2-7 is followed by an origin tag (`origin_tags`)
4. Every `[inference: ...]` tag has a non-empty rationale (`inference_rationales`)
5. The 8-section structure is intact (`structure`)

If any check fails, the verifier emits `overall: fail` in its YAML frontmatter. The pipeline reads the frontmatter, marks the topic as `needs_review`, records the failed checks in `meta.yaml`'s `error` field, and stops. The pipeline does not auto-retry.

If the verifier's frontmatter is malformed, missing, or unparseable, the topic is also marked `needs_review` with a reason like "malformed verify frontmatter" or "verify.md is missing YAML frontmatter". A broken verifier agent cannot silently pass topics — any deviation from the schema is treated as failure.

**Recovery:**
1. Read `output/<topic>/verify.md` to see which checks failed and why. The markdown body contains human-readable details for each check.
2. Decide whether the issue is a genuine quality problem (inspect `overview_final.md`), a triage/revision bug (inspect `triage.md` and the revision ledger), or a verifier false positive (verifier is strict — sometimes wrong).
3. To re-run: `rm output/<topic>/verify.md output/<topic>/verify.md.done` forces just the verify phase to re-run. Or `rm output/<topic>/overview_final.md*` to re-run revision and verify. Or `recover` + `run` to re-queue.

### Process crashes mid-pipeline

All state file writes (meta.yaml, queue.yaml) use atomic writes (write to `.tmp`, then `rename`). If the process dies mid-write, the original file is untouched.

Agent output files are protected by the `.done` sidecar mechanism described above — a crashed subprocess leaves content but no sidecar, and the next run re-invokes that agent.

**Recovery:** `recover` scans for topics with in-progress status (`researching`, `synthesizing`, `validating`, `triaging`, `revising`, `verifying`) and resets them to `pending`. It also re-queues topics in the terminal `failed` and `needs_review` states. Then `run` picks them up. Cached outputs with sidecars are reused; partial outputs without sidecars are re-run.

### Cost limit exceeded

If `max_cost_per_topic` is set and cumulative cost exceeds it between phases, the pipeline bails with "Cost limit exceeded: $X.XX spent, limit is $Y.YY." The topic is marked failed.

The check runs between phases, not within a phase, so a single expensive agent won't be stopped mid-run — but the next phase won't start if the budget is blown.

**Recovery:** Either increase the limit in config and `recover` + `run`, or accept the partial results (early phases may be complete even if later phases didn't run).

### All research agents fail

If all 3 research agents fail and there are no cached research outputs, the pipeline bails at the research phase. This can happen due to rate limits, auth issues, or network problems.

**Recovery:** Fix the underlying issue, then `recover` + `run`.

### Fewer than 2 of 4 validators succeed

If fewer than 2 validators complete successfully, the pipeline bails at the validation phase. Triage working from 0 or 1 validator's findings produces low-signal action lists — the contract is to bail here rather than proceed with a degraded signal.

If exactly 2 or 3 validators succeed, the pipeline logs a warning and proceeds to triage with partial validation. Triage handles whatever findings it receives.

**Recovery:** `recover` + `run`. Failed validators will re-run; successful ones stay cached.

### Triage, revision, or verify phase fails

Each of the three post-validation phases bails hard on agent failure. Unlike validation, these phases have only one agent each — there's no partial-success fallback.

**Recovery:** `recover` + `run`. Upstream phases stay cached via their sidecars, so the re-run only executes from the failing phase onward.

### Queue file corruption

`queue.yaml` is protected by advisory file locks and atomic writes. If it still gets corrupted (manual edit gone wrong, filesystem issue), the pipeline will fail with a parse error instead of silently treating it as empty.

**Recovery:** Fix the YAML syntax, or delete and recreate with `init` (you'll need to re-add your topics). Topic state in `output/*/meta.yaml` is independent of the queue file.

### Rate limiting

The Claude CLI may hit rate limits, especially with multiple parallel agents. Rate limit errors come back as agent failures (non-zero exit code from the subprocess) or, in some cases, as hangs if the CLI retries internally without progress. The per-agent timeout is the backstop for hangs.

**Recovery:** The pipeline records the failure and moves on. `recover` + `run` retries, with cached outputs reused. For persistent rate limit issues, reduce `max_concurrent_topics` or `max_concurrent_agents` in config.

## The recovery flow

```
# see what happened
agentic-research status -v

# re-queue failed, in-progress, and needs_review topics
agentic-research recover

# run again (cached outputs with sidecars are reused)
agentic-research run
```

`recover` does three things:
1. Scans `output/*/meta.yaml` for topics with status `failed`, `needs_review`, `researching`, `synthesizing`, `validating`, `triaging`, `revising`, or `verifying`
2. Resets their meta to `pending` (but preserves agent results, so cached outputs are reused)
3. Re-adds them to `queue.yaml` if not already present

## Manual intervention

Sometimes you want to redo a specific part without recovering the whole topic. Always delete the `.done` sidecar along with the output file, otherwise the cache check will still see a stale sidecar.

- **Redo one research agent:** `rm output/my-topic/research/expert.md output/my-topic/research/expert.md.done` then `run`
- **Redo synthesis:** `rm output/my-topic/overview.md*` then `run`
- **Redo validation:** `rm output/my-topic/validation/*.md output/my-topic/validation/*.md.done` then `run` (or `rm output/my-topic/validation/*` for brevity)
- **Redo triage:** `rm output/my-topic/triage.md*` then `run`
- **Redo revision + verify:** `rm output/my-topic/overview_final.md* output/my-topic/verify.md*` then `run`
- **Redo just verify:** `rm output/my-topic/verify.md*` then `run`
- **Redo everything for a topic:** `rm -rf output/my-topic` then `recover` + `run`

The glob `*.md*` catches both the `.md` and `.md.done` files for a phase; prefer it over deleting them one at a time.
