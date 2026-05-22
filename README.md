# ai-memory

> Long-term memory for AI coding agents. Quit Claude Code mid-task,
> start OpenAI Codex in the same directory, continue without
> re-explaining the architecture, the failed approaches, or the open
> questions.

[![status: v0.2 milestones complete](https://img.shields.io/badge/status-v0.2--complete-green)](docs/ARCHITECTURE.md)
[![Rust](https://img.shields.io/badge/rust-1.95+-blue)](rust-toolchain.toml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

## What it is

LLM coding agents lose all context when a session ends. ai-memory
gives them a shared, persistent wiki: every prompt, tool call, and
decision is captured automatically; when a session ends, the relevant
pages get rewritten as a coherent narrative; when the next agent
starts (Claude Code, Codex, OpenCode, …) it sees a handoff with
"where you left off" already prepended.

The wiki is plain markdown in a git repo — `grep`-able, openable in
Obsidian, backed up with `rsync`. No vector database to babysit, no
`write_note` ceremony, no manual context-loading. The full design is
in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); the influences and
priors are at the [bottom](#influences-and-prior-art).

## Quick start

You need: Docker + an agent CLI (Claude Code, Codex, OpenCode, Cursor,
or anything else that speaks MCP).

```bash
# 1. Generate a bearer token (one-time; save the output).
export TOKEN=$(docker run --rm akitaonrails/ai-memory:latest generate-auth-token)

# 2. Start the server. Replace the API keys (or omit the four
#    -e AI_MEMORY_LLM_* / EMBEDDING_* lines for zero-LLM mode — FTS5
#    search still works without any keys).
docker run -d --name ai-memory \
    -p 49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_AUTH_TOKEN="$TOKEN" \
    -e AI_MEMORY_LLM_PROVIDER=anthropic \
    -e ANTHROPIC_API_KEY=sk-ant-... \
    -e AI_MEMORY_EMBEDDING_PROVIDER=openai \
    -e OPENAI_API_KEY=sk-... \
    akitaonrails/ai-memory:latest

# 3. Wire your agent CLI to it. Claude Code shown below; for Codex,
#    OpenCode, Cursor, Claude Desktop, Gemini CLI, OpenClaw, see
#    docs/install.md.

# 3a. Register the MCP endpoint (auto-edits ~/.claude/settings.json)
docker run --rm -v "$HOME:/host" akitaonrails/ai-memory:latest \
    install-mcp --client claude-code --apply \
        --config-file /host/.claude/settings.json \
        --server-url "http://localhost:49374/mcp" \
        --auth-token "$TOKEN"

# 3b. Extract the bundled hook scripts to your home dir
docker cp ai-memory:/usr/local/share/ai-memory/hooks ~/.ai-memory/

# 3c. Wire the hooks into ~/.claude/settings.json (also auto-edits)
docker run --rm -v "$HOME:/host" akitaonrails/ai-memory:latest \
    install-hooks --agent claude-code --apply \
        --hooks-dir /host/.ai-memory/hooks \
        --config-file /host/.claude/settings.json \
        --server-url "http://localhost:49374" \
        --auth-token "$TOKEN"
```

Both `install-mcp` and `install-hooks` accept `--apply` to **mutate
the agent's config file in place** (idempotent — re-runs replace
ai-memory's entry, preserving every other server / hook the user has
configured; a timestamped `.bak-<ts>` is written next to the file
before each modifying write). Drop `--apply` to keep the legacy
print-the-JSON behaviour.

That's it. Start a Claude Code session as usual — every prompt and
tool call now lands in ai-memory, and the next session you open in
this project will see a handoff with where you left off.

**For everything else** — Codex, OpenCode, Cursor, Claude Desktop,
Gemini CLI, OpenClaw, the curl-based hook installer (no docker
needed), running ai-memory without docker, the full subcommand
reference, the homelab deploy pattern, security hardening — see
[**`docs/install.md`**](docs/install.md).

## How it works in practice

You mostly don't think about it. Hooks capture every prompt + tool
call + session boundary automatically. The agent gains awareness of
prior work without you typing anything special. A few patterns are
worth knowing:

### Cross-agent handoff

```
$ claude
> "Working on the auth refactor. JWT rotation story is broken; trying
   session cookies as an alternative."
[work for an hour]
> /exit

$ codex   # in the same directory, hours or days later
[SessionStart hook fetches the handoff; the next agent sees it.]
> "Picking up: you were investigating session cookies as an
   alternative to broken JWT rotation. Continuing?"
```

You did nothing special. Handoff created automatically on Claude
Code's session-end, surfaced automatically on Codex's session-start.

### Compaction recovery

When Claude Code or Codex compact their working context, the
`PreCompact` hook fires and ai-memory writes a fresh
`sessions/<id>.md` page summarising the session so far. After
compaction, the agent can recover the summary via `memory_recent`
even though its raw history is gone.

### Adopting ai-memory mid-project: bootstrap

If you're installing ai-memory in a project you've been working on
for months, the wiki starts empty and the first few sessions are
net-zero — you're populating, not retrieving. `ai-memory bootstrap`
solves that by LLM-summarising your existing `git log`, README,
`docs/`, and module-level doc-comments into seed wiki pages.

```bash
# Run from your project's repo root (requires an LLM provider on the
# server). Default settings ingest everything; budget caps at 50k
# input tokens (~$0.04 with Kimi 2.6).
docker run --rm \
    -v ai-memory-data:/data \
    -v "$PWD:/repo" \
    -e AI_MEMORY_AUTH_TOKEN="$TOKEN" \
    -e AI_MEMORY_LLM_PROVIDER=anthropic \
    -e ANTHROPIC_API_KEY=sk-ant-... \
    akitaonrails/ai-memory:latest \
    bootstrap --repo-path /repo --workspace homelab --project myproj
```

Bootstrap produces a `wiki/bootstrap.md` manifest listing every page
generated + a one-paragraph rationale. Run with `--dry-run` first to
preview which sources would be sent without paying for the LLM call.
Re-running on the same project requires `--force`.

See [`docs/install.md`](docs/install.md#bootstrap-mid-project) for
the full flag reference + per-source priority order.

### Spelunking your own history

```bash
docker exec ai-memory ls /data/wiki/sessions/
docker exec ai-memory cat /data/wiki/sessions/<uuid>.md

# Open in Obsidian / any markdown viewer:
docker cp ai-memory:/data/wiki ./my-ai-memory-wiki

# Time-travel:
docker exec ai-memory git -C /data/wiki log --oneline
```

### Rules vs facts — ai-memory tells you when something belongs in CLAUDE.md

When you type something like "don't forget to never add a function
without a unit test", that's a **durable project rule**, not a
session-level observation. Rules need to fire on every relevant
action — that's what your project's `CLAUDE.md` / `AGENTS.md` is for
(it's loaded into the agent's system prompt every turn), while
ai-memory queries only fire when the agent thinks to call them.

The consolidator now classifies each compiled observation as
`decision | fact | rule | gotcha`. Rule-tagged pages are auto-routed
to `wiki/_rules/<slug>.md`, and the next time you run `memory_lint`
the agent sees a suggestion:

> **rule_suggestion**: Page `_rules/never-ship-code-without-test.md`
> looks like a durable project rule. Consider copying it into your
> project's CLAUDE.md / AGENTS.md so the agent sees it on every
> turn, not just when it remembers to call memory_query.

ai-memory never edits your `CLAUDE.md` itself — the suggestion is
the whole UX. You copy what's useful, ignore what isn't.

### Nudge the agent to *use* memory proactively

Lifecycle hooks handle *capture* and *handoff resume* without you
typing anything. Proactive *querying* still depends on the agent
thinking to call `memory_query`. For projects where memory matters,
one command installs the recommended snippet into your `CLAUDE.md`:

```bash
docker run --rm -v "$PWD:/host" akitaonrails/ai-memory:latest \
    install-instructions --target /host/CLAUDE.md
```

The block is wrapped in `<!-- ai-memory:start -->` /
`<!-- ai-memory:end -->` markers so re-running picks up an updated
snippet without duplicating. Use `--target /host/AGENTS.md` for
non-Claude agents, or any other path for project-rules files
(`.cursor/rules`, `.windsurfrules`, etc.). Append `--print` to
preview without writing.

## LLM provider — recommended defaults

You can run ai-memory entirely without an LLM (FTS5 search +
rule-based summaries, $0). When you *do* configure one, the
options below are ranked by fitness for ai-memory's
consolidation workload — see
[`docs/llm-provider-comparison.md`](docs/llm-provider-comparison.md)
for the empirical writeup behind this ranking.

> **TL;DR.** Use **Claude Haiku 4.5** as your default. Switch
> to **GPT-5.4-mini** if you want the same quality cheaper +
> faster. Switch to **qwen3:32b on Ollama** if you have a
> local LLM server and prefer $0 / fully-self-hosted. The
> three are interchangeable; pick once and forget.

### Option 1 — Claude Haiku 4.5 *(recommended default)*

Best balance of speed (~7 s), restraint, and classification
quality. The only model that consistently classifies durable
project rules as `kind: rule` so the consolidator auto-routes
them to `_rules/<slug>.md`. ~$0.02 per consolidation; cost
is negligible for personal use.

```bash
AI_MEMORY_LLM_PROVIDER=anthropic
AI_MEMORY_LLM_MODEL=claude-haiku-4-5
ANTHROPIC_API_KEY=sk-ant-…
```

Or via OpenRouter (handy if you already have an OpenRouter
account and want one bill):

```bash
AI_MEMORY_LLM_PROVIDER=openai-compat
AI_MEMORY_LLM_BASE_URL=https://openrouter.ai/api/v1
AI_MEMORY_LLM_MODEL=anthropic/claude-haiku-4.5
LLM_API_KEY=sk-or-v1-…
```

### Option 2 — OpenAI GPT-5.4-mini *(cheaper alternative)*

~5× cheaper than Haiku, ~2× faster (~4 s avg). Same parse
reliability, same faithfulness. One known weakness: mild
over-classification on trivial sessions (will sometimes
manufacture an extra `decisions/` page for a thin
session). Acceptable for most users.

```bash
AI_MEMORY_LLM_PROVIDER=openai
AI_MEMORY_LLM_MODEL=gpt-5.4-mini
OPENAI_API_KEY=sk-…
```

Or via OpenRouter:

```bash
AI_MEMORY_LLM_PROVIDER=openai-compat
AI_MEMORY_LLM_BASE_URL=https://openrouter.ai/api/v1
AI_MEMORY_LLM_MODEL=openai/gpt-5.4-mini
LLM_API_KEY=sk-or-v1-…
```

### Option 3 — Local Ollama qwen3:32b *(free / self-hosted)*

$0 per consolidation. Requires a machine with at least ~24 GB
of unified or VRAM memory to keep the Q4_K_M weights warm
(~20 GB) plus headroom. Strix Halo / Apple Silicon / a
recent NVIDIA card all work. Latency is ~90 s but
consolidation is a background job — users never see it.

One-time setup on the Ollama host:

```bash
ollama pull qwen3:32b
ollama pull nomic-embed-text   # for embeddings; see below
# Recommended Ollama env:
#   OLLAMA_KEEP_ALIVE=20m       (keep models warm between consolidations)
#   OLLAMA_FLASH_ATTENTION=1
#   OLLAMA_KV_CACHE_TYPE=q8_0   (halves KV memory)
```

ai-memory env:

```bash
AI_MEMORY_LLM_PROVIDER=openai-compat
AI_MEMORY_LLM_BASE_URL=http://<ollama-host>:11434/v1
AI_MEMORY_LLM_MODEL=qwen3:32b
LLM_API_KEY=ollama-local                  # any non-empty value; Ollama doesn't validate
```

If you bind ai-memory to a non-loopback address so Claude
Code on a different machine can reach it, also set:

```bash
AI_MEMORY_ALLOWED_HOSTS=<your-host-or-ip>,localhost,127.0.0.1
```

(Without this rmcp's DNS-rebinding guard rejects external
`Host` headers with 403. See
[`docs/llm-provider-comparison.md`](docs/llm-provider-comparison.md)
for the discovery story.)

### What we don't recommend

- **Claude Sonnet 4.5** — strictly dominated by Haiku for
  this task: same parse reliability, 3× cost, hallucinated
  details before the prompt was tightened. Use it only if
  you specifically need extended reasoning (e.g. cross-page
  lint sweeps).
- **Reasoning-mode models** (Kimi-K2.6, Claude with extended
  thinking enabled, GPT-o3, Gemini "thinking" variants) —
  these models burn `max_tokens` budget on internal
  reasoning before emitting visible content; with the
  strict-JSON consolidation prompt they hang or emit empty
  responses. If you must use one, turn reasoning off.

### Embedding provider

The LLM provider drives consolidation + lint. Embeddings are
a *separate* concern (hybrid retrieval over the wiki — BM25
+ vector RRF). Defaults when `AI_MEMORY_EMBEDDING_PROVIDER`
is set:

| Provider | Default model | Dim |
|---|---|---|
| `openai` | `text-embedding-3-small` | 1536 |
| `voyage` | `voyage-3` | 1024 |

For the local stack, point the OpenAI embedder at Ollama:

```bash
AI_MEMORY_EMBEDDING_PROVIDER=openai
AI_MEMORY_EMBEDDING_BASE_URL=http://<ollama-host>:11434/v1
AI_MEMORY_EMBEDDING_MODEL=nomic-embed-text
AI_MEMORY_EMBEDDING_DIM=768
OPENAI_API_KEY=ollama-local
```

Skipping the embedding provider entirely is fine —
`memory_query` falls back to pure FTS5 (BM25) and still
works; you just lose vector re-ranking.

Per-tier feature breakdown + the openai-compat / Ollama setup
is in [`docs/install.md`](docs/install.md#llm-provider-tiers).

## Architecture in 60 seconds

A single Rust binary, optionally containerised. Runs as an
[MCP](https://modelcontextprotocol.io/) server over stdio + HTTP.
Owns a data directory containing:

```
<data_dir>/
├── wiki/    # markdown source of truth (git-versioned)
├── raw/     # immutable session log archive
├── db/      # SQLite (FTS5 + page_embeddings) — derived index
├── models/  # reserved for local embedding model (v0.3+)
└── logs/    # rolling daily tracing output
```

Agent lifecycle hooks fire-and-forget POST to the server's HTTP
ingress. The server queues writes through a single SQLite writer
(no `database is locked`). On session end an optional LLM-driven pass
rewrites pages atomically with supersession (`is_latest=false` +
`supersedes` chain) and opens a typed handoff for the next agent.
The retention sweep decays unused episodic content while semantic
concept pages compound forever; pinned pages are exempt. Retrieval
is FTS5 by default; when an embedder is configured, hybrid RRF over
`page_embeddings` joins the FTS5 ranks.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the canonical
data-flow diagram + crate breakdown + cross-cutting invariants.

## Docs

| File | What it is |
|---|---|
| [`docs/install.md`](docs/install.md) | **Installation cookbook.** Every agent CLI, every alternative (curl, source build, no-docker, no-auth). Read after the Quick start if your setup doesn't match the happy path. |
| [`docs/mcp-install.md`](docs/mcp-install.md) | Per-client MCP config snippets (Cursor, Claude Desktop, Gemini CLI, OpenClaw, pi). |
| [`docs/deploy.md`](docs/deploy.md) | Homelab deploy: bin/deploy, bearer-token auth, TLS via cloudflared. |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Operational summary: data flow, crate layout, cross-cutting invariants, schema. |
| [`docs/design-decisions.md`](docs/design-decisions.md) | The full v1 spec. |
| Research docs under `docs/` | Karpathy LLM Wiki notes, agentmemory / basic-memory / cognee deep-dives, lessons-learned from upstream issues. |

## Influences and prior art

- **[Karpathy LLM Wiki](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f)** — the compile-not-retrieve pattern.
- **[agentmemory](https://github.com/rohitg00/agentmemory)** — most of the right ideas; this project is the Rust successor.
- **[basic-memory](https://github.com/basicmachines-co/basic-memory)** — the markdown-on-disk source-of-truth model.
- **[cognee](https://github.com/topoteretes/cognee)** — pipeline composition and triplet embeddings.
- **[A-MEM](https://arxiv.org/abs/2502.12110)** — Zettelkasten-style atomic notes with link evolution.

## License

Dual-licensed under MIT OR Apache-2.0.

## Acknowledgements

This codebase is being built collaboratively with Claude Code
(Anthropic Claude Opus 4.7) following the plan documented in
`docs/design-decisions.md`.
