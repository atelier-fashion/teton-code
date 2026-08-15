---
id: ASSUME-010
title: "No `#[cfg(test)] mod` ever appears above an `impl Tool` block in tool sources"
status: unresolved
req: BUG-172
created: 2026-08-14
resolved:
---

## Assumption

The boundary-coverage scan's truncation anchor (`"\n#[cfg(test)]\nmod "`,
BUG-172) assumes tool source files only place `#[cfg(test)] mod …` as the
trailing test module. A `#[cfg(test)] mod helpers;` declared *above* a file's
`impl Tool` block would still truncate the scan early and silently hide the
tool — the same LESSON-432 shape BUG-172 closed for `cfg(test)` *items*.

## Context

Accepted as residual risk in BUG-172 because the shape is rare (all nine tool
files end with a single `#[cfg(test)] mod tests`) and such a module is
genuinely test code. The registry-derived second check
(`the_builtin_registry_registers_nothing_the_enumeration_has_not_claimed`)
only covers *registered* tools, so it is not a backstop for an
unregistered-but-implemented tool. If a mid-file `cfg(test)` module ever
becomes idiomatic here, tighten the anchor to `mod tests` specifically (and
mutation-check the change per LESSON-527).

## Resolution

(unresolved — revisit if a tool file grows a `cfg(test)` module above its
impl, or when the boundary suite next changes shape)
