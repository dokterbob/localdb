# Vision

> This page describes where localdb is headed, not what it already does. For what works today, start
> with the [quickstart](quickstart.md).

localdb is built on a simple commitment: **your knowledge should live with you.** On your machine,
under your control, readable by nobody you haven't chosen.

## A library of your own

Everyone accumulates documents that matter to them — notes, research papers, records, saved
articles, books, correspondence. localdb turns a collection like that into a **library**: documents
you keep together because they belong together, that you can search in plain language and that
answer with exact quoted passages, each one pointing back to the document it came from. (One note on
words: in the project's code and technical specifications, a library is called a _store_. This page
says library.)

You can keep as many libraries as you like, side by side: one for your own notes, one for your
bookmarks, one for a research project, later ones for your email and conversations. Each library is
separate, each has its own rules, and all of them live on your machine.

## Your knowledge stays yours

Everything else in this document follows from one principle, so it is worth stating plainly:

- **Your knowledge lives on your machine.** Indexing and search happen locally. Nothing is sent to
  anyone's servers.
- **You decide what leaves it.** Nothing is shared unless you explicitly share it, and sharing one
  library exposes nothing about the others.
- **Owners keep control.** Whoever owns a library always decides who can read it, and can change
  their mind. Access to sensitive material is granted explicitly, person by person — never implied,
  never ambient.
- **No intermediary holds your data.** There is no company in the middle, no cloud account your
  documents pass through, no service whose disappearance takes your library with it.

## Making sense of more than anyone can read

The collections that matter most are usually the ones too large to read: a government records
release running to millions of scanned pages, an organization's decades of files, a lifetime of
personal papers, six thousand books. Researchers, journalists, archivists, and writers all face the
same problem — the answer is in there somewhere, and no human has time to find it by hand.

localdb's job is to make such collections answerable: ask a question, get back the exact passages
that bear on it, each traceable to its source, and follow the connections onward from there.

The same holds for a newer kind of collection: the accumulated memory of an AI assistant. An agent's
notes, distilled facts, and working knowledge form a library too — one that should live on your
machine and answer with sources, exactly like the others.

## Every answer shows its source

A search result you can't trace is a rumor. In localdb, every quote and every search result carries
its origin: which document it came from, where in that document, when it was captured, and where it
was captured from — and that trail can be verified, not just asserted. (The specifications call this
_provenance_.) The standard is the one a careful journalist or scholar already works to: never cite
what you can't trace.

This matters twice over once sharing exists. Knowledge that has passed from hand to hand is only
trustworthy if each hand is on record — so anything that reaches you through others will carry its
whole chain of origin with it.

## Sharing without a middleman

The long-horizon goal is a network of shared libraries. The same localdb that indexes your notes can
one day host libraries you've chosen to share with friends and colleagues — and give you search
across the libraries they have shared with you.

Here is the shape of it. Alice has spent years building a library on a subject she knows deeply. She
shares it with Bob: now Bob can search it, and every result still carries its origin. Alice also
passes along a _reading list_ — a library of libraries — which includes a library that Carol once
shared with her. Bob's computer then talks to Carol's **directly**. Alice is not a relay: her laptop
can be off, her copy out of date, and Bob's access to Carol's library still works — provided Carol
approves him, because her library never stops being hers to control.

What travels through the network of people is _introductions and permissions_, never copies of
anyone's documents routed through a third party. A reading list is pointers, not contents — so no
single machine going dark takes a collection down with it.

And because a design like this is only useful if it can be trusted with the most sensitive material,
the security model is being designed to make accidental sharing impossible by construction — safe
enough that your email and personal messages can sit in libraries alongside your public collections,
without you ever wondering whether the wrong one leaked.

The result is a different way for knowledge to reach you. Instead of a feed ranked by an advertiser,
you get a growing collection weighted by who you trust — enriched by what the people you trust have
found, with every claim traceable to its source.

## Where the project is today

Today, localdb is the personal foundation of that picture, and it stands on its own: point it at
your files, search them from the terminal or from an AI assistant you already use, get cited
answers. Sharing comes later and deliberately — pieces of it will appear only when they can be built
on mature, well-tested foundations.

Everything on this page beyond the personal foundation is direction, not commitment: it exists so
that near-term decisions can be judged against the long-term goal.

## For contributors

The groundwork for all of this is already built into today's design, and it is engineering, not
aspiration: the constraints and concrete mechanisms live in the project's specifications — see
[specs/01-architecture.md §5](https://github.com/dokterbob/localdb/blob/main/specs/01-architecture.md)
(federation-readiness constraints),
[specs/02-domain-model.md](https://github.com/dokterbob/localdb/blob/main/specs/02-domain-model.md)
(identifiers and provenance), and
[specs/06-roadmap.md](https://github.com/dokterbob/localdb/blob/main/specs/06-roadmap.md) (the
capability roadmap and federation requirements).
