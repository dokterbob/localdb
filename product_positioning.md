# Product Positioning and Vision

This document defines what the product is, who it is for, why it should exist, and how it should be positioned.

## Product definition

The product is a **local-first knowledge server** for files, web content, and agent workflows. It ingests user-owned content, turns it into a searchable and reusable knowledge layer, and exposes that layer through a web UI, a CLI, an API, and MCP so both humans and software can use it.[cite:88][web:96][web:99]

It is not just a desktop search app and not just a backend library. It is a **productized retrieval layer** that can run on a laptop, on a home Mac, or on a small Linux server, while also being modular enough for developers to embed pieces of it into their own applications.[cite:88]

## Core idea

Most local semantic search tools split into two weak categories today: polished local apps that are closed and hard to integrate, or open building blocks that are powerful but rough to install, configure, and operate.[web:74][web:86][cite:88] The product exists to bridge that gap with a system that feels simple and local-first, but is also open, scriptable, and reusable as infrastructure.[cite:88]

The core promise is straightforward: **point it at your knowledge, and it becomes available everywhere you work** — in a browser, in scripts, in CLIs, and in MCP-enabled tools. That promise matters because local knowledge should not be trapped inside a single UI or a single vendor-specific application.[cite:88][web:96]

## Problem statement

People increasingly have valuable information spread across Markdown notes, PDFs, documentation folders, exported chats, HTML pages, screenshots, and other semi-structured artifacts. Traditional search works poorly on meaning, while many AI-oriented retrieval stacks require too much configuration, too many moving parts, or too much cloud dependence for users who want a private and simple setup.[cite:88][web:35][web:104]

At the same time, users who care about local AI, privacy, and agent workflows want more than a consumer search box. They want APIs, automation, MCP, reusable indexes, portable storage, and the ability to connect their own embedding and LLM backends when needed.[cite:88][cite:90][web:96][web:107]

## What the product is not

The product is not positioned as a general-purpose chatbot, not as a cloud SaaS knowledge base, and not as an enterprise compliance platform. It may support question-answering and agent retrieval workflows, but the center of gravity is the **knowledge layer itself**: ingestion, indexing, retrieval, navigation, and integration.[cite:88]

It is also not a framework-first product. Developers should be able to use its libraries and protocol surfaces, but the primary positioning should remain a usable product with strong defaults rather than a bag of primitives.[cite:88]

## Primary users

### 1. Local-first technical users

This group includes developers, researchers, founders, and advanced operators who keep important knowledge in files and want fast semantic retrieval without sending everything to cloud services. They care about privacy, inspectability, performance, and the ability to integrate the system into their existing workflows.[cite:88][cite:90]

These users are often comfortable with CLIs and APIs, but they still value a polished UI and a low-friction setup path.[cite:88]

### 2. Agent and automation users

This group wants a local or self-hosted knowledge layer that MCP-enabled tools and custom agents can query. They care about predictable APIs, citations, indexing control, and the ability to expose their local knowledge to coding agents, research agents, and internal automations.[cite:88][web:96][web:99]

### 3. Small-team self-hosters

This group wants to run the product on a home server, office Mac, or small Linux machine and access it from multiple clients. They want something simpler than assembling a full custom RAG stack, but more open than a sealed desktop app.[cite:88]

## Secondary users

Secondary users may include writers, consultants, lawyers, and knowledge workers with large local document corpora, but the early product should not be positioned around generic mass-market file search. The strongest early wedge is technical and local-first rather than broad consumer productivity.[cite:88]

## Value proposition

The product’s value proposition has four parts.

### Simple by default

It should work out of the box with local storage, local embeddings, and minimal config. Users should be able to point it at files and start searching quickly without first designing a retrieval architecture.[cite:88][web:109]

### Open and composable

The same system should expose web APIs, a CLI, and MCP so users can automate it, extend it, and build on top of it. This prevents lock-in to a single GUI or a single workflow surface.[web:96][web:99][cite:88]

### Local-first and privacy-friendly

The default path should not require sending private documents to third-party services. Local embeddings and local storage should be the baseline, with external OpenAI-compatible endpoints available only when users explicitly want them.[cite:88][web:109][web:107][web:111]

### Upgradeable architecture

Users should be able to start with a laptop-local deployment and later move to a home server, a remote Qdrant instance, different embedding backends, or richer enrichment pipelines without replacing the whole product.[web:104][web:98][cite:88]

## Product thesis

The thesis behind the product is that there is room for a **self-hosted retrieval product** that sits between consumer desktop search and raw RAG infrastructure. Consumer tools optimize for polished search but usually do not expose reusable APIs or agent integration, while infrastructure tools optimize for flexibility but often fail the usability test for day-to-day local knowledge work.[web:74][web:86][web:35][cite:88]

A local knowledge server with strong defaults, hybrid retrieval, and multiple interfaces can occupy that gap. The long-term defensibility comes from product quality, local-first trust, protocol compatibility, and a layered architecture that lets the system serve both humans and software.[cite:88][web:104][web:96]

## Key product pillars

### 1. One knowledge layer, many surfaces

The same indexed corpus should be available through the web UI, CLI, API, and MCP. Search results, citations, metadata, and collection behavior should feel consistent across these surfaces.[cite:88][web:96][web:99]

### 2. Strong defaults, optional complexity

A new user should not need to understand vector databases, sparse encoders, chunking strategies, or embedding providers to succeed. Advanced users should be able to customize these later, but the default experience should stay opinionated and calm.[cite:88][web:109]

### 3. Local-first before cloud-first

The default deployment should prioritize local execution, local storage, and local embeddings. Remote services and external APIs are extensions, not prerequisites.[cite:88][web:109][web:104]

### 4. Modular internals

Internally, the system should be decomposable into reusable layers so developers can adopt only the pieces they need. This supports the layered open-source approach and makes the product more resilient over time.[cite:88]

### 5. Retrieval quality over hype

The product should be judged by retrieval quality, indexing robustness, citations, and operational reliability. Chat and generation features are useful, but they should not distract from the quality of the underlying knowledge layer.[cite:88][web:104]

## Positioning statement

For local-first technical users and self-hosters who want their files and web content to become a reusable AI-ready knowledge layer, this product is a **local knowledge server** that ingests, indexes, and exposes private knowledge through search, APIs, CLI, and MCP. Unlike closed desktop search apps or DIY RAG stacks, it combines strong out-of-the-box usability with open integration surfaces and self-hosted flexibility.[cite:88][web:74][web:86][web:96]

## Category framing

The best category language is probably not “RAG framework” and not merely “desktop search.” Better category candidates are:

- Local knowledge server
- Self-hosted semantic knowledge layer
- Local-first retrieval server
- Personal or team knowledge index for AI tools

“Local knowledge server” is likely the clearest umbrella term because it communicates both the deployment model and the product’s role as infrastructure that serves multiple clients.[cite:88]

## Competitive framing

The product should be framed against three adjacent categories.

| Category | What they do well | Where this product differs |
|---|---|---|
| Native semantic search apps | Great UX, fast onboarding, local feel | More open, scriptable, agent-compatible, and reusable as infrastructure.[web:74][web:86] |
| DIY RAG stacks | Highly flexible and extensible | Simpler defaults, easier operation, better product UX.[web:35][web:104] |
| Knowledge/chat apps | Friendly question-answering UI | Focused on the underlying knowledge layer, citations, and multi-surface access rather than just chat. |

## Why open source

Open source is not just a licensing choice here; it is part of the positioning. Users in this category care about inspectability, self-hosting, portability, and not being trapped in a black-box local app. A layered FOSS approach also increases adoption because some users will want the whole product while others will want only a library, the MCP server, the CLI, or the ingestion pipeline.[cite:88]

## Initial scope narrative

The first release should be positioned as a strong foundational product, not a maximal one. It does not need to solve every document format, every graph use case, or every collaborative workflow on day one. It needs to convincingly solve the core use case of local-first ingestion, hybrid retrieval, and multi-surface access with solid defaults.[cite:88][web:104]

That means the early narrative should emphasize:

- Bring your local files into one searchable knowledge layer.
- Use that knowledge from the browser, terminal, or MCP clients.
- Keep everything local by default.
- Swap in external providers only when you want to.
- Upgrade from laptop use to home-server deployment without rethinking the whole stack.[cite:88][web:104][web:107]

## Product principles

The following principles should guide decisions when trade-offs arise.

1. **Local-first beats cloud-first by default.**
2. **Usable defaults beat configurability in the main path.**
3. **Open protocols beat proprietary surfaces where possible.**
4. **Retrieval quality beats flashy chat demos.**
5. **One shared core beats duplicated logic across interfaces.**
6. **Optional complexity beats mandatory complexity.**
7. **Layered reuse beats monolithic lock-in.**

## Open positioning questions

Several product-definition questions remain open and should be answered before naming, branding, or launch messaging harden.

### Audience focus

- Is the explicit first audience individual technical users, or technical individuals plus very small teams?
- Is the language “personal knowledge” too limiting if home-server and team access are important?

### Category and naming

- Is “local knowledge server” the final category term, or should the product use “semantic search” or “retrieval” language more prominently?
- Should the name sound infrastructure-like, app-like, or both?

### Relationship to chat and agents

- Is chat a core built-in feature, or only a downstream consumer of the retrieval layer?
- How strongly should MCP and agent workflows appear in top-level positioning versus being framed as advanced capabilities?

### Commercial and governance questions

- Pure community FOSS, open-core, or dual licensing later?
- Is hosted sync or hosted remote control ever in scope, or is the project intentionally self-hosted only?

### Trust and privacy messaging

- How strongly should privacy be emphasized compared with flexibility and integration?
- Will optional external providers complicate the purity of the local-first message, and if so how should that be framed?

## One-paragraph version

This product is a local-first knowledge server that turns files and web content into a reusable semantic knowledge layer for both humans and software. It is designed to be simple enough to run locally with strong defaults, but open enough to expose APIs, CLI access, and MCP for agents and automations, giving technical users and self-hosters a private, composable alternative to closed desktop search apps and overly complex DIY RAG stacks.[cite:88][web:74][web:86][web:96][web:104]

## One-sentence version

A local-first knowledge server that makes your files and web content searchable and reusable everywhere you work — browser, terminal, API, and MCP — without forcing you into a closed app or a custom RAG stack.[cite:88][web:96][web:104]
