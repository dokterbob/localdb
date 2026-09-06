# Agent Memory with localdb

A practical recipe: give an AI agent hybrid, citation-backed search over its own persistent memory,
fully local, using nothing but what localdb ships today.

## The pattern

Most agent harnesses that maintain durable memory keep it as **plain markdown files** — OpenClaw's
agent workspace (`MEMORY.md` plus daily `memory/YYYY-MM-DD.md` logs), Hermes-style setups, and
[gbrain](https://github.com/garrytan/gbrain)-backed deployments all treat a markdown tree on disk as
the source of truth for what the agent knows. That is exactly the shape localdb indexes best:

1. **Point a store at the memory tree:**

   ```bash
   localdb store add agent-memory
   localdb source add ~/.openclaw/workspace --store agent-memory
   localdb index --store agent-memory
   ```

2. **Wire the MCP server into the harness** so the agent can search its own memory. Follow the
   [MCP setup guide](mcp.md#setup) for your host — stdio for a local harness
   (`claude mcp add localdb -- $(which localdb) mcp`), or HTTP via `localdb serve` for a harness on
   another machine. Optionally add the localdb agent skill (`npx skills add dokterbob/localdb`) so
   the agent knows the CLI, citation shape, and tool signatures without discovering them by trial.

3. **Search from the agent.** The agent's `search` calls now return ranked excerpts from its own
   memory with structured citations — which file, which byte span, which content hash — combining
   semantic recall ("what did I conclude about X?") with exact keyword match (names, dates,
   identifiers). All of it runs locally; nothing about the agent's memory leaves the machine.

Keeping the memory store separate from your document stores means the agent's recall and your own
archives stay independently scoped — searches can target either or both.

## Keeping the index current

localdb does **not** yet re-index automatically when a memory file changes: the daemon's file
watcher currently watches config changes only, not source paths (see the honest gap list in the
[comparison doc](comparison.md#where-localdb-is-behind)). Until watch-triggered re-indexing lands,
re-run indexing whenever memory has changed, whichever fits the harness:

- **Manually / on a schedule** — `localdb index --store agent-memory` is incremental (unchanged
  files are skipped), so a cron entry every few minutes is cheap.
- **Post-write hook** — if the harness supports hooks after memory writes, have it invoke
  `localdb index --store agent-memory` there; with a running daemon the command submits a background
  job and returns quickly.

## Why this matters

The emerging evidence in the agent-memory space points one way: the durable, queryable knowledge
layer — not the model — is what carries long-term agent capability.
[gbrain](https://github.com/garrytan/gbrain) (Garry Tan) is a production system built on precisely
this loop: agents write markdown memory, a hybrid index with a typed knowledge graph serves it back
as cited answers. WikiSkill ([arXiv 2608.27454](https://arxiv.org/abs/2608.27454), Google Research
and Virginia Tech) shows experimentally that a persistent knowledge layer between an agent's raw
experience and its skills is the component that carries the performance gains. localdb gives you the
retrieval half of that layer today — local, provenance-carrying, harness-agnostic — with the
citations that let an agent (or you) verify where a remembered "fact" actually came from.

## The current boundary: reads only

localdb's MCP surface is **read-only in v1**. `localdb mcp --allow-write` is accepted but registers
no writing tools and prints a warning — deliberately, because write semantics through agents deserve
their own design pass (see
[specs/05-surfaces.md §4](https://github.com/dokterbob/localdb/blob/main/specs/05-surfaces.md)). In
this recipe the harness writes memory as files, and localdb serves it back; the agent cannot write
through localdb itself. A first-class MCP write path for agent memory is tracked in
[#349](https://github.com/dokterbob/localdb/issues/349).
