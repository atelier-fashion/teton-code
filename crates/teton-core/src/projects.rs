//! The project registry's value types and its ranking (REQ-584 Leg A, ADR-1).
//!
//! **Pure, and deliberately so.** Nothing here touches the filesystem: the
//! daemon owns the file (`tetond::projects`) and the CLI must never read it at
//! all (the REQ's Permissions table). What both sides need is one *vocabulary*
//! for the same facts, and one ranking they cannot come to disagree about —
//! which is exactly the split `session_root::classify` already uses, and for
//! the same reason: the part with judgement in it is testable with no temp dir.
//!
//! The registry is a **cache of a fact the daemon already computes**. Every
//! session is created with a root and REQ-583 classifies it; recording the
//! `project`-kind ones is bookkeeping on a value already in hand. That is what
//! makes BR-3's "the scan is never a walk of home" the default rather than a
//! restraint — most of the answer arrives without looking for it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The most entries the registry keeps (REQ-584 BR-2, ADR-3).
///
/// Chosen against the *ranking*, not against memory. A machine with more than
/// 128 live checkouts is one where a name query is the only usable surface
/// anyway, and the entries that fall off are the least recently used — by
/// construction the ones a query is least likely to want. At roughly 200 B per
/// entry the whole document stays under 30 KiB, which is what lets ADR-2
/// rewrite it whole instead of growing a compaction rule.
///
/// BR-2 calls the exhaustion **silent**: the oldest `last_seen` goes, and
/// nothing is said. A diagnostic here would be a line about the tool's own
/// bookkeeping on a surface that exists to answer "where is my repo".
pub const MAX_KNOWN_PROJECTS: usize = 128;

/// How a project came to be known (REQ-584 System Model).
///
/// The distinction is a **ranking** key, not a permission one: a directory the
/// user has actually worked in is a better answer to "where is my X" than one
/// a scan happened to find, even when both match the query equally well.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSource {
    /// A session was created at it, or moved to it. Ranks first.
    Launched,
    /// The dev-folder scan found it and nobody has used it yet.
    Scanned,
    /// A source this build does not know.
    ///
    /// Tolerant for the same reason BUG-186 made the outcome enums tolerant:
    /// this value travels into a **cosmetic** surface, and a registry file
    /// written by a newer daemon must not make an older one refuse to read its
    /// own project list. It ranks last, behind everything this build
    /// understands.
    #[serde(other)]
    Unknown,
}

/// One project this machine is known to hold (REQ-584 System Model).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownProject {
    /// Absolute path to the directory that held a project marker.
    pub path: PathBuf,
    /// The path's basename. **Not unique** — two `api/` checkouts may both be
    /// known, which is why BR-6's recipe falls back to `/cd <path>`.
    pub name: String,
    /// How it became known.
    pub source: ProjectSource,
    /// Unix seconds when it was first recorded.
    pub first_seen: u64,
    /// Unix seconds of the most recent create/`cd` that landed here. The
    /// primary recency key, and the LRU key the cap evicts by.
    pub last_seen: u64,
    /// Session creates and `/cd`s that landed here. Secondary ranking key.
    pub uses: u32,
}

impl KnownProject {
    /// A freshly recorded entry.
    #[must_use]
    pub fn new(path: PathBuf, source: ProjectSource, now: u64) -> Self {
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        Self {
            path,
            name,
            source,
            first_seen: now,
            last_seen: now,
            // A scanned entry has not been *used*; only a launch is a use.
            uses: u32::from(matches!(source, ProjectSource::Launched)),
        }
    }
}

/// How well a query matched a project (REQ-584 ADR-7).
///
/// Ordered best-first, so the derived `Ord` **is** the ranking's first key and
/// no separate rank number has to be kept in agreement with the variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchClass {
    /// The name is the query.
    Exact,
    /// The name starts with the query.
    Prefix,
    /// The name contains the query.
    Substring,
    /// Some path segment other than the name contains the query.
    PathSegment,
}

impl MatchClass {
    /// How `query` matches `project`, or `None` when it does not.
    ///
    /// Case-insensitive throughout (the REQ's ProjectQuery row). An empty query
    /// is not a match *class* at all — [`ProjectRegistry::rank`] treats "no
    /// query" as "everything, unranked by class" rather than routing it here,
    /// so this never has to invent a class for the absence of a question.
    #[must_use]
    pub fn of(project: &KnownProject, query: &str) -> Option<Self> {
        let q = query.to_lowercase();
        if q.is_empty() {
            return None;
        }
        let name = project.name.to_lowercase();
        if name == q {
            return Some(Self::Exact);
        }
        if name.starts_with(&q) {
            return Some(Self::Prefix);
        }
        if name.contains(&q) {
            return Some(Self::Substring);
        }
        // Path segments other than the basename — `~/src/teton/api` answers a
        // query for `teton` even though its *name* is `api`.
        let matched_segment = project
            .path
            .iter()
            .filter_map(|seg| seg.to_str())
            .any(|seg| seg.to_lowercase().contains(&q));
        matched_segment.then_some(Self::PathSegment)
    }
}

/// Every project this machine is known to hold.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRegistry {
    /// Keyed by path so a re-record is an update rather than a duplicate, and
    /// so iteration order is deterministic before ranking even touches it.
    entries: BTreeMap<PathBuf, KnownProject>,
}

impl ProjectRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many projects are known.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is known.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every entry, in path order. Ranking is [`Self::rank`]'s job.
    pub fn iter(&self) -> impl Iterator<Item = &KnownProject> {
        self.entries.values()
    }

    /// Record a landing at `path` (BR-1), or a scan's find.
    ///
    /// **A path already present is updated, never duplicated**, and the
    /// promotion is one-directional: a `Scanned` entry becomes `Launched` the
    /// first time it is used, and a `Launched` one never demotes. A scan that
    /// re-finds a project the user has actually worked in must not throw that
    /// evidence away — it is the difference between the two ranking tiers.
    ///
    /// `uses` counts landings only, so a re-scan does not inflate it.
    pub fn record(&mut self, path: PathBuf, source: ProjectSource, now: u64) {
        match self.entries.get_mut(&path) {
            Some(existing) => {
                existing.last_seen = now;
                if matches!(source, ProjectSource::Launched) {
                    existing.source = ProjectSource::Launched;
                    existing.uses = existing.uses.saturating_add(1);
                }
            }
            None => {
                self.entries
                    .insert(path.clone(), KnownProject::new(path, source, now));
            }
        }
        self.enforce_cap();
    }

    /// Drop every entry `alive` rejects (BR-2).
    ///
    /// The predicate is supplied rather than computed here because "does this
    /// path still hold a project marker" is filesystem knowledge and this
    /// module has none — which is the whole of ADR-1.
    pub fn prune(&mut self, alive: &mut dyn FnMut(&Path) -> bool) {
        self.entries.retain(|path, _| alive(path));
    }

    /// Evict by `last_seen` until the cap holds (BR-2, ADR-3).
    fn enforce_cap(&mut self) {
        while self.entries.len() > MAX_KNOWN_PROJECTS {
            // The oldest landing goes. `min_by_key` over a BTreeMap is stable
            // in path order, so a tie on `last_seen` evicts deterministically
            // rather than by hash order (LESSON-540's shape).
            let Some(victim) = self
                .entries
                .values()
                .min_by_key(|entry| (entry.last_seen, entry.path.clone()))
                .map(|entry| entry.path.clone())
            else {
                break;
            };
            self.entries.remove(&victim);
        }
    }

    /// The projects matching `query`, best first (ADR-7).
    ///
    /// **A total order, and the last key is what makes it one.** Match class,
    /// then source, then `last_seen` descending, then `uses` descending, then
    /// **path ascending**. Without that final tiebreak two entries equal on
    /// every other key would come out in whatever order the map yielded them,
    /// and an assertion about "which one is first" would pass on one platform
    /// and fail on another — LESSON-540, which is why REQ-585's discovery
    /// sorts before it caps.
    ///
    /// `None` for the query means "everything", ranked by the remaining keys.
    #[must_use]
    pub fn rank(&self, query: Option<&str>) -> Vec<&KnownProject> {
        let mut matched: Vec<(Option<MatchClass>, &KnownProject)> = match query {
            Some(q) if !q.is_empty() => self
                .entries
                .values()
                .filter_map(|p| MatchClass::of(p, q).map(|c| (Some(c), p)))
                .collect(),
            _ => self.entries.values().map(|p| (None, p)).collect(),
        };
        matched.sort_by(|(a_class, a), (b_class, b)| {
            a_class
                .cmp(b_class)
                .then_with(|| a.source.cmp(&b.source))
                .then_with(|| b.last_seen.cmp(&a.last_seen))
                .then_with(|| b.uses.cmp(&a.uses))
                .then_with(|| a.path.cmp(&b.path))
        });
        matched.into_iter().map(|(_, p)| p).collect()
    }

    /// The single project `name` resolves to, for BR-8's `/cd <name>`.
    ///
    /// Three outcomes rather than an `Option`, because the caller must tell
    /// "nothing by that name" from "several" — they are different refusals and
    /// only one of them lists candidates.
    #[must_use]
    pub fn resolve_name(&self, name: &str) -> NameResolution<'_> {
        let wanted = name.to_lowercase();
        let hits: Vec<&KnownProject> = self
            .rank(None)
            .into_iter()
            .filter(|p| p.name.to_lowercase() == wanted)
            .collect();
        match hits.len() {
            0 => NameResolution::None,
            1 => NameResolution::Unique(hits[0]),
            _ => NameResolution::Ambiguous(hits),
        }
    }
}

/// What a bare name resolved to in the registry (BR-8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameResolution<'a> {
    /// No known project carries that name.
    None,
    /// Exactly one — the `/cd` moves.
    Unique(&'a KnownProject),
    /// Several, ranked. The caller prints them and moves nowhere.
    Ambiguous(Vec<&'a KnownProject>),
}

/// One dev folder the locator looked in, and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookedIn {
    /// The folder's display spelling (`~`-relative where it applies).
    pub display: String,
    /// How many known projects sit under it.
    pub count: usize,
}

/// Everything a `projects` answer says (REQ-584 System Model's LocatorView).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocatorView {
    /// The matches, best first, already ranked and bounded by the caller.
    pub matches: Vec<LocatorRow>,
    /// The dev folders that exist, with their project counts.
    pub looked_in: Vec<LookedIn>,
    /// Whether the scan that produced this stopped at its budget.
    pub stopped_early: bool,
    /// The query this answers, for the empty-result sentence.
    pub query: Option<String>,
}

/// One rendered match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatorRow {
    /// The project's name, bounded and neutralised by the caller.
    pub name: String,
    /// Its display path, bounded and neutralised by the caller.
    pub display: String,
    /// `launched` or `scanned`, in words.
    pub source: &'static str,
    /// How long ago it was last used, in words (`2 h ago`).
    pub last_used: String,
    /// The recipe that moves the session there — `/cd <name>`, or `/cd <path>`
    /// when the name is ambiguous (BR-6).
    pub recipe: String,
}

/// **The one renderer** for a locator answer (BR-6, BR-9).
///
/// The `projects` tool returns this text and `/projects` renders these same
/// facts — REQ-582's one-renderer rule, and the reason it is here rather than
/// in either caller: two composers of "where your projects are" would drift the
/// moment one of them gained a field (LESSON-546).
///
/// The CLI may **style** the result — colour, indentation, a header — but the
/// facts and their wording are this function's.
#[must_use]
pub fn render_locator(view: &LocatorView) -> String {
    let mut out = String::new();

    if view.matches.is_empty() {
        match &view.query {
            Some(q) => out.push_str(&format!("no known project matches `{q}`")),
            None => out.push_str("no known projects yet"),
        }
        if view.looked_in.is_empty() {
            out.push_str("; no development folders were found on this machine.\n");
        } else {
            out.push_str("; looked in: ");
            out.push_str(
                &view
                    .looked_in
                    .iter()
                    .map(|f| f.display.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            out.push_str(".\n");
        }
    } else {
        for row in &view.matches {
            out.push_str(&format!(
                "{} — {} ({}, last used {})  →  {}\n",
                row.name, row.display, row.source, row.last_used, row.recipe
            ));
        }
        if !view.looked_in.is_empty() {
            out.push_str("\ndevelopment folders: ");
            out.push_str(
                &view
                    .looked_in
                    .iter()
                    .map(|f| format!("{} ({})", f.display, f.count))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            out.push('\n');
        }
    }

    // A budget stop is reported the way a tool walk reports one: a partial
    // answer that does not say it is partial is the failure REQ-583 exists to
    // prevent, and this surface is the one a user asks "where is my repo" at.
    if view.stopped_early {
        out.push_str(
            "\nthe scan stopped at its budget, so this list may be incomplete; \
             launch teton from a project once and it is remembered.\n",
        );
    }
    // REQ-615 BR-8. Every row above ends in a `/cd <name>` recipe, and the
    // 2026-09-04 session read those recipes as instructions it could carry out
    // — it called `projects`, then went back to `shell: cd …`. One line closes
    // that reading: the recipe names an act, and the act is the user's.
    //
    // Last, after the budget notice, so it is the final thing read and no list
    // can push it out; unconditional, because a listing with no matches still
    // carries `/cd` in the sentence that says how a project becomes known.
    out.push_str(PROJECTS_ARE_THE_USERS_TO_MOVE_TO);
    out
}

/// REQ-615 BR-8's trailing line — the sentence that turns the `/cd` recipes
/// above it from something to run into something to ask for.
///
/// A named constant so the test that pins it and the renderer that emits it are
/// one string: AC-7 requires a mutation deleting it to fail, and a hand-typed
/// copy in the test would keep passing against a renderer that had dropped it.
pub const PROJECTS_ARE_THE_USERS_TO_MOVE_TO: &str = "\nOnly the user can run `/cd`. Ask them.\n";

/// How long ago `then` was, in the words the locator uses.
///
/// Coarse on purpose — the reader is choosing between projects, not auditing.
/// Saturating on a `then` in the future (a clock that moved backwards) rather
/// than underflowing.
#[must_use]
pub fn relative_time(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..=59 => "just now".to_owned(),
        60..=3_599 => format!("{} m ago", secs / 60),
        3_600..=86_399 => format!("{} h ago", secs / 3_600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(path: &str, source: ProjectSource, last_seen: u64, uses: u32) -> KnownProject {
        let mut p = KnownProject::new(PathBuf::from(path), source, last_seen);
        p.last_seen = last_seen;
        p.uses = uses;
        p
    }

    fn registry(entries: Vec<KnownProject>) -> ProjectRegistry {
        let mut r = ProjectRegistry::new();
        for e in entries {
            r.entries.insert(e.path.clone(), e);
        }
        r
    }

    /// The name comes off the path, and a scanned entry is not a use.
    #[test]
    fn a_new_entry_takes_its_name_from_the_path_and_counts_only_launches() {
        let launched =
            KnownProject::new(PathBuf::from("/a/teton-code"), ProjectSource::Launched, 5);
        assert_eq!(launched.name, "teton-code");
        assert_eq!(launched.uses, 1, "a launch is a use");
        assert_eq!(launched.first_seen, 5);

        let scanned = KnownProject::new(PathBuf::from("/a/other"), ProjectSource::Scanned, 5);
        assert_eq!(
            scanned.uses, 0,
            "a scan found it; nobody has worked in it yet — that is the \
             difference the two ranking tiers are built on"
        );
    }

    /// **BR-1.** Re-recording a path updates it; promotion is one-directional.
    #[test]
    fn recording_the_same_path_updates_it_and_never_demotes_a_launch() {
        let mut r = ProjectRegistry::new();
        r.record(PathBuf::from("/a/x"), ProjectSource::Scanned, 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r.iter().next().unwrap().uses, 0);

        // Used for the first time: promoted, and counted.
        r.record(PathBuf::from("/a/x"), ProjectSource::Launched, 20);
        let e = r.iter().next().unwrap();
        assert_eq!(r.len(), 1, "an update, not a duplicate");
        assert_eq!(e.source, ProjectSource::Launched);
        assert_eq!(e.uses, 1);
        assert_eq!(e.last_seen, 20);
        assert_eq!(e.first_seen, 10, "first_seen is not touched by a re-record");

        // A later scan re-finds it. It must NOT throw the launch evidence away.
        r.record(PathBuf::from("/a/x"), ProjectSource::Scanned, 30);
        let e = r.iter().next().unwrap();
        assert_eq!(
            e.source,
            ProjectSource::Launched,
            "a re-scan must not demote a project the user has actually worked in"
        );
        assert_eq!(e.uses, 1, "and a scan is not a use");
        assert_eq!(e.last_seen, 30, "but it is still evidence of liveness");
    }

    /// **BR-2 / ADR-3.** The cap evicts the oldest landing, silently.
    #[test]
    fn the_cap_drops_the_oldest_last_seen() {
        let mut r = ProjectRegistry::new();
        for i in 0..MAX_KNOWN_PROJECTS {
            r.record(
                PathBuf::from(format!("/a/p{i:03}")),
                ProjectSource::Launched,
                1_000 + i as u64,
            );
        }
        assert_eq!(r.len(), MAX_KNOWN_PROJECTS);

        r.record(PathBuf::from("/a/newest"), ProjectSource::Launched, 9_999);
        assert_eq!(r.len(), MAX_KNOWN_PROJECTS, "the cap holds");
        assert!(
            r.iter().all(|e| e.path != Path::new("/a/p000")),
            "the oldest last_seen is the one that goes"
        );
        assert!(r.iter().any(|e| e.path == Path::new("/a/newest")));
    }

    /// **BR-2.** Pruning is driven by a supplied predicate — this module has no
    /// filesystem knowledge and must not grow any (ADR-1).
    #[test]
    fn prune_drops_what_the_predicate_rejects() {
        let mut r = registry(vec![
            at("/a/live", ProjectSource::Launched, 10, 1),
            at("/a/dead", ProjectSource::Launched, 20, 1),
        ]);
        r.prune(&mut |p| p != Path::new("/a/dead"));
        assert_eq!(r.len(), 1);
        assert_eq!(r.iter().next().unwrap().path, Path::new("/a/live"));
    }

    /// **ADR-7 / AC-6.** Match class orders the ranking's first key.
    #[test]
    fn match_class_orders_exact_prefix_substring_then_path_segment() {
        let exact = at("/a/teton", ProjectSource::Scanned, 1, 0);
        let prefix = at("/a/teton-code", ProjectSource::Scanned, 1, 0);
        let substring = at("/a/my-teton-notes", ProjectSource::Scanned, 1, 0);
        let segment = at("/a/teton/api", ProjectSource::Scanned, 1, 0);
        let miss = at("/a/unrelated", ProjectSource::Scanned, 1, 0);

        assert_eq!(MatchClass::of(&exact, "teton"), Some(MatchClass::Exact));
        assert_eq!(MatchClass::of(&prefix, "teton"), Some(MatchClass::Prefix));
        assert_eq!(
            MatchClass::of(&substring, "teton"),
            Some(MatchClass::Substring)
        );
        assert_eq!(
            MatchClass::of(&segment, "teton"),
            Some(MatchClass::PathSegment),
            "a parent segment answers the query even when the name does not"
        );
        assert_eq!(MatchClass::of(&miss, "teton"), None);

        // Case-insensitive, and an empty query is not a class.
        assert_eq!(MatchClass::of(&exact, "TeToN"), Some(MatchClass::Exact));
        assert_eq!(MatchClass::of(&exact, ""), None);

        // The derived Ord IS the ranking key — no separate rank number to keep
        // in agreement with the variants.
        assert!(MatchClass::Exact < MatchClass::Prefix);
        assert!(MatchClass::Prefix < MatchClass::Substring);
        assert!(MatchClass::Substring < MatchClass::PathSegment);
    }

    /// **ADR-7, the whole order — and that it is TOTAL.**
    ///
    /// Eight rows with a deliberate tie on every key in turn, so each tiebreak
    /// is the one thing deciding its pair. The last leg is the point: feeding
    /// the same set in a different order must produce the same ranking, which
    /// is only true if the final path key exists.
    #[test]
    fn rank_is_a_total_order_and_the_path_tiebreak_is_what_makes_it_one() {
        let entries = vec![
            // class decides: exact beats prefix, though it is older and scanned
            at("/z/api", ProjectSource::Scanned, 1, 0),
            at("/z/api-gateway", ProjectSource::Launched, 900, 90),
            // source decides: same class (prefix), same recency, same uses
            at("/y/apix-a", ProjectSource::Launched, 500, 5),
            at("/y/apix-b", ProjectSource::Scanned, 500, 5),
            // last_seen decides: same class and source
            at("/x/apiy-new", ProjectSource::Launched, 800, 1),
            at("/x/apiy-old", ProjectSource::Launched, 100, 1),
            // uses decides: same class, source, recency
            at("/w/apiz-many", ProjectSource::Launched, 300, 50),
            at("/w/apiz-few", ProjectSource::Launched, 300, 2),
        ];

        let forward = registry(entries.clone());
        let ranked: Vec<&str> = forward
            .rank(Some("api"))
            .iter()
            .map(|p| p.name.as_str())
            .collect();

        assert_eq!(ranked[0], "api", "match class outranks everything else");
        let pos = |n: &str| ranked.iter().position(|x| *x == n).unwrap();
        assert!(pos("apix-a") < pos("apix-b"), "launched outranks scanned");
        assert!(pos("apiy-new") < pos("apiy-old"), "recency decides next");
        assert!(pos("apiz-many") < pos("apiz-few"), "then uses");

        // Totality. Re-insert in reverse; a ranking without the final path key
        // would be free to differ here.
        let mut reversed_entries = entries;
        reversed_entries.reverse();
        let reversed = registry(reversed_entries);
        let ranked_again: Vec<&str> = reversed
            .rank(Some("api"))
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(
            ranked, ranked_again,
            "the order must not depend on insertion order — that is what the \
             path tiebreak buys, and without it this is platform-flaky"
        );
    }

    /// A tie on literally every key but the path is broken by the path.
    #[test]
    fn two_entries_equal_on_every_other_key_rank_by_path() {
        let r = registry(vec![
            at("/b/same", ProjectSource::Launched, 100, 3),
            at("/a/same", ProjectSource::Launched, 100, 3),
        ]);
        let ranked: Vec<&PathBuf> = r.rank(None).iter().map(|p| &p.path).collect();
        assert_eq!(
            ranked,
            vec![&PathBuf::from("/a/same"), &PathBuf::from("/b/same")],
            "ascending path is the last resort, and it must be deterministic"
        );
    }

    /// No query means everything, ranked by the remaining keys.
    #[test]
    fn no_query_ranks_everything_by_recency_then_uses() {
        let r = registry(vec![
            at("/a/old", ProjectSource::Launched, 10, 9),
            at("/a/new", ProjectSource::Launched, 99, 1),
        ]);
        let ranked: Vec<&str> = r.rank(None).iter().map(|p| p.name.as_str()).collect();
        assert_eq!(ranked, vec!["new", "old"]);
        assert_eq!(r.rank(Some("")).len(), 2, "an empty query is not a filter");
    }

    /// **BR-8.** The three name outcomes are distinguishable.
    #[test]
    fn resolve_name_tells_none_from_one_from_several() {
        let r = registry(vec![
            at("/a/teton-code", ProjectSource::Launched, 10, 1),
            at("/one/api", ProjectSource::Launched, 20, 1),
            at("/two/api", ProjectSource::Scanned, 30, 0),
        ]);
        assert!(matches!(r.resolve_name("nothing"), NameResolution::None));
        assert!(matches!(
            r.resolve_name("teton-code"),
            NameResolution::Unique(p) if p.path == Path::new("/a/teton-code")
        ));
        match r.resolve_name("api") {
            NameResolution::Ambiguous(hits) => {
                assert_eq!(hits.len(), 2);
                assert_eq!(
                    hits[0].path,
                    Path::new("/one/api"),
                    "candidates arrive ranked, so the printed list has an order"
                );
            }
            other => panic!("two projects named `api` must be ambiguous: {other:?}"),
        }
        // Case-insensitive, like every other match in this module.
        assert!(matches!(
            r.resolve_name("TETON-CODE"),
            NameResolution::Unique(_)
        ));
    }

    /// **ADR-1, as a fact about the code.** This module does no I/O.
    ///
    /// The split is only worth anything if it holds: the CLI links this crate
    /// and must never be able to read the registry file through it. A doc
    /// comment saying so cannot fail a build, so this reads the source — the
    /// shape `boundary_coverage` uses.
    ///
    /// Fails **open** on a read error rather than panicking: a source scan that
    /// aborts when `src/` changes under it is BUG-159, and this assertion is
    /// not worth reintroducing that.
    #[test]
    fn this_module_touches_no_filesystem() {
        let Ok(src) =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/projects.rs"))
        else {
            return;
        };
        // The test module itself legitimately reads its own source, right here.
        let body = src.split("#[cfg(test)]").next().unwrap_or(&src);
        for forbidden in ["std::fs", "File::", "read_dir", "OpenOptions"] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` appears in the pure half of this module — the \
                 daemon owns the file and the CLI must not be able to reach it \
                 through here (ADR-1)"
            );
        }
    }

    /// A registry written by a newer daemon still loads (BUG-186's lesson).
    #[test]
    fn an_unknown_source_degrades_rather_than_failing_the_document() {
        let json = r#"{"entries":{"/a/x":{"path":"/a/x","name":"x","source":"teleported","first_seen":1,"last_seen":2,"uses":0}}}"#;
        let r: ProjectRegistry =
            serde_json::from_str(json).expect("an unknown source must not fail the registry");
        assert_eq!(r.iter().next().unwrap().source, ProjectSource::Unknown);
        assert!(
            ProjectSource::Launched < ProjectSource::Unknown
                && ProjectSource::Scanned < ProjectSource::Unknown,
            "and it ranks behind everything this build understands"
        );
    }
}
