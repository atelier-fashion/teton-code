//! The per-turn repeat ledger: an identical tool call, made twice in one turn,
//! is refused by the harness (REQ-617 BR-4/BR-5/BR-6, ADR-4).
//!
//! # What it is for
//!
//! In one turn of the 2026-09-04 transcript the model ran `ls -la` five times,
//! `cd ~/GitHub/teton-code && pwd` four times, `pwd` three times and `projects`
//! four times. Every one of those returned the same bytes it had returned
//! before, and — because the harness runs one tool per reply (BUG-147) — every
//! one cost a full model round trip. Twenty-six dispatched calls answered a
//! question that nine would have.
//!
//! The `skill` tool has refused a repeated identical invocation since REQ-587,
//! and it worked: the transcript shows exactly one `refused: repeated` and no
//! `skill` loop. No other tool had the rule. This module is that rule, for the
//! rest of them.
//!
//! # Why the harness and not the prompt
//!
//! A sentence telling the model not to repeat itself is a directive, and
//! LESSON-532 measured what directives are worth to a small local model: 0 of 3,
//! across three rounds of moving, dictating and isolating one. The loop is
//! broken by the code that dispatches, or it is not broken.
//!
//! # Identical means identical (BR-6)
//!
//! The fingerprint is the tool name plus the **canonical JSON** of the
//! arguments. `serde_json::Value` stores objects in a `BTreeMap`, so
//! serialization is key-sorted and `{"a":1,"b":2}` and `{"b":2,"a":1}` produce
//! one fingerprint — which is right, they are the same call.
//!
//! `ls -la` and `ls -la .` produce two, which is also right, and it is the
//! escape hatch: a model that genuinely wants fresh output can ask for something
//! different. The rule is about the *call*, not about whether the world moved
//! underneath it. A `shell` whose output would have changed is not exempt —
//! nothing here can know that without running it, and running it is the cost the
//! rule exists to avoid.
//!
//! # Two thresholds, and why the second one exists
//!
//! A read that answered once has answered. A **write** is different: an `edit`
//! that failed, or a `cargo build` that was re-run after a fix, is a legitimate
//! second attempt, because something happened in between that the arguments do
//! not record. So write-capable calls get one retry and are refused on the
//! third; read-only calls are refused on the second.
//!
//! An unknown `shell` verb counts as **write-capable**. That is the fail-safe
//! direction: over-classifying a write as read-only would refuse a real retry,
//! which turns a rule about waste into a rule that breaks work.

use serde_json::Value;
use std::collections::HashMap;

/// Verbs a `shell` command may lead with and still count as read-only.
///
/// Deliberately short and deliberately literal. Every entry either produces no
/// change at all or is the read half of a tool whose write half is spelled
/// differently (`git status`, `git log`). Anything not here — including anything
/// clever, anything with a redirection, anything the daemon does not recognise —
/// is write-capable and gets the second chance.
///
/// REQ-615 defines the write-verb set for its own refusal; whichever REQ lands
/// first owns the table and the other consumes it. Until then the two are
/// complements by construction: this list is closed, and everything else is a
/// write.
pub const READ_ONLY_SHELL_VERBS: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "find", "grep", "wc", "echo", "file", "stat", "which",
];

/// Two-word read-only forms, matched before the single-verb list.
///
/// `git` alone is write-capable (`git commit`, `git push`), so the read-only
/// members have to be named as pairs rather than by their first token.
pub const READ_ONLY_SHELL_PAIRS: &[&str] = &["git status", "git log", "git diff", "git show"];

/// Tools whose result cannot change within one turn, so one call is enough.
///
/// `skill` is **not** here and is not governed by this module at all: it keeps
/// REQ-587's own counter, which implements a more permissive rule (BR-6b admits
/// a repeat when another tool completed in between). Filing it here would refuse
/// calls REQ-587 deliberately allows.
pub const READ_ONLY_TOOLS: &[&str] = &["read", "glob", "grep", "projects", "teton_docs"];

/// How many identical calls are dispatched before the next one is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allowance {
    /// Read-only: the second identical call is refused.
    Once,
    /// Write-capable: the third is refused, because a retry after a real change
    /// is legitimate exactly once.
    Twice,
}

impl Allowance {
    /// The number of dispatches this allowance permits.
    #[must_use]
    pub const fn dispatches(self) -> u32 {
        match self {
            Self::Once => 1,
            Self::Twice => 2,
        }
    }
}

/// What a call is allowed, from its tool name and arguments.
///
/// The `shell` arm reads the command's leading verb; every other tool is decided
/// by name alone. Nothing here reads a *result* — the classification has to be
/// available before dispatch, which is the whole point.
#[must_use]
pub fn allowance_for(tool: &str, arguments: &Value) -> Allowance {
    if tool == "shell" {
        return shell_allowance(arguments);
    }
    if READ_ONLY_TOOLS.contains(&tool) {
        Allowance::Once
    } else {
        Allowance::Twice
    }
}

/// A `shell` call's allowance, from the verb its command leads with.
fn shell_allowance(arguments: &Value) -> Allowance {
    let Some(command) = arguments.get("command").and_then(Value::as_str) else {
        // No command at all: the tool will refuse it. Classify as write-capable
        // so a malformed call is never *also* given the stricter threshold —
        // this arm should not be where a model meets its first refusal.
        return Allowance::Twice;
    };
    let trimmed = command.trim_start();

    // A command carrying a shell operator is not the command its first verb
    // names: `ls && rm -rf x` leads with `ls`. Anything that can chain, pipe,
    // redirect, or substitute is write-capable regardless of what it opens with.
    if trimmed.contains([';', '&', '|', '>', '<', '`', '$', '(', ')']) {
        return Allowance::Twice;
    }

    let mut words = trimmed.split_whitespace();
    let Some(first) = words.next() else {
        return Allowance::Twice;
    };
    if let Some(second) = words.next() {
        let pair = format!("{first} {second}");
        if READ_ONLY_SHELL_PAIRS.contains(&pair.as_str()) {
            return Allowance::Once;
        }
    }
    if READ_ONLY_SHELL_VERBS.contains(&first) {
        Allowance::Once
    } else {
        Allowance::Twice
    }
}

/// One call, identified by what was asked rather than by what came back.
///
/// The arguments are hashed rather than stored: `tool_call_repeated` carries the
/// tool name and a count and must not carry a path, a command, or a search
/// pattern, and a ledger that never holds them cannot leak them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallFingerprint {
    tool: String,
    args: String,
}

impl CallFingerprint {
    /// Fingerprint `tool` called with `arguments`.
    #[must_use]
    pub fn new(tool: &str, arguments: &Value) -> Self {
        Self {
            tool: tool.to_owned(),
            // `to_string` on a `Value` is canonical for our purposes: object keys
            // come out of a `BTreeMap` in sorted order, so two spellings of one
            // object fingerprint alike. If `serde_json`'s `preserve_order`
            // feature is ever enabled workspace-wide, that stops being true —
            // `the_fingerprint_ignores_key_order` is the test that would go red.
            args: arguments.to_string(),
        }
    }
}

/// What the ledger says about a call that is about to be dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Dispatch it.
    First,
    /// Refuse it. `count` is how many times it has already been dispatched, and
    /// `first_result_len` is the byte length of what the first one returned.
    Refused { count: u32, first_result_len: usize },
}

/// One turn's record of what has already been called.
///
/// Per turn by construction: it is a field of the turn's own mutable state,
/// created with it and dropped with it, so BR-6's "a new prompt turn starts an
/// empty ledger" is true because there is no other ledger to inherit — not
/// because something remembers to clear this one.
#[derive(Debug, Default)]
pub struct RepeatLedger {
    seen: HashMap<CallFingerprint, Entry>,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    dispatched: u32,
    first_result_len: usize,
}

impl RepeatLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `tool(arguments)` may be dispatched.
    ///
    /// Read-only: consult this, then dispatch or refuse. Recording the dispatch
    /// is [`Self::record`]'s job and is deliberately a separate call — a call
    /// that is refused before it runs must not be recorded as having run, and
    /// folding the two together is how that would happen.
    #[must_use]
    pub fn verdict(&self, tool: &str, arguments: &Value) -> Verdict {
        let allowed = allowance_for(tool, arguments).dispatches();
        match self.seen.get(&CallFingerprint::new(tool, arguments)) {
            Some(entry) if entry.dispatched >= allowed => Verdict::Refused {
                count: entry.dispatched,
                first_result_len: entry.first_result_len,
            },
            _ => Verdict::First,
        }
    }

    /// Record that `tool(arguments)` was dispatched and returned
    /// `result_len` bytes.
    ///
    /// The length recorded is the **first** one, kept across later dispatches:
    /// the refusal tells the model what it already holds, and after a
    /// write-capable retry the interesting figure is still what the first call
    /// put in the conversation.
    pub fn record(&mut self, tool: &str, arguments: &Value, result_len: usize) {
        self.seen
            .entry(CallFingerprint::new(tool, arguments))
            .and_modify(|e| e.dispatched += 1)
            .or_insert(Entry {
                dispatched: 1,
                first_result_len: result_len,
            });
    }

    /// How many distinct calls this turn has dispatched, for the tests that
    /// count work rather than inspect it.
    #[must_use]
    pub fn distinct_calls(&self) -> usize {
        self.seen.len()
    }
}

/// The sentence the model reads instead of a result (BR-4).
///
/// It states three things and nothing else: that this exact call already ran,
/// how much it returned so the model can find it, and what to do instead. The
/// last clause is why this text rides **outside** the untrusted envelope — the
/// envelope's closing sentence tells the model never to act on directives in the
/// block, and this block is the harness asking it to act.
#[must_use]
pub fn refusal_message(count: u32, first_result_len: usize) -> String {
    let times = if count == 1 {
        "already ran in this turn".to_owned()
    } else {
        format!("already ran {count} times in this turn")
    };
    format!(
        "repeated: this exact call {times} and returned {first_result_len} bytes; \
         the result is above. Change the arguments or finish."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shell(command: &str) -> Value {
        json!({ "command": command })
    }

    #[test]
    fn a_read_only_tool_is_refused_on_the_second_call() {
        let mut ledger = RepeatLedger::new();
        let args = json!({ "path": "src/lib.rs" });
        assert_eq!(ledger.verdict("read", &args), Verdict::First);
        ledger.record("read", &args, 1_234);
        assert_eq!(
            ledger.verdict("read", &args),
            Verdict::Refused {
                count: 1,
                first_result_len: 1_234
            }
        );
    }

    /// **The benign path, and it is the one that matters.** A write-capable tool
    /// gets its retry: an `edit` that failed and is tried again after the model
    /// changed its mind about something the arguments do not record is a real
    /// second attempt, not a loop.
    #[test]
    fn a_write_capable_tool_is_dispatched_twice_and_refused_on_the_third() {
        let mut ledger = RepeatLedger::new();
        let args = json!({ "path": "src/lib.rs", "old": "a", "new": "b" });
        assert_eq!(ledger.verdict("edit", &args), Verdict::First);
        ledger.record("edit", &args, 10);
        assert_eq!(
            ledger.verdict("edit", &args),
            Verdict::First,
            "the second edit must dispatch — a retry after a real change is \
             legitimate exactly once"
        );
        ledger.record("edit", &args, 10);
        assert!(matches!(
            ledger.verdict("edit", &args),
            Verdict::Refused { count: 2, .. }
        ));
    }

    /// **BR-6, and the escape hatch it names.** The rule is about the call.
    #[test]
    fn identical_means_identical_and_a_changed_argument_is_a_new_call() {
        let mut ledger = RepeatLedger::new();
        ledger.record("shell", &shell("ls -la"), 500);
        assert!(matches!(
            ledger.verdict("shell", &shell("ls -la")),
            Verdict::Refused { .. }
        ));
        assert_eq!(
            ledger.verdict("shell", &shell("ls -la .")),
            Verdict::First,
            "`ls -la .` is a different call and must dispatch — this is the \
             escape hatch the refusal points the model at"
        );
    }

    /// If `serde_json`'s `preserve_order` is ever turned on, this goes red — and
    /// it is the only thing that would notice, because every other test here
    /// builds its arguments in one order.
    #[test]
    fn the_fingerprint_ignores_key_order() {
        let a = json!({ "alpha": 1, "beta": 2 });
        let b = json!({ "beta": 2, "alpha": 1 });
        assert_eq!(
            CallFingerprint::new("read", &a),
            CallFingerprint::new("read", &b),
            "two spellings of one object must fingerprint alike; they are the \
             same call"
        );
    }

    #[test]
    fn a_fresh_ledger_dispatches_what_the_last_one_refused() {
        let mut ledger = RepeatLedger::new();
        ledger.record("read", &json!({ "path": "a" }), 1);
        assert!(matches!(
            ledger.verdict("read", &json!({ "path": "a" })),
            Verdict::Refused { .. }
        ));
        let fresh = RepeatLedger::new();
        assert_eq!(
            fresh.verdict("read", &json!({ "path": "a" })),
            Verdict::First,
            "BR-6: a new prompt turn starts an empty ledger"
        );
    }

    #[test]
    fn read_only_shell_verbs_get_the_stricter_threshold() {
        for command in [
            "ls -la",
            "pwd",
            "cat README.md",
            "git status",
            "git log --oneline",
        ] {
            assert_eq!(
                allowance_for("shell", &shell(command)),
                Allowance::Once,
                "`{command}` leads with a read-only verb"
            );
        }
    }

    /// **Unknown verbs are write-capable, and so is anything that can chain.**
    ///
    /// The second half is the one worth having: `ls && rm -rf build` leads with
    /// `ls`, and a classifier that read only the first token would hand the
    /// stricter threshold to a command that deletes a directory. It would not be
    /// a *safety* bug — the threshold only decides when a repeat is refused —
    /// but it would be a classifier that is wrong about what it is looking at,
    /// and REQ-615 is about to consume this same table for a refusal where it
    /// would be one.
    #[test]
    fn an_unknown_verb_or_anything_that_can_chain_is_write_capable() {
        for command in [
            "cargo build",
            "rm -rf build",
            "git commit -m x",
            "ls && rm -rf build",
            "ls; rm -rf build",
            "ls | tee out.txt",
            "cat a > b",
            "echo `rm -rf x`",
            "echo $(rm -rf x)",
        ] {
            assert_eq!(
                allowance_for("shell", &shell(command)),
                Allowance::Twice,
                "`{command}` must be write-capable"
            );
        }
    }

    #[test]
    fn a_shell_call_with_no_command_is_write_capable() {
        assert_eq!(
            allowance_for("shell", &json!({})),
            Allowance::Twice,
            "a malformed call is the tool's to refuse; it must not also meet the \
             stricter repeat threshold on its way there"
        );
    }

    /// `skill` is governed by REQ-587's counter, not by this module. Filing it
    /// under `READ_ONLY_TOOLS` would refuse the repeats BR-6b deliberately
    /// admits, so the absence is asserted rather than left to inspection.
    #[test]
    fn skill_is_not_in_the_read_only_table() {
        assert!(!READ_ONLY_TOOLS.contains(&"skill"));
    }

    #[test]
    fn the_refusal_names_the_bytes_and_what_to_do() {
        let message = refusal_message(1, 4_096);
        assert!(message.starts_with("repeated:"), "{message}");
        assert!(message.contains("4096 bytes"), "{message}");
        assert!(message.contains("the result is above"), "{message}");
        assert!(
            message.contains("Change the arguments or finish."),
            "{message}"
        );
        // A count above one reads correctly too — the write-capable arm reaches
        // this with two dispatches behind it.
        assert!(refusal_message(2, 1).contains("already ran 2 times"));
    }
}
