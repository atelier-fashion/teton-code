---
id: TASK-113
title: "BR-12: a typed effort-refused error, a single per-call fallback, and the session refusal memo"
status: complete
parent: REQ-559
created: 2026-08-11
updated: 2026-08-11
dependencies: [TASK-112]
---

## Description

What happens when a provider refuses the effort field. BR-12 requires a typed
error naming the provider, the requested level and the clamped level; a fallback
to the `none` shape for that call; no silent retry; and never both shapes sent to
"see which works". ADR-F resolves OQ-6 on top of that: the refusal is remembered
for the **session**, never written to config, never allowed to mutate the
declared `reasoning_shape`, and always visible on the surface.

## Files to Create/Modify

- `crates/teton-providers/src/lib.rs` — new `ProviderError::EffortRefused`
  variant beside the existing `Build` / `ClientError` variants (:232)
- `crates/teton-providers/src/failure.rs` — classification for the new variant
- `crates/tetond/src/runtime.rs` — the session-scoped `provider_id → refused`
  memo and the single-fallback retry of the same call with `Omit(RefusedThisSession)`
- `crates/tetond/tests/` — a new integration test for AC-2b and AC-10

## Acceptance Criteria

- [ ] `ProviderError::EffortRefused { provider_id, requested, clamped }` exists,
      carries all three values BR-12 names, and its `Display` names all three.
      It carries **no response body and no prompt text** (conventions.md: no
      content in error messages).
- [ ] A 4xx whose body names the effort field is classified as `EffortRefused`
      rather than as a generic `ClientError`. Detection is on the **effort field
      name** appearing in the error body — narrow on purpose: a 400 for an
      unrelated reason must stay a `ClientError` and must **not** poison the
      session memo.
- [ ] On `EffortRefused` the daemon retries the call **exactly once**, with
      `ResolvedEffort::Omit(EffortOmission::RefusedThisSession)`. A test asserts
      the capture contains exactly two requests: the first with the effort field,
      the second with neither reasoning field. **Not three** — a second refusal on
      the fallback is a hard failure, not another retry.
- [ ] The capture contains **no request carrying both shapes** at any point in the
      sequence (AC-10, AC-2b).
- [ ] After the refusal, the session memo holds that `provider_id`, and every
      subsequent call to it in the same session resolves to
      `Omit(RefusedThisSession)` and issues **one** request with no reasoning
      field. A test asserts the third call produces exactly one request — this is
      the OQ-6 / ADR-F saving, and without it the fallback costs a doubled request
      for the life of the session.
- [ ] The memo is **session-scoped**: a test asserts a second session against the
      same provider sends the effort field again (the declared shape is unchanged
      and a provider that gained support self-heals). A test asserts the on-disk
      config is **byte-identical** before and after a refusal — BR-4 forbids
      sniffing a shape from a response, and persisting one would be exactly that.
- [ ] **AC-2b**: a registered `openai-compatible` provider with **no declared
      `reasoning_shape`** sends the effort field on its first call (the ADR-E
      `effort_only` default), and against a mock answering 400 on that field
      produces the typed error and the single fallback described above.
- [ ] The session continues after the refusal via the existing degradation path —
      the turn completes and returns content. A refusal is not a turn failure.

## Technical Notes

**"Never retries silently" is not "never remembers".** BR-12 forbids making the
failing request again and hoping. ADR-F's memo does the opposite — it declines to
make a request already known to fail. Read ADR-F before changing this behavior;
the reasoning for rejecting both alternatives (400-per-call-forever, and
downgrading the declared shape) is recorded there.

**Key the memo by `provider_id`, not by endpoint.** Two providers pointing at the
same endpoint are separately configured things, and the memo must be scoped by
the key the user configured — the codebase's "a remembered grant is scoped by its
key" principle (architecture.md, REQ-563/LESSON-495).

**Do not put the memo in `Config` or in any persisted store.** It belongs beside
the session's other runtime degradation state. A grep for the memo's name must
not find it in `config.rs`.

**LESSON-447 applies**: the fallback must preserve the invariant it backs up and
fail loudly. The invariant here is "one shape per request" — preserved, since the
fallback sends zero shapes. The loudness is the typed error and the surface
rendering; a fallback that quietly stopped sending effort and displayed a level
anyway is the BUG-146/BUG-153 misattribution this REQ exists partly to avoid.
