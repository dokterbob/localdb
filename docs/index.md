---
layout: default
title: localdb docs
---

<section class="hero">
  <img class="hero-logo" src="{{ '/assets/images/localdb-logo-light.png' | relative_url }}" alt="localdb">
  <div class="eyebrow">Local-first retrieval for agents</div>
  <h1>Search your files with citations, from the terminal or an AI assistant.</h1>
  <p>
    localdb indexes notes, PDFs, EPUBs, Office documents, HTML, and plain text into a private
    hybrid search store with structured citations and MCP access.
  </p>
  <div class="hero-actions">
    <a class="button primary" href="{{ '/quickstart.html' | relative_url }}">Start with the quickstart</a>
    <a class="button secondary" href="{{ '/mcp.html' | relative_url }}">Connect an MCP client</a>
  </div>
</section>

<section class="feature-strip" aria-label="Highlights">
  <div>
    <strong>Hybrid search</strong>
    <span>BM25 plus dense vectors</span>
  </div>
  <div>
    <strong>Verifiable results</strong>
    <span>Citations, hashes, and spans</span>
  </div>
  <div>
    <strong>Local by default</strong>
    <span>No daemon or cloud required</span>
  </div>
</section>

<section class="doc-section">
  <div class="section-heading">
    <h2>Start Here</h2>
    <p>Install localdb, index your first source, and search it from the CLI or an assistant.</p>
  </div>

  <div class="doc-grid">
    <a class="doc-card accent-green" href="{{ '/install.html' | relative_url }}">
      <span>01</span>
      <h3>Install</h3>
      <p>Build from source or install a release binary.</p>
    </a>
    <a class="doc-card accent-blue" href="{{ '/quickstart.html' | relative_url }}">
      <span>02</span>
      <h3>Quickstart</h3>
      <p>Create a store, add sources, index files, and run your first search.</p>
    </a>
    <a class="doc-card accent-gold" href="{{ '/cli.html' | relative_url }}">
      <span>03</span>
      <h3>CLI Reference</h3>
      <p>Every command, option, output shape, and exit code.</p>
    </a>
    <a class="doc-card accent-rose" href="{{ '/mcp.html' | relative_url }}">
      <span>04</span>
      <h3>MCP Server</h3>
      <p>Expose indexed stores to Claude, Codex, or another MCP-capable client.</p>
    </a>
  </div>
</section>

<section class="doc-section">
  <div class="section-heading">
    <h2>Reference</h2>
    <p>Design notes and operational details for contributors and advanced users.</p>
  </div>

  <div class="link-list">
    <a href="{{ '/configuration.html' | relative_url }}">Configuration</a>
    <a href="{{ '/architecture.html' | relative_url }}">Architecture</a>
    <a href="{{ '/http-api.html' | relative_url }}">HTTP API</a>
    <a href="{{ '/comparison.html' | relative_url }}">Comparison</a>
    <a href="{{ '/design-decisions.html' | relative_url }}">Design decisions</a>
    <a href="{{ '/release-engineering.html' | relative_url }}">Release engineering</a>
  </div>
</section>
