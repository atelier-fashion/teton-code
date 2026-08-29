---
id: LESSON-586
title: "A rule that names one field of a validated class is a rule about the class — enumerate it"
component: "daemon/config"
domain: "security"
stack: ["rust", "daemon"]
concerns: ["security", "privacy"]
tags: ["credentials", "auth-ref", "config", "spec-validation", "drift-guard"]
req: REQ-596
created: 2026-08-29
updated: 2026-08-29
---

## What Happened

REQ-596's BR-1 said every variable named by a configured `auth_ref = "env:<VAR>"`
must be absent from the `shell` child. Reading the code during spec validation
showed that `is_recognized_auth_ref` — the predicate that defines what a
credential reference *is* — gates **two** fields, not one:
`[[providers]].auth_ref` and `[web] search_key_ref`. Both resolve through the
same `std::env::var` arm.

Implementing BR-1 as written would have shipped this REQ's own leak, in the field
that merely happened to have been written second. BR-1.1 and AC-1.1 were added
before any code was written.

Because the enumeration ended up in a different crate from the fields, a derived
guard was added: a test scans `config.rs` for the predicate's call sites and
fails if a third appears without the enumeration following it.

## Lesson

When a spec rule names a field, check whether the field is one member of a
validated *class*. If a single predicate decides "this is a credential
reference", then the rule is about everything that predicate gates — and the
spec should say so by naming the predicate, not by naming today's fields.

Then make it hold mechanically. Co-locating the enumeration with the fields is
not enough: a third field can be added next to a neighbouring enumeration without
anyone updating it. A derived count is what actually holds.

## Why It Matters

The second field is always the one that leaks. It was added later, it is less
famous, and every rule written before it exists silently excludes it. Here the
gap was caught in spec validation for the cost of reading one function; caught in
production it would have been a credential disclosure through a model-driven
`echo`.

## Applies When

Writing or validating any rule that names a config field, an API parameter, or a
route by name; implementing a security rule over "credentials", "secrets", or
"PII" where a validator already defines the class.
