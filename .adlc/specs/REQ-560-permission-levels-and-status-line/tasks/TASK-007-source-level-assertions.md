---
id: TASK-007
title: "Source-level assertions: no level on the egress path, no status write outside the seams"
status: complete
parent: REQ-560
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-006]
---

## Description

Two scanners over production source, both in the shape REQ-544 established: they
make a *future* call site impossible to add quietly, which is the failure mode
neither a unit test nor a behavioural test can cover.

- **AC-5**: no egress-path predicate references the permission level (BR-3, BR-4)
- **AC-16**: no status-line write reaches stdout outside the `Surface` /
  `Prompter` seams (BR-12)

## Files to Create/Modify

- `crates/tetond/src/egress/mod.rs` — a `#[cfg(test)]` assertion reusing
  `crate::call_sites::scan::{production_sources, code_only}`: no production
  source file under `src/egress/` mentions `PermissionLevel`, `permission_level`,
  `table_for`, or `PermissionGate`. Reuse the shared helper rather than writing a
  second "production source only" rule — that sharing is why the helper exists
  (`call_sites.rs:95-100`)
- `crates/teton/src/status.rs` — a `#[cfg(test)]` scanner over the **client**
  crate's production sources. The `teton` crate has no equivalent of
  `call_sites::scan`, so add a small one (walk `src/`, truncate each file at its
  first `\n#[cfg(test)]` following the daemon's convention that test items come
  last) and assert that `status.rs` and the status-rendering path in `prompt.rs`
  contain no `print!`/`println!`/`eprint`/`io::stdout()` outside the
  `FramedStdinPrompter` methods that *are* the `Prompter` seam

## Acceptance Criteria

- [ ] **AC-5**: the egress scan passes on the tree as built, and is proven to
      *fail* when a `permission_level` reference is temporarily introduced into
      an egress source — demonstrate the red state before committing the green
      one (LESSON-454: a gate nobody has seen fail is not known to work)
- [ ] **AC-16**: the client scan passes, and is proven to fail when a
      `println!` is temporarily added to `status.rs`
- [ ] Both scans skip test modules, so a test that legitimately names a level or
      prints does not trip them
- [ ] Both scans fail loudly if they match **zero** files — a scanner that
      silently stops seeing its target is worse than no scanner (the drift
      `call_sites.rs` calls out)
- [ ] The `teton`-side helper is documented as the client twin of
      `call_sites::scan`, naming why it is a separate copy (different crate,
      different `CARGO_MANIFEST_DIR`)
- [ ] `cargo test --workspace` green; no clippy warnings

## Technical Notes

**BUG-159 hazard, flagged for this task specifically.** `call_sites.rs` and
`harness/duty.rs` read production source with `.expect("readable source file")`
after walking it, so any writer touching `src/` mid-run panics them. This task
adds two more scanners of that shape. Do not run the suite concurrently with an
edit to `src/`; if you see that panic, it is BUG-159 and not this change.

Prefer `code_only`-style stripping over a raw substring search so a mention
inside a doc comment does not trip the scan — the daemon's existing scanners set
that precedent, and a scanner that fires on prose is a scanner people disable.
