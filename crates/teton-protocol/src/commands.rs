//! The session's built-in command roster, as a fact the daemon can state
//! (REQ-617 BR-1, ADR-1).
//!
//! # Why this exists at all
//!
//! Asked *"is transcript on?"*, the model had no way to answer. The self-config
//! guide named `/provider`, `/policy`, `/web`, `/doctor` and `/help`; nothing in
//! the prompt named `/transcript`, `/cd`, `/clear`, `/effort`, `/permissions`,
//! `/model` or `/boundary`. So it did what a model does with a question it has no
//! data for: it searched the repository for seven tool calls, read a Claude Code
//! file, and reported that file's setting as Teton's. Every part of that is
//! downstream of one missing fact.
//!
//! # Why a derived roster and not the CLI's own table
//!
//! [`crate`] is the one crate both binaries depend on, and the daemon is the
//! thing that builds the prompt. The CLI's `slash::COMMANDS` cannot move here:
//! each of its rows carries a `handler: fn(&mut Connection, &mut UiContext, &str)
//! -> Result<CommandOutcome>` and a `mirror: Option<Mirror>`, both naming CLI
//! types, and `teton` depends on `tetond` rather than the reverse — so the move
//! would invert the dependency graph, not merely relocate a constant.
//!
//! What the daemon needs is three strings per row and no function pointers. That
//! is what this is. It is the architecture's "declared identity over derived
//! identity" rule applied to a command name: the roster *declares* what may be
//! said about a command, and nothing re-derives it from the other subsystem's
//! table.
//!
//! # Where drift is caught, and why it is caught there
//!
//! Only the CLI crate can see both tables, so the guard lives in `slash.rs`'s
//! test block and asserts the two name sets are **equal** — not that one contains
//! the other. Both directions matter and they fail differently:
//!
//! - a `CommandSpec` with no [`SessionCommand`] is a command the model never
//!   learns about, which is the whole defect this REQ exists to fix;
//! - a [`SessionCommand`] with no `CommandSpec` is worse — the model confidently
//!   names a command that does not exist, and the user types it and is told it is
//!   unknown.
//!
//! A one-directional guard would catch one of those and leave the other open,
//! which is the shape BUG-149 took (an input tag with no output marker, invisible
//! to a subset assertion until a human noticed).
//!
//! # What is deliberately not here
//!
//! **Aliases.** `/quit` has `/exit`; the roster carries only the canonical name.
//! An alias is a way to *type* a command, and this roster's consumers exist to
//! name one command for the user to type. Offering a small model two spellings of
//! one thing is how it invents a third. `/help` collapses aliases for the same
//! reason (REQ-582 BR-7).
//!
//! **Argument grammar.** The `effect` clause mentions an argument only where the
//! command is unusable without knowing it. The authority on argument shape is
//! clap, at the moment of parsing, in its own words (REQ-582 AC-7) — a second
//! hand-written account of it here would be a second thing to keep true.

/// One built-in session command, as the daemon may describe it.
///
/// Three strings and a bool: everything the prompt, the `teton_docs commands`
/// page and the session-state nudge need, and nothing that names a CLI type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCommand {
    /// The name typed after the `/`, without the slash. May contain a space
    /// (`model set`), matching the CLI's own longest-match dispatch.
    pub name: &'static str,
    /// One clause saying what it does. No trailing period: the renderers punctuate,
    /// because a clause that arrives pre-punctuated cannot be joined into a list.
    pub effect: &'static str,
    /// Whether only the user may run it.
    ///
    /// **Every row is `true` today, and the field is still worth its bytes.** The
    /// roster's entire claim is *the model names these and the user runs them*,
    /// and a claim carried by a field can be asserted by a test; a claim carried
    /// by a comment cannot. If a command ever becomes model-runnable, the row that
    /// says so is the row a reader checks — not the prose around it.
    pub user_only: bool,
}

impl SessionCommand {
    /// The name as the user types it, slash included.
    #[must_use]
    pub fn spelling(&self) -> String {
        format!("/{}", self.name)
    }
}

/// Every built-in session command, in `/help` order.
///
/// Mirrors `teton::slash::COMMANDS` row for row; the equality guard lives in that
/// module's tests, because it is the only place both tables are visible.
pub const SESSION_COMMANDS: &[SessionCommand] = &[
    SessionCommand {
        name: "help",
        effect: "list the commands this session knows",
        user_only: true,
    },
    SessionCommand {
        name: "cost",
        effect: "show the cost report for this machine",
        user_only: true,
    },
    SessionCommand {
        name: "effort",
        effect: "show or set the reasoning effort: /effort [low|medium|high|xhigh|max]",
        user_only: true,
    },
    SessionCommand {
        name: "model",
        effect: "show the model the local tier is on",
        user_only: true,
    },
    SessionCommand {
        name: "model set",
        effect: "switch the local tier to a catalog model: /model set <name>",
        user_only: true,
    },
    SessionCommand {
        name: "model list",
        effect: "show the model catalog and each entry's fit for this machine",
        user_only: true,
    },
    SessionCommand {
        name: "model status",
        effect: "report the recorded model decision and the weights' install state",
        user_only: true,
    },
    SessionCommand {
        name: "clear",
        effect: "drop this session's retained conversation; the next prompt starts fresh",
        user_only: true,
    },
    SessionCommand {
        name: "cd",
        effect: "move this session's root — the directory tools are scoped to; bare, print it",
        user_only: true,
    },
    SessionCommand {
        name: "projects",
        effect: "list the projects this machine knows, each with the /cd that moves there",
        user_only: true,
    },
    SessionCommand {
        name: "verbose",
        effect: "toggle the routing and turn-end notices for this session",
        user_only: true,
    },
    SessionCommand {
        name: "transcript",
        effect: "record this session to a file, or stop: /transcript [on|off]; bare, show the state",
        user_only: true,
    },
    SessionCommand {
        name: "context",
        effect: "carry this repository's notes in the prompt, or stop: /context [on|off]; bare, show the state",
        user_only: true,
    },
    SessionCommand {
        name: "context init",
        effect: "write this repository's TETON.md now: /context init [--force] (asks first)",
        user_only: true,
    },
    SessionCommand {
        name: "permissions",
        effect: "show or set this session's permission level: /permissions [level]",
        user_only: true,
    },
    SessionCommand {
        name: "web setup",
        effect: "set up web lookup: pick a tier, name a backend, confirm before anything is written",
        user_only: true,
    },
    SessionCommand {
        name: "web allow",
        effect: "lift this session's web taint restriction; grants no new tier",
        user_only: true,
    },
    SessionCommand {
        name: "web refresh",
        effect: "drop a URL's cached copy so the next lookup re-fetches: /web refresh <url>",
        user_only: true,
    },
    SessionCommand {
        name: "shell allow",
        effect: "lift this session's local-tier pin after an unknown-reach shell command; typed input only",
        user_only: true,
    },
    SessionCommand {
        name: "provider setup",
        effect: "register a provider and route a tier to it: /provider setup [vendor] [tier]",
        user_only: true,
    },
    SessionCommand {
        name: "provider test",
        effect: "test a registered provider with one consented call: /provider test <id>",
        user_only: true,
    },
    SessionCommand {
        name: "provider list",
        effect: "list the providers registered on this machine, with what each one calls",
        user_only: true,
    },
    SessionCommand {
        name: "provider add",
        effect: "register a provider by hand; the key is asked for, never typed on the line",
        user_only: true,
    },
    SessionCommand {
        name: "boundary list",
        effect: "list the privacy boundaries: path globs whose content never leaves this machine",
        user_only: true,
    },
    SessionCommand {
        name: "boundary add",
        effect: "add a privacy boundary over a path glob: /boundary add <glob>",
        user_only: true,
    },
    SessionCommand {
        name: "policy show",
        effect: "show the effective routing table and where each tier and category resolves",
        user_only: true,
    },
    SessionCommand {
        name: "policy set-tier",
        effect: "route a tier to a provider: /policy set-tier <tier> <provider>",
        user_only: true,
    },
    SessionCommand {
        name: "policy set-category",
        effect: "route one category ahead of its tier: /policy set-category <category> <provider>",
        user_only: true,
    },
    SessionCommand {
        name: "doctor",
        effect: "diagnose the daemon, socket, model state and providers",
        user_only: true,
    },
    SessionCommand {
        name: "quit",
        effect: "end the session, exactly as Ctrl-D does",
        user_only: true,
    },
];

/// Look a command up by its canonical name.
#[must_use]
pub fn find(name: &str) -> Option<&'static SessionCommand> {
    SESSION_COMMANDS.iter().find(|c| c.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The properties every row must hold, checked here rather than at each
    /// consumer — a renderer that trusted these would otherwise each need its own
    /// copy of the check.
    #[test]
    fn every_row_is_well_formed() {
        for c in SESSION_COMMANDS {
            assert!(!c.name.is_empty(), "an empty name");
            assert!(
                !c.name.starts_with('/'),
                "`{}` carries its own slash; the renderers add it, or a page \
                 ends up with `//transcript`",
                c.name
            );
            assert!(
                !c.effect.is_empty(),
                "`{}` has no effect clause, so `teton_docs commands` would print \
                 a bare name and the model would have learned nothing about it",
                c.name
            );
            assert!(
                !c.effect.ends_with('.'),
                "`{}`'s effect ends with a period; the renderers punctuate, and a \
                 pre-punctuated clause cannot be joined into a list",
                c.name
            );
            assert!(
                c.effect.starts_with(|ch: char| ch.is_lowercase()),
                "`{}`'s effect starts with a capital; it is a clause, not a \
                 sentence, and it is rendered mid-line",
                c.name
            );
        }
    }

    /// Names are unique, and the check is on the whole set rather than on
    /// neighbours: `model` and `model set` are different rows and both are valid,
    /// so a sorted-adjacent-duplicate check would have to know that.
    #[test]
    fn names_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for c in SESSION_COMMANDS {
            assert!(seen.insert(c.name), "`{}` appears twice", c.name);
        }
        assert_eq!(seen.len(), SESSION_COMMANDS.len());
    }

    /// The roster's whole claim, asserted rather than narrated.
    #[test]
    fn every_command_is_the_users_to_run() {
        for c in SESSION_COMMANDS {
            assert!(
                c.user_only,
                "`{}` says the model may run it. Nothing in this session \
                 dispatches a built-in command from a model, so either the \
                 daemon grew a way to and this roster is now the thing that \
                 says so, or this row is wrong.",
                c.name
            );
        }
    }

    #[test]
    fn spelling_adds_the_slash_and_find_round_trips() {
        assert_eq!(find("transcript").unwrap().spelling(), "/transcript");
        assert_eq!(find("model set").unwrap().spelling(), "/model set");
        assert!(find("nonesuch").is_none());
    }
}
