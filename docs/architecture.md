# Architecture

## The problem

We want to deeply research a queue of topics without supervision. A single LLM pass gives you a summary, not understanding. We need sourced evidence, credibility distinctions, bias checking, and validation. The output should be something you can read and actually trust the claims in.

## High-level design

The system is a CLI that processes a queue of topics through a multi-agent pipeline. Each topic goes through six phases, and the output is a directory of structured markdown files with cited sources.

```
queue.yaml -> [pipeline per topic] -> output/{topic-id}/
```

The pipeline per topic:

```
research (3 agents, parallel)
    -> synthesis (1 agent)
    -> validation (4 agents, parallel)
    -> triage (1 agent)
    -> revision (1 agent)
    -> verify (1 agent)
    -> done
```

11 agent invocations per topic. Each agent is a `claude -p` subprocess with a specific prompt, model, and tool set.

Each phase has exactly one job, and each phase's output format is a hard contract that the next phase trusts. The data contracts — what each phase consumes, what it produces, what structure is required — are specified in [`pipeline-contracts.md`](pipeline-contracts.md). That document is the source of truth; prompts and code enforce it.

The shape of the pipeline is deliberate: validation produces critique, triage merges and prioritizes, revision applies fixes mechanically, verify checks the fixes landed. Separating these jobs prevents the failure mode where a single "revision" agent conflates triage with application and silently drops findings.

## Module breakdown

8 source files, ~3000 lines total (including tests).

### `main.rs` (275 lines)
CLI entry point. Clap-based subcommands (`run`, `add`, `status`, `recover`, `init`, `preflight`, `remove`). Resolves config paths, filters pending topics, constructs the `WorkerPool`, and displays results.

Owns the `Config -> Arc<Config>` lifecycle. The CLI `--model` flag overrides the config's default model before the Arc wrap. `prompts_dir` is resolved from relative to absolute here and stored back into the config.

### `config.rs` (188 lines)
`Config` struct with serde defaults. All pipeline settings live here:

- Global defaults: `model`, `max_turns`, `agent_timeout`, `max_concurrent_topics`
- Per-agent overrides: `agents` HashMap keyed by agent name
- Path settings: `output_dir`, `queue_file`, `prompts_dir`
- Cost: `max_cost_per_topic`

Three resolver methods: `model_for(name)`, `max_turns_for(name)`, `timeout_for(name)`. Each checks the per-agent override first, falls back to the global default.

Also holds named constants for display values (`TOPIC_PREVIEW_LEN`, `SLUG_MAX_LEN`, etc.) that were previously magic numbers.

### `agent.rs` (423 lines)
The agent invocation layer. Three things here:

1. **`AgentRunner` trait** - The injectable interface. Takes an `AgentConfig`, returns an `AgentResult`. The pipeline calls this, never `claude` directly.
2. **`ClaudeRunner`** - Production implementation. Builds a `claude -p` command with the config's model, max turns, allowed tools, and timeout. Parses usage/cost from the JSON response. Falls back to extracting output from stdout if the agent doesn't write its output file.
3. **`AgentConfig` constructors** - `research()`, `synthesis()`, `validator()`, `triage()`, `revision()`, `verify()`. Each sets the appropriate tool list for that agent type (research gets WebSearch, triage/revision get Read+Write only, verify gets Read only, etc.).

`AgentResult` carries: success/failure, duration, error message, `AgentUsage` (tokens + cost), and the raw JSON response.

### `queue.rs` (1055 lines, ~500 are tests)
Queue and topic state management. Two file contracts:

- `queue.yaml`: The inbox. Topics are here while pending, removed on completion or failure.
- `output/{topic-id}/meta.yaml`: Per-topic state. Created when a topic is claimed, updated after each agent, persists after completion.

All queue file operations go through `with_queue_lock` (shared read) or `with_queue_lock_mut` (exclusive read-modify-write) using `fs2` advisory locks. All file writes are atomic (write to `.tmp`, then rename).

Key operations:
- `claim_topic`: Creates meta.yaml with status "researching"
- `record_agent_result`: Appends agent status, duration, tokens, cost to meta
- `complete_topic`: Sets status "done", removes from queue
- `fail_topic`: Sets status "failed", removes from queue
- `recover_failed`: Scans output dir for failed/interrupted topics, resets meta to "pending", re-adds to queue
- `get_all_statuses`: Reads all metas, categorizes into pending/in-progress/done/failed

### `pipeline.rs` (844 lines, ~400 are tests)
The per-topic pipeline and the cross-topic worker pool.

**`TopicPipeline`**: Runs the six phases for a single topic. Holds `Arc<Config>`, `Arc<dyn AgentRunner>`, and `Arc<Mutex<QueueManager>>`. Each phase:

1. Checks if output files already exist (resume logic)
2. Loads prompt from disk, substitutes `{topic}`, `{research_dir}`, etc.
3. Resolves model/turns/timeout from per-agent config
4. Runs agent(s) via the `AgentRunner` trait
5. Records results to meta.yaml
6. Saves raw JSON response to `responses/`

Research and validation phases use `futures::future::join_all` to run agents concurrently. Synthesis, triage, revision, and verify are sequential (each depends on the prior phase's output).

Cost guardrail: `check_cost_limit()` runs between each phase. Reads cumulative cost from meta.yaml and bails if it exceeds `max_cost_per_topic`.

**`WorkerPool`**: Owns `Arc<Config>` and the output directory. Spawns one `tokio::spawn` task per topic, bounded by a semaphore (`max_concurrent_topics`). Each task creates a `TopicPipeline` and runs it.

### `roster.rs`
Single source of truth for the agent definitions:

- `RESEARCH_AGENTS`: 3 entries (academic, expert, general)
- `VALIDATION_AGENTS`: 4 entries (bias, sources, claims, completeness)
- `SYNTHESIS_PROMPT`, `TRIAGE_PROMPT`, `REVISION_PROMPT`, `VERIFY_PROMPT`: Filenames
- `all_prompt_files()`: Returns all 11 prompt filenames (used by preflight and tests)

Each `AgentDef` has: name, prompt file, output file, needs_web flag.

### `preflight.rs`
Pre-run validation. Checks: claude CLI installed, auth works (quick test invocation), config valid, queue parseable, no duplicate IDs, all 11 prompt files exist, output dir writable. Suggests `init` if prompts are missing.

### `init.rs`
Project scaffolding. All 11 prompts are embedded in the binary via `include_str!`. `init` extracts them to a `prompts/` directory, creates `config.yaml` with documented defaults, `queue.yaml`, and `output/`. Skips existing files unless `--force` is passed.

## Data flow

### Per-topic lifecycle

```
1. User adds topic to queue.yaml (via `add` command or direct edit)
2. `run` reads queue, filters out done/failed topics
3. WorkerPool spawns a task per topic (semaphore-bounded)
4. TopicPipeline.claim_topic() creates output/{id}/meta.yaml, status: "researching"
5. Research phase:
   - For each agent: check if output file exists (skip if cached)
   - Run uncached agents in parallel via join_all
   - Record results to meta.yaml
   - Check cost limit
6. Synthesis phase:
   - Check if overview.md exists (skip if cached)
   - Run synthesizer
   - Record result
   - Check cost limit
7. Validation phase:
   - Same pattern: skip cached, run uncached in parallel
   - Count successes. If <2/4 succeed, bail. 2 or 3 is a warning.
   - Check cost limit
8. Triage phase:
   - Check if triage.md exists (skip if cached)
   - Run triage agent on the 4 validator reports
   - Must account for every validator finding (action list + discarded)
   - If failed, bail
9. Revision phase:
   - Check if overview_final.md exists (skip if cached)
   - Run revision agent on overview.md + triage.md (NOT validator reports)
   - If failed, bail
10. Verify phase:
    - Check if verify.md exists (skip if cached)
    - Run verify agent (Haiku, mechanical checks only)
    - On PASS: complete_topic()
    - On FAIL: mark topic `needs_human_review: true` in meta.yaml, do NOT auto-retry
11. complete_topic(): status -> "done", remove from queue.yaml
```

If any phase bails, `fail_topic()` records the error in meta.yaml and removes from queue. `recover` is the only way to re-queue.

### File layout per topic

```
output/{topic-id}/
  meta.yaml                # state: status, timestamps, per-agent results, needs_human_review flag
  overview.md              # synthesis output
  triage.md                # triage action list
  overview_final.md        # revised synthesis + ledger
  verify.md                # verifier report
  research/
    academic.md
    expert.md
    general.md
  validation/
    bias.md
    sources.md
    claims.md
    completeness.md
  responses/               # raw JSON from each claude invocation
    research_academic.json
    research_expert.json
    research_general.json
    synthesis.json
    validate_bias.json
    validate_sources.json
    validate_claims.json
    validate_completeness.json
    triage.json
    revision.json
    verify.json
  sources/                 # reserved for future per-source files
```

See [`pipeline-contracts.md`](pipeline-contracts.md) for the format of each file.

## Key design decisions

### Queue as inbox, meta as state

We split "what needs processing" from "what happened." `queue.yaml` is the inbox (add topics, they disappear when processed). `meta.yaml` is the permanent record. This means:

- Queue stays clean (no accumulation of done topics)
- State survives even if queue.yaml is corrupted
- `status` reads from meta, not queue
- `recover` reads from meta, writes to queue

### Resume via file existence

The pipeline checks if each agent's output file exists (with content) before running it. This makes `run` idempotent. If 2 of 3 research agents finished before a crash, only the third re-runs. No special "checkpoint" system, just files on disk.

The tradeoff: there's no way to force-rerun a specific agent without deleting its output file. We chose simplicity over fine-grained control.

### Atomic writes everywhere

Every write to a state file (meta.yaml, queue.yaml) and every raw response save goes through `atomic_write()`: write to a `.tmp` sibling, then `rename()`. POSIX `rename` on the same filesystem is atomic. If the process dies mid-write, the original file is untouched.

### AgentRunner trait for testability

The pipeline never calls `claude` directly. It calls `runner.run_agent(config)` through the `AgentRunner` trait. Tests use `FakeRunner` (all succeed or all fail) and `SelectiveFakeRunner` (fail specific agents by name). Both write real files to disk, so the resume logic is tested against the same file-existence checks the production code uses.

### Per-agent config resolution

The pipeline resolves model, max_turns, and timeout per agent name. If `config.yaml` has an override for that agent, use it. Otherwise fall back to the global default. This lets you use opus for synthesis (where quality matters most) and sonnet for validators (where speed matters more).

### Fail removes from queue, recover re-adds

There's one path to retry a failed topic: `recover`. `fail_topic` removes the topic from queue.yaml, so `run` won't auto-retry. This is intentional. A topic that failed due to a bad prompt would just fail again. `recover` resets the meta to "pending" and re-adds to queue, but preserves existing agent outputs so the resume logic can reuse them.

### File locking

Queue file operations use `fs2` advisory locks (`flock`). Shared lock for reads, exclusive lock for writes. This protects against concurrent `add` and `run` from different terminals. It's cooperative locking, so direct file edits (vim, etc.) aren't protected.

## Known limitations

Things we've explicitly decided not to address yet:

- **Status strings are untyped.** `"researching"`, `"done"`, `"failed"` etc. are string literals, not an enum. Adding a pipeline phase means updating match arms in multiple files manually. A `TopicStatus` enum would make the compiler catch missed updates.
- **Config path resolution is inconsistent.** `prompts_dir` is resolved to absolute in main.rs before the Arc wrap. `output_dir` and `queue_file` aren't. A `ResolvedConfig` type would make this correct by construction.
- **`allowed_tools` is split between roster.rs and agent.rs.** The roster has a `needs_web` flag, but the actual tool lists are hardcoded in the `AgentConfig` constructors. Completing the centralization would put tool sets in the roster.
- **No rate limit handling.** If claude returns a rate limit error, the agent fails and the topic can be recovered. There's no backoff or retry within a run.
- **`init.rs` prompt list is separate from the roster.** The `include_str!` embeds must be compile-time constants, so they can't reference the roster dynamically. The roster's `all_prompt_files()` is used everywhere else, but init has its own list.
- **The `AgentRunner` trait is claude-specific in practice.** The interface is model-agnostic (`AgentConfig` in, `AgentResult` out), but `AgentConfig` contains `allowed_tools` which is a Claude Code concept. Swapping to a different LLM provider would require either ignoring that field or mapping it.

## Concurrency model

Two levels of parallelism:

1. **Cross-topic**: `WorkerPool` spawns one `tokio::spawn` task per topic, bounded by a semaphore. Default is 2 concurrent topics.
2. **Within-topic**: Research agents (3) and validation agents (4) run concurrently via `futures::future::join_all`. Synthesis and revision are sequential.

`QueueManager` is wrapped in `Arc<Mutex<QueueManager>>` to allow cross-topic parallelism. The mutex is held briefly for each meta.yaml read/write, not for the duration of an agent invocation.

## Cost tracking

Every agent invocation records token usage and cost in meta.yaml (parsed from the claude CLI's JSON response). Fields: `input_tokens`, `output_tokens`, `cache_creation_tokens`, `cache_read_tokens`, `cost_usd`.

`check_cost_limit()` runs between each pipeline phase. It reads the cumulative cost from meta.yaml and bails if it exceeds `max_cost_per_topic` (0 = no limit).

The raw JSON responses are saved to `responses/` for analytics beyond what we extract into meta.yaml.

Note: if you're on a Claude Pro/Max subscription (authenticated via `claude auth login`), the `cost_usd` values are informational, not actual charges. They represent what the equivalent API cost would be. If you have `ANTHROPIC_API_KEY` set, those are real charges.
