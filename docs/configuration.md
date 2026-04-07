# Configuration

## config.yaml

All fields are optional. Defaults are applied for anything not specified. `init` creates a config file with all defaults documented.

```yaml
# how many topics to process concurrently
max_concurrent_topics: 2

# default timeout per agent invocation (seconds)
agent_timeout: 600

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

# per-agent overrides (optional)
agents:
  synthesizer:
    model: "opus"
    max_turns: 30
  revision:
    model: "opus"
  validate_sources:
    timeout: 900
```

## Global defaults

| Field | Default | What it controls |
|---|---|---|
| `max_concurrent_topics` | 2 | How many topics process in parallel (semaphore-bounded) |
| `agent_timeout` | 600 | Seconds before an agent subprocess is killed |
| `model` | "sonnet" | Claude model name passed to `claude -p --model` |
| `max_turns` | 25 | Max conversation turns per agent (`--max-turns`) |
| `max_cost_per_topic` | 0 | USD limit per topic (0 disables the check) |
| `output_dir` | "output" | Where topic results go |
| `queue_file` | "queue.yaml" | The topic inbox file |
| `prompts_dir` | "prompts" | Where agent prompts live |

## Per-agent overrides

The `agents` section lets you override `model`, `max_turns`, and `timeout` per agent name. Agent names:

| Name | Phase | What it does |
|---|---|---|
| `research_academic` | Research | Academic papers and peer-reviewed sources |
| `research_expert` | Research | Expert opinions and institutional positions |
| `research_general` | Research | Journalism, community discussion, practitioner perspectives |
| `synthesizer` | Synthesis | Combines research into unified overview |
| `validate_bias` | Validation | Checks for framing, selection, and balance issues |
| `validate_sources` | Validation | Verifies sources exist and are accurately represented |
| `validate_claims` | Validation | Cross-references claims against evidence |
| `validate_completeness` | Validation | Identifies coverage gaps |
| `revision` | Revision | Incorporates validation feedback into final output |

Each override field is optional. If not specified, the global default applies.

### When to use per-agent overrides

**Model overrides** are the most useful. Synthesis and revision benefit from a stronger model (they need to reason across multiple sources). Research and validation can use a faster model since they're doing more mechanical work.

```yaml
agents:
  synthesizer:
    model: "opus"
  revision:
    model: "opus"
```

**Timeout overrides** are useful for agents that do web fetching. `validate_sources` verifies URLs, which can be slow.

```yaml
agents:
  validate_sources:
    timeout: 900
```

**Max turns overrides** are useful if a specific agent keeps running out of turns on complex topics.

```yaml
agents:
  research_academic:
    max_turns: 35
```

## Cost guardrails

`max_cost_per_topic` is checked between each pipeline phase (after research, after synthesis, after validation). If cumulative cost (summed from all agents' `cost_usd` in meta.yaml) exceeds the limit, the pipeline bails.

The check happens between phases, not within a phase. If a single agent is expensive, the pipeline won't stop it mid-run. But it won't start the next phase if the budget is blown.

Setting this to 0 (the default) disables the check entirely.

Note on billing: if you're on a Claude Pro/Max subscription (authenticated via `claude auth login`), the cost values are informational. They represent equivalent API pricing. You're not charged separately. If you have `ANTHROPIC_API_KEY` set, those are real API charges.

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
- `id`: Used as the output directory name. Auto-generated from input text if not specified. Keep it filesystem-friendly (no slashes, reasonable length).
- `input`: The topic description. Can be a one-liner or a multi-line block with scoping hints.

Topics are removed from the queue when they complete or fail. `status` shows all topics (queued and processed).
