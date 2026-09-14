---
title: "Runbook: Database Failover"
date: 2020-11-05
tags: [ops, database, runbook]
---

# Runbook: Database Failover

This document describes the manual failover procedure for the primary
database cluster.

## When to use this

Use this runbook when automated failover has not triggered within five
minutes of a primary outage alert.

## Steps

1. Confirm the primary is actually down, not just slow.
2. Promote the standby replica.
3. Repoint application connection strings.
4. Verify write traffic resumes on the new primary.
