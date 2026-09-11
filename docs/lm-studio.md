# Chat with your notes locally using LM Studio

LM Studio can be the AI app you chat with and the MCP client that searches localdb. In this setup,
localdb indexes and searches your files with its local embedding model, and LM Studio runs the
language model that reads the retrieved excerpts and writes an answer. Both run on your machine;
you do not need an API key or a cloud model subscription.

LM Studio is available for macOS, Windows, and Linux; check its
[system requirements](https://lmstudio.ai/docs/app/system-requirements) for supported hardware.
This walkthrough targets macOS Apple Silicon and Linux, the platforms with
[published localdb binaries](install.md#supported-platforms). It does not assume a native Windows
localdb installation.

## 1. Install and download models

Install [localdb](install.md) and [LM Studio](https://lmstudio.ai/). Use an LM Studio version with
MCP support (0.3.17 or later). Download and load a chat model that supports tool calling and fits
your machine's memory. Download its runtime too, before going offline.

The chat model and embedding model serve different purposes: LM Studio generates answers, while
localdb's embedding model makes your notes searchable. Keep localdb's local embedding provider for
this recipe. On an existing installation, review your [configuration](configuration.md), including
store overrides, before indexing private files. The default embedding settings are:

```yaml
defaults:
  indexing:
    embedding:
      provider: local
      model: pplx-embed-context-v1-0.6b
```

Hosted embedding providers also send text out of the machine, even when the chat model is local.

## 2. Index a folder and check search

For a fresh installation, run these commands in a terminal, replacing `~/notes` with your folder:

```bash
localdb add ~/notes
localdb search "a topic in my notes"
```

The first command creates the configuration and `default` store and indexes the folder. First use
downloads localdb's default embedding model (about 706 MB); subsequent runs reuse the cached model.
Complete this step while online. Use local files for this recipe; URL and feed sources need network
access to fetch their content.

If you already use another store, substitute its name for `default` in the MCP configuration below.
Confirm that search returns relevant results before connecting LM Studio.

## 3. Connect LM Studio to localdb

Find the installed binary's absolute path:

```bash
which localdb
```

In LM Studio, open the **Program** tab in the right sidebar, then **Install → Edit mcp.json**.
Add the `localdb` entry under `mcpServers`, preserving any existing entries. A complete minimal file
looks like this; replace `/absolute/path/to/localdb` with the path printed above:

```json
{
  "mcpServers": {
    "localdb": {
      "command": "/absolute/path/to/localdb",
      "args": ["mcp", "--store", "default"]
    }
  }
}
```

Save the file and enable localdb's tools for your chat. See LM Studio's
[MCP setup instructions](https://lmstudio.ai/docs/app/mcp) for its configuration UI.
LM Studio launches `localdb mcp` as a local subprocess and talks to it over stdio. Neither
`localdb serve` nor LM Studio's HTTP API server is needed. If you use a custom localdb configuration,
append `"--config", "/absolute/path/to/config.yaml"` to `args`.

## 4. Ask a question and verify tool use

Start a chat with your downloaded model and try:

> Use localdb to list the available stores, then search my notes for [a topic you indexed].
> Answer from the search results and cite the source paths. If nothing matches, say so.

Check that the chat actually calls `list_stores` and `search` and that the returned paths match
your notes. A plausible answer alone does not prove the model searched. The `--store default`
argument limits which store this connection exposes; repeat `--store` to include another one.

If localdb fails to start, check the absolute binary path, config path, and store name. If tools
are available but the model never calls them, check that they are enabled and that the loaded
model supports tool calling. For context overflow, ask for fewer search results or short excerpts
instead of entire documents.

## 5. Verify offline operation

After both models and their runtimes have downloaded, disconnect networking and repeat the search
and chat steps with local files. LM Studio documents which features work in its
[offline operation guide](https://lmstudio.ai/docs/app/offline); model discovery, downloads, and
update checks require a connection.

For ongoing no-egress use, keep networking disconnected or enforce outbound restrictions for the
client and its subprocesses. Review telemetry, logging, cloud memory, plugins, and other MCP
connections; disable anything that forwards content outside your intended boundary. A local model,
stdio, and store scoping alone do not enforce those restrictions. See
[Data and model privacy](mcp.md#data-and-model-privacy).
