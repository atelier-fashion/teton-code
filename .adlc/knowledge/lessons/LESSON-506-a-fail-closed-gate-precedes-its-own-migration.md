---
id: LESSON-506
title: "A fail-closed load gate runs before the migration meant to satisfy it"
component: "daemon/config"
domain: "configuration"
stack: ["rust", "daemon"]
concerns: ["migration", "startup", "backward-compatibility"]
tags: ["validation", "usability-pass", "fail-closed", "serde-optional", "one-shot-migration"]
req: REQ-557
created: 2026-08-11
updated: 2026-08-11
---

## What Happened

REQ-557 made `ModelProvider.model` a required declared field and shipped a
one-shot migration for configs written before it existed. Two layers each tried
to enforce required-ness, and each one broke the migration from a different
direction:

1. **The deserializer.** A bare `model: String` makes every pre-REQ config fail
   to parse — and a config that cannot be opened cannot be migrated. ADR-B fixed
   this with `Option` + `#[serde(default)]`.
2. **`Config::validate()`.** ADR-B's original wording moved required-ness here,
   which *looks* like the right home next to the existing duplicate-id and
   raw-key checks. It is not. `Config::load` is `from_toml` then `validate`, and
   `load_config` converts any load error into "Refusing to start rather than fall
   back to an empty config that would silently drop your privacy boundaries."
   So a pre-REQ config — every provider `model: None` — would fail validation and
   refuse startup **before migration could run**. ADR-B got the config to parse;
   validation still gated startup one layer down.

The second failure also inverted the requirement's own promise: BR-7 says the
daemon *starts with that provider unusable*, but a validation error would let a
single unresolvable provider brick the whole daemon.

## Lesson

Split **validity** from **usability**. A structural error (duplicate id, a raw
key where a reference belongs, a `default_provider` naming a provider that does
not exist) is invalid — fail closed at load. A record that is merely *incomplete*
in one field is a usability condition: report it by id in a separate non-fatal
pass, mark it unusable, refuse to *use* it at the point of use, and let everything
else start.

The requirement's own vocabulary is usually the tell. BR-7 said "unusable", not
"invalid". A config naming a provider we cannot yet price is not corrupt.

## Why It Matters

A fail-closed startup gate is correct and worth keeping — that is why the trap is
easy to walk into. But any gate that runs *before* a migration will veto the
migration's own input, and the failure mode is total: the daemon will not start,
so the code that would fix the config never executes. The user's only remedy is
to hand-edit the file the product was supposed to migrate for them.

The blast radius scales the wrong way, too. Enforcing at load makes one bad entry
fatal for every good one.

## Applies When

- Adding a required field to a persisted, user-owned config or schema that
  existing installs already wrote.
- Any load path shaped `parse → validate → refuse-to-start`, where a migration is
  expected to run after load.
- Reviewing an ADR that places a new requirement in an existing validation pass
  "because that is where the other checks live" — check what that pass does on
  failure before agreeing.
