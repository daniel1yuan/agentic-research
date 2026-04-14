# agentic-research

Automated deep research pipeline. You give it a queue of topics, it runs a multi-agent pipeline against each one, and you get back a structured corpus of findings with cited sources and validation.

The goal is unsupervised, grounded research. Not summaries from a single model pass, but actual sourced evidence with credibility tiers, expert vs non-expert distinctions, and bias checking.

## Prerequisites

- [Claude Code CLI](https://claude.ai/download) installed and authenticated
- Rust toolchain (for building from source)

## Getting started

### Build from source

```bash
git clone <repo-url>
cd agentic-research
cargo build --release
```

The binary is at `target/release/agentic-research`. Copy it wherever you want.

### Set up a project directory

The binary is self-contained. All prompts and config templates are embedded in it. To set up a new research project:

```bash
mkdir my-research && cd my-research
agentic-research init
```

This creates:

```
my-research/
  config.yaml       # settings (model, timeout, concurrency)
  queue.yaml         # topic inbox
  prompts/           # 11 agent prompt files (editable)
  output/            # research results land here
```

You can run `init` again safely. It won't overwrite existing files unless you pass `--force`.

### Quick start

```bash
# 1. set up
agentic-research init

# 2. add topics
agentic-research add "Intermittent fasting for longevity"
agentic-research add --id crispr-ethics "CRISPR gene editing ethics"

# 3. validate setup
agentic-research preflight

# 4. run
agentic-research run
```

## How it works

Each topic goes through a six-phase pipeline:

1. **Research** (3 agents in parallel)
   - Academic: papers, systematic reviews, meta-analyses
   - Expert: institutional positions, named authorities, credentialed commentary
   - General: journalism, community discussion, practitioner perspectives

2. **Synthesis** (1 agent)
   - Reads all research outputs and produces a structured overview
   - Organized by claim/sub-topic, not by source
   - Every factual claim cites its source with a credibility tier

3. **Validation** (4 agents in parallel)
   - Bias check: framing, selection, false balance, omission
   - Source validation: do the cited sources actually exist and say what we claim?
   - Claim validation: are the key claims supported by the evidence?
   - Completeness: are there obvious gaps, missing perspectives, or underrepresented stakeholders?

4. **Triage** (1 agent)
   - Reads all four validator reports, merges and prioritizes findings
   - Emits a single structured action list for the revision agent

5. **Revision** (1 agent)
   - Reads the triage action list and produces a final revised synthesis
   - Addresses major issues, adds caveats, fills gaps where possible

6. **Verify** (1 agent)
   - Mechanical pass/fail checks against the revised synthesis
   - Validates structure, citation tags, section format, document length

Total: 11 agent invocations per topic (3 + 1 + 4 + 1 + 1 + 1).

## Source credibility tiers

Every source gets labeled:

- **Tier 1**: Peer-reviewed papers, systematic reviews, meta-analyses
- **Tier 2**: Expert commentary, institutional reports, domain authorities
- **Tier 3**: Quality journalism, industry reports, well-sourced blog posts
- **Tier 4**: Forum discussions, opinion pieces, social media (included but clearly labeled as non-expert)

Non-expert opinions are valid data points. They're just not presented as expert opinions.

## Output structure

```
output/{topic-id}/
  overview.md            # first-pass synthesis
  overview_final.md      # revised synthesis (after validation feedback)
  meta.yaml              # pipeline state, per-agent durations/cost/tokens, errors
  research/
    academic.md
    expert.md
    general.md
  validation/
    bias.md
    sources.md
    claims.md
    completeness.md
  responses/             # raw JSON from each claude invocation (for analytics)
    research_academic.json
    synthesis.json
    ...
  sources/               # (reserved for individual source files)
```

## Commands

| Command | What it does |
|---|---|
| `init` | Scaffold a new project directory |
| `init --force` | Same, but overwrites existing files |
| `add "topic"` | Add a topic to the queue |
| `add --id custom-id "topic"` | Add with a custom ID |
| `run` | Preflight checks, then process all pending topics |
| `run --skip-preflight` | Skip validation |
| `run --model opus` | Override model |
| `status` | Show all topics by state (pending, in-progress, done, failed) |
| `status -v` | Same with per-agent cost/token/duration details |
| `recover` | Re-queue failed and interrupted topics for retry |
| `preflight` | Validate setup without running |
| `remove <id>` | Remove a topic from the queue |

## Topic format

Topics can be simple one-liners or multi-line with scoping hints. The `input` field in `queue.yaml` accepts block scalars:

```yaml
topics:
  - id: crispr-ethics
    input: |
      CRISPR gene editing ethics and regulation.
      Focus on: germline vs somatic editing distinction,
      international regulatory landscape post-2022,
      disability rights community perspectives.
```

## Cost tracking

Every agent invocation records its token usage and cost in `meta.yaml`:

```yaml
agents:
  research_academic:
    status: done
    duration: 180.2s
    input_tokens: 3000
    output_tokens: 1200
    cache_creation_tokens: 17000
    cost_usd: 0.15
```

`status` shows per-topic totals. `status -v` breaks it down per agent. The raw JSON responses are saved to `responses/` if you want to run your own analytics.

## Failure handling

There are a few things that can go wrong, and the pipeline handles each of them:

- **Agent timeout or failure**: If some research agents fail but at least one succeeds, the pipeline continues. Failed agents are recorded in `meta.yaml` with the error.
- **Process crash mid-pipeline**: Agent outputs are written to disk as they complete. On re-run, the pipeline checks what already exists and skips those agents. If 2 of 3 research agents finished before a crash, only the third re-runs.
- **`recover` command**: Scans for topics with `failed` or interrupted status (stuck in `researching`, `synthesizing`, etc.), resets them to `pending`, and re-adds them to the queue. Existing agent outputs are preserved for reuse.

The design principle: re-running should always be safe. If something already exists on disk with content, we don't redo it.

## Queue management

`queue.yaml` is the inbox. Topics get added there, processed, and removed when done. State for each topic lives in `output/{topic-id}/meta.yaml` alongside its research outputs.

Queue file access uses advisory file locks (`flock`) so concurrent `add` and `run` commands from different terminal sessions don't clobber each other. This is cooperative locking, so editing the yaml in your editor while a run is active isn't protected (just don't do that).

## Configuration

`config.yaml` in the project root. All fields are optional with defaults:

```yaml
max_concurrent_topics: 2   # not yet active (topics run sequentially, agents within a topic run in parallel)
agent_timeout: 600          # seconds per agent invocation
model: "sonnet"             # claude model (sonnet, opus, haiku)
output_dir: "output"
queue_file: "queue.yaml"
```

## Prompts

The quality of the research comes from the prompts in `prompts/`. Each one has specific instructions for:

- What sources to look for and how to evaluate them
- Output format and structure requirements
- Citation discipline and credibility labeling
- How to handle uncertainty and conflicting evidence

These are the most important files in the project. If you want to tune the research quality, that's where to look. `init` writes the defaults. You can edit them freely. `init --force` resets them back to defaults.

## Documentation

Detailed docs in `docs/`:

- **[Architecture](docs/architecture.md)** - Module breakdown, data flow, design decisions, concurrency model, known limitations
- **[Prompts](docs/prompts.md)** - How the 9 prompts work together, credibility tiers, editing guide
- **[Configuration](docs/configuration.md)** - All config fields, per-agent overrides, cost guardrails, queue format
- **[Failure Modes](docs/failure-modes.md)** - What can go wrong, recovery procedures, manual intervention

## Tests

```bash
cargo test
```
