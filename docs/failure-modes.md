# Failure Modes and Recovery

## The design principle

Re-running should always be safe. The pipeline is idempotent. If something already exists on disk with content, we don't redo it. If something failed, we record what failed and let you decide when to retry.

## What can go wrong

### Agent times out

Each agent invocation has a configurable timeout (`agent_timeout` in config, per-agent override via `agents.{name}.timeout`). If an agent exceeds its timeout, the subprocess is killed and the result is recorded as failed in meta.yaml with the error "Timed out after Ns."

**Recovery**: The pipeline continues if at least one research agent succeeded (or was cached). For validation, partial failures are allowed (warns but proceeds to revision). For synthesis and revision, a failure stops the pipeline and the topic is marked failed.

Re-running after a timeout: `recover` then `run`. The agents that completed successfully are cached and won't re-run.

### Agent produces bad output

The agent might write a malformed file, an incomplete analysis, or just not follow the prompt well. The pipeline doesn't validate output quality (it checks existence and non-emptiness, nothing more).

**Recovery**: Delete the specific output file you want to redo (e.g., `rm output/my-topic/research/academic.md`) and re-run. The pipeline will see it's missing and re-run that agent. Everything else stays cached.

### Process crashes mid-pipeline

All state file writes (meta.yaml, queue.yaml) use atomic writes (write to `.tmp`, then `rename`). If the process dies mid-write, the original file is untouched. The `.tmp` file is stale and ignored.

Agent output files are written by the claude subprocess, not by our code (the agent uses the Write tool). If the subprocess dies mid-write, the output file might be partial. On resume, the pipeline sees a non-empty file and treats it as cached. To fix: delete the partial file and re-run.

**Recovery**: `recover` scans for topics with in-progress status (`researching`, `synthesizing`, etc.) and resets them to `pending`. Then `run` picks them up. Cached outputs are reused.

### Cost limit exceeded

If `max_cost_per_topic` is set and cumulative cost exceeds it between phases, the pipeline bails with "Cost limit exceeded: $X.XX spent, limit is $Y.YY." The topic is marked failed.

**Recovery**: Either increase the limit in config and `recover` + `run`, or accept the partial results (research and synthesis might be complete even if validation didn't run).

### All research agents fail

If all 3 research agents fail and there are no cached research outputs, the pipeline bails at the research phase. This can happen due to rate limits, auth issues, or network problems.

**Recovery**: Fix the underlying issue, then `recover` + `run`.

### All validators fail

If all 4 validators fail, the pipeline bails. The synthesis (overview.md) is still intact. Partial validator failure (1-3 out of 4) logs a warning and proceeds to revision with whatever validation feedback is available.

**Recovery**: Same as above. Or if validation isn't important for a particular topic, you can manually read overview.md as your output.

### Queue file corruption

`queue.yaml` is protected by advisory file locks and atomic writes. If it still gets corrupted (manual edit gone wrong, filesystem issue), the pipeline will fail with a parse error instead of silently treating it as empty.

**Recovery**: Fix the YAML syntax, or delete and recreate with `init` (you'll need to re-add your topics). Topic state in `output/*/meta.yaml` is independent of the queue file.

### Rate limiting

The Claude CLI may hit rate limits, especially with multiple parallel agents. Rate limit errors come back as agent failures (non-zero exit code from the subprocess).

**Recovery**: The pipeline records the failure and moves on. `recover` + `run` retries, with cached outputs reused. If rate limits are persistent, reduce `max_concurrent_topics` to 1 and let agents within a topic run sequentially (not currently configurable, would require code change).

## The recovery flow

```
# see what happened
agentic-research status -v

# re-queue failed and interrupted topics
agentic-research recover

# run again (cached outputs are reused)
agentic-research run
```

`recover` does three things:
1. Scans `output/*/meta.yaml` for topics with status `failed`, `researching`, `synthesizing`, `validating`, or `revising`
2. Resets their meta to `pending` (but preserves agent results, so cached outputs are reused)
3. Re-adds them to `queue.yaml` if not already present

## Manual intervention

Sometimes you want to redo a specific part without recovering the whole topic:

- **Redo one research agent**: `rm output/my-topic/research/expert.md` then `run`
- **Redo synthesis**: `rm output/my-topic/overview.md` then `run`
- **Redo all validation**: `rm output/my-topic/validation/*.md` then `run`
- **Redo revision**: `rm output/my-topic/overview_final.md` then `run`
- **Redo everything for a topic**: `rm -rf output/my-topic` then `recover` + `run`
