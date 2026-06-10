# Vision: From Personal Knowledge Server to Peer Sense-Making Network

> Status: direction, not commitment. Nothing in this document is MVP scope. It exists so that
> near-term architecture decisions can be judged against the long-horizon goal. The MVP carries
> only the *hooks* listed at the end. Companion to [product_positioning.md](product_positioning.md).

## The arc

The product starts as a **personal knowledge server**: point it at your files and URLs, get hybrid
semantic search everywhere you work — CLI, MCP, API, browser. That is the whole of the first
release, and it must stand on its own.

The long-horizon goal is larger: a **peer-to-peer truth-finding and sense-making network**. The
same server that indexes your notes also holds stores you choose to share with friends, and stores
your friends have shared with you — and, transitively, stores *their* friends shared with them.
Knowledge propagates through the social graph, friend-of-a-friend, with provenance attached at
every hop. Instead of a feed ranked by an advertiser, you get a corpus weighted by who you trust.

## Stores are the unit of everything

A **store** is a named knowledge base with an identity, a privacy level, and its own indexing
policy. From day one there are multiple stores per instance — your files in one, your bookmarks in
another, later your email and work chat in others. Each store is **private** or **shared**.

Stores are the unit of sharing, of trust, of access control, and (later) of federation. This is
why stores are a first-class domain concept in the MVP rather than a folder-shaped afterthought.

## Sharing propagates; content does not relay

The defining constraint for federation: **direct connections, no indirection.**

When Alice shares a list of stores with Bob — some hosted on her node, some that were shared *to*
her from Carol — Bob's node does not pull Carol's content through Alice. Alice's server may be a
laptop that is asleep. Instead:

1. Alice's share is a list of **store references + introductions**: for each store, where its
   origin lives and a capability (or a way to request one) to access it.
2. Bob's node contacts each remote store's **origin** (Carol's node) directly, presents the
   delegated introduction, and requests its own credential.
3. From then on Bob ↔ Carol is a **direct connection**. Alice being offline never breaks Bob's
   access to Carol's store.

What propagates through the social graph is **credentials and capabilities, not proxied traffic**.
This is capability handoff in the OCAP/delegation tradition. Candidate substrates to evaluate when
the time comes (research pointers, not commitments): **iroh** tickets for direct connectivity and
hole-punching, **UCAN**-style delegated capability tokens, and the protocols surveyed in
[specs/06-roadmap.md](specs/06-roadmap.md).

## Mature authentication, or none

Shared stores use **mature, audited authentication mechanisms** — OIDC/OAuth2 for client-server
sharing, established capability-token systems for peer delegation. **No homegrown crypto, ever.**
If a sharing feature would require inventing a protocol, the feature waits.

## Provenance and trust as first-class metadata

Sense-making across a social graph only works if every piece of knowledge answers: *where did this
come from, and via whom?* Every chunk therefore carries provenance from day one: origin store,
source, content hash, fetch time — and later, the share-path (who shared it, via whom). Trust
signals are metadata you can filter and rank on, not a black-box score.

## Messages are stores too

Email, messengers, and group chat are future store types (`imap`, `mbox`, messenger connectors).
A thread between three friends is already a tiny sense-making network; indexing it with thread
context intact is the natural bridge between "my files" and "our shared understanding." This is a
key reason the embedding interface is **document-aware (contextualized) from day one**: message
chunks need their thread as context to embed well.

## What the MVP actually carries

The MVP ships none of the above. It carries exactly four architectural hooks, each cheap now and
expensive to retrofit:

| Hook | Where specified |
|---|---|
| Stable, content-addressed document/chunk IDs | [specs/02-domain-model.md](specs/02-domain-model.md) |
| Provenance metadata on every chunk | [specs/02-domain-model.md](specs/02-domain-model.md) |
| Per-store visibility enum (`private` \| `shared`; only `private` functional in MVP) | [specs/01-architecture.md](specs/01-architecture.md) |
| Store as first-class entity (multiple stores per instance) | [specs/01-architecture.md](specs/01-architecture.md) |
