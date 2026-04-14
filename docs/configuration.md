# Configuration

## config.yaml

All fields are optional. Defaults are applied for anything not specified. `init` creates a config file with all defaults documented and a recommended per-agent override block.

```yaml
# how many topics to process concurrently
max_concurrent_topics: 2

# default timeout per agent invocation (seconds)
agent_timeout: 3600

# default claude model for all agents
model: "sonnet"

# default max conversation turns per agent
max_turns: 25

# max USD per topic before the pipeline bails (0 = no limit)
max_cost_per_topic: 0

# paths (relative to project root)
output_dir: "output"
queue_file: "queue.yaml"
prompts_dir: "prompts"

# per-agent overrides (optional — see recommended block below)
agents:
  synthesizer:
    model: "opus"
  validate_sources:
    max_turns: 35
    timeout: 900
    max_web_tool_calls: 25
  validate_completeness:
    model: "haiku"
  triage:
    model: "opus"
  verify:
    model: "haiku"
```

## Global defaults

| Field | Default | What it controls |
|---|---|---|
| `max_concurrent_topics` | 2 | How many topics process in parallel (semaphore-bounded) |
| `max_concurrent_agents` | 4 | Max agent invocations running at once, across all topics |
| `agent_timeout` | 600 | Seconds before an agent subprocess is killed |
| `model` | `"sonnet"` | Claude model name passed to `claude -p --model` |
| `max_turns` | 25 | Max conversation turns per agent (`--max-turns`) |
| `max_cost_per_topic` | 0 | USD limit per topic (0 disables the check) |
| `output_dir` | `"output"` | Where topic results go |
| `queue_file` | `"queue.yaml"` | The topic inbox file |
| `prompts_dir` | `"prompts"` | Where agent prompts live |

Note: the `init` template writes `agent_timeout: 3600` rather than the 600s code default. 600s is too aggressive for the research agents (they legitimately run multi-minute web search sessions); 3600s gives headroom with per-agent overrides (like `validate_sources: 900`) to tighten the backstop where hangs have been observed.

## Per-agent overrides

The pipeline has 11 agents across 6 phases. The `agents` section lets you override `model`, `max_turns`, `timeout`, and `max_web_tool_calls` per agent name. Unset fields fall back to the global defaults above.

| Name | Phase | What it does |
|---|---|---|
| `research_academic` | Research | Peer-reviewed papers, systematic reviews, meta-analyses |
| `research_expert` | Research | Credentialed experts and institutional positions |
| `research_general` | Research | Journalism, practitioner writing, community discussion |
| `synthesizer` | Synthesis | Combines the three research files into a structured overview |
| `validate_bias` | Validation | Directional unfairness: framing, source imbalance, false balance |
| `validate_sources` | Validation | Source existence, attribution accuracy, tier inflation, origin-tag consistency |
| `validate_claims` | Validation | Claim-to-source fit, overstatement, cherry-picking, unmarked inference |
| `validate_completeness` | Validation | Neutral coverage gaps — content in research files not in synthesis |
| `triage` | Triage | Merges, prioritizes, and assigns severity to validator findings |
| `revision` | Revision | Mechanically applies triage's action list to produce the final document |
| `verify` | Verify | Runs six mechanical checks on the final document and the revision ledger |

## Override fields

### `model`

One of `opus`, `sonnet`, `haiku`, or a full model name (e.g. `claude-opus-4-6`). Synthesis and triage benefit from Opus — they are the hardest reasoning in the pipeline. Validate_completeness and verify do mechanical work and run fine on Haiku. Research and most validators run on Sonnet.

```yaml
agents:
  synthesizer:
    model: "opus"
  triage:
    model: "opus"
  validate_completeness:
    model: "haiku"
  verify:
    model: "haiku"
```

### `max_turns`

How many conversation turns an agent gets before the CLI kills it. Default 25 is enough for most agents. `validate_sources` is recommended to use `max_turns: 35` because prioritization plus web tool calls plus report writing can push it past 25 on topics with many sources.

```yaml
agents:
  validate_sources:
    max_turns: 35
```

### `timeout`

Wall-clock timeout in seconds for the agent subprocess. Default comes from `agent_timeout`. `validate_sources` is recommended to use `timeout: 900` (15 minutes) — past runs showed it occasionally hangs indefinitely, and a 15-minute backstop fails fast without sacrificing the happy case (typical runs complete in 3-5 minutes).

```yaml
agents:
  validate_sources:
    timeout: 900
```

### `max_web_tool_calls`

The combined cap on `WebSearch` and `WebFetch` calls for this agent. Only `validate_sources` uses it today; other agents ignore the field. The default is 25 if unset. Observed usage in real runs is 4-20 WebSearch calls per validation run, so 25 provides comfortable headroom.

This is a hard budget, not a sample size. The validator's prompt instructs it to prioritize which sources to verify (load-bearing citations, aggressive tier assignments, sources cited in the executive summary) rather than randomly sampling. Lower the cap if you want tighter spending; raise it for topics with many sources.

```yaml
agents:
  validate_sources:
    max_web_tool_calls: 25
```

## Recommended config block

This is what `init` lays down and what's known to work well in practice:

```yaml
agents:
  synthesizer:
    model: "opus"
  validate_sources:
    max_turns: 35
    timeout: 900
    max_web_tool_calls: 25
  validate_completeness:
    model: "haiku"
  triage:
    model: "opus"
  verify:
    model: "haiku"
```

Unmentioned agents use the global defaults (`sonnet`, `max_turns: 25`, `agent_timeout: 3600`). This block trades a small amount of extra cost (Opus for synthesis and triage) for substantial quality improvements in the reasoning-heavy phases, while keeping the mechanical phases on Haiku to save.

## Cost guardrails

`max_cost_per_topic` is checked between each pipeline phase (after research, after synthesis, after validation, after triage, after revision). If cumulative cost (summed from all agents' `cost_usd` in meta.yaml) exceeds the limit, the pipeline bails and the topic is marked failed.

The check happens between phases, not within a phase. A single expensive agent won't be stopped mid-run, but the next phase won't start if the budget is blown. Setting this to 0 (the default) disables the check entirely.

Note on billing: if you're on a Claude Pro/Max subscription (authenticated via `claude auth login`), the cost values are informational — they represent equivalent API pricing. If you have `ANTHROPIC_API_KEY` set, those are real API charges.

## queue.yaml

The topic inbox. Two ways to add topics:

### Via CLI

```bash
agentic-research add "Intermittent fasting for longevity"
agentic-research add --id crispr-ethics "CRISPR gene editing ethics"
```

### Via direct edit

```yaml
topics:
  - id: intermittent-fasting
    input: "Intermittent fasting for longevity"

  - id: crispr-ethics
    input: |
      CRISPR gene editing ethics and regulation.
      Focus on: germline vs somatic editing distinction,
      international regulatory landscape post-2022,
      disability rights community perspectives.
```

Each topic needs:

- `id` — used as the output directory name. Auto-generated from input text if not specified. Keep it filesystem-friendly (no slashes, reasonable length).
- `input` — the topic description. Can be a one-liner or a multi-line block with scoping hints.

Topics are removed from the queue when they complete, fail, or are marked needs_review. `status` shows all topics regardless of terminal state.
