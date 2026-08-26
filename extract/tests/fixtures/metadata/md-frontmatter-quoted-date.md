---
title: "Postmortem: November Outage"
date: "2020-11-05"
tags: ["incident", "postmortem"]
---

# Postmortem: November Outage

This document has the same date as `md-frontmatter-date.md` but written as
a quoted YAML string rather than a bare ISO date scalar. YAML parsers
sometimes handle these two representations differently (bare dates parse
to a native date/datetime type in some parsers; quoted strings always
parse as plain strings), so this file probes whether localdb's front
matter date extraction normalizes both forms to the same result.

## Summary

A brief outage occurred in the primary region. Root cause and remediation
steps follow below.

## Timeline

- 14:02 UTC — alert fires
- 14:09 UTC — on-call acknowledges
- 14:31 UTC — mitigation deployed
- 14:45 UTC — resolved
