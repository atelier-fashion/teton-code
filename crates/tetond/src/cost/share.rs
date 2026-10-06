//! Per-call spend shares for an `agent` call's children (REQ-623 ADR-4, BR-7,
//! BR-8).
//!
//! A call's children spend from the prompt's one ceiling, and the parent pays.
//! When the call starts, the headroom left under the ceiling — the ceiling less
//! what the prompt has already spent — is split equally among the call's
//! children (floor), and each child's egress checks the child's **own** spend
//! against its share. When a child ends with share left over while siblings
//! still run, the unspent amount is split equally among the running siblings
//! (floor; the remainder stays unused) and their ceilings rise. A ceiling never
//! falls.
//!
//! ## Three layers
//!
//! - **The arithmetic** — [`headroom`], [`split`], [`unspent`] and
//!   [`release_parts`] — pure functions over integers, tested as tables.
//! - **The pool** — [`SharePool`] — each child's current ceiling and whether it
//!   has ended. Its `Mutex` only sequences calls: a release reads the running
//!   set and raises it as one step, so two children ending at once cannot each
//!   count the other as still running and hand out the same headroom twice.
//! - **The wiring** — [`ChildSpend`] — what a child's egress is built with
//!   ([`Egress::with_child_spend`](crate::egress::Egress::with_child_spend)):
//!   the pool to read the ceiling from, the child's own accumulator to check
//!   against it, and the parent's accumulator to also add into.
//!
//! ## Why the ceiling is read, not stamped
//!
//! [`Egress::with_spend_ceiling`](crate::egress::Egress::with_spend_ceiling)
//! stamps a number at construction. A ceiling that rises needs a reader: a
//! sibling's release has to reach the next call of a child whose egress was
//! built before the sibling ended. So the egress asks [`SharePool::share_of`]
//! at every check.
//!
//! ## The parent pays
//!
//! Every call a child makes adds into the parent's accumulator as well as the
//! child's own, so when the call returns the parent's next model call checks
//! the real headroom on the existing `SpendCeilingReached` path — the typed
//! outcome and its arm both exist already (LESSON-557), and a child that runs
//! out of share is refused with that same outcome.
//!
//! ## Units
//!
//! Every figure here is in the unit [`PromptSpend`] accumulates. A child's own
//! accumulator is fed by the same meter the prompt's is, through the ledger's
//! one conversion, so [`ChildSpend::spent`] and
//! [`CostLedger::spent_by_child`](super::CostLedger::spent_by_child) agree for
//! the same child — pinned by
//! `a_childs_spend_reaches_its_own_and_the_parents_accumulator_in_ledger_units`.
//!
//! ## No ceiling
//!
//! A prompt with no ceiling builds an *unlimited* pool: every share is `None`,
//! nothing is checked, and nothing is released. That is a state of its own
//! rather than a share of `u64::MAX`, so "no ceiling" cannot be read back as "a
//! very large one" by anything echoing it (`ChildBounds`, `ChildResult`).

use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use futures::Stream;
use teton_protocol::agent::ChildId;
use teton_protocol::{ProviderId, SessionId, TurnId};
use teton_providers::transport::{ByteStream, TransportError, TransportResponse};

use super::{CostAttribution, CostMeter, PromptSpend};

// ---------------------------------------------------------------------------
// The arithmetic
// ---------------------------------------------------------------------------

/// What is left under `ceiling` once `spent` is gone, or `None` with no ceiling.
///
/// Saturating: a prompt whose last call overshot (REQ-588 ADR-2) has no
/// headroom, not a negative amount of it.
#[must_use]
pub fn headroom(ceiling: Option<u64>, spent: u64) -> Option<u64> {
    ceiling.map(|c| c.saturating_sub(spent))
}

/// Each child's stamped share: `floor(headroom / n)`.
///
/// The remainder is left unused rather than handed to whichever child happens
/// to come first, so every child of a call starts on the same number. Zero
/// children is zero, not a division by zero — there is nobody to stamp.
#[must_use]
pub fn split(headroom: u64, n: usize) -> u64 {
    u64::try_from(n)
        .ok()
        .and_then(|n| headroom.checked_div(n))
        .unwrap_or(0)
}

/// What a child that ended with `ceiling` after spending `spent` left unspent.
///
/// Saturating: a child whose last call carried it past its ceiling left
/// nothing, and an overshoot is never charged to its siblings.
#[must_use]
pub fn unspent(ceiling: u64, spent: u64) -> u64 {
    ceiling.saturating_sub(spent)
}

/// How `unspent` divides among `running` siblings: `(each, unused)`.
///
/// `each` is `floor(unspent / running)` and `unused` is the remainder, which
/// stays in the prompt's pool. With nobody running, the whole amount is unused
/// — there is no sibling to give it to.
#[must_use]
pub fn release_parts(unspent: u64, running: usize) -> (u64, u64) {
    match u64::try_from(running).ok().filter(|n| *n > 0) {
        Some(n) => (unspent / n, unspent % n),
        None => (0, unspent),
    }
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

/// One child's place in a [`SharePool`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChildShare {
    child: ChildId,
    /// The child's ceiling now: its stamped share plus every part it has been
    /// released. Only ever raised.
    ceiling: u64,
    /// Whether the child has reached a terminal status. An ended child's
    /// ceiling is frozen — it is the child's final ceiling — and it neither
    /// releases twice nor receives parts.
    ended: bool,
}

/// The spend shares of one `agent` call's children (ADR-4).
///
/// Created once per call, before any child starts, from the prompt's headroom
/// and the call's children; shared by every child's egress through an `Arc`.
#[derive(Debug)]
pub struct SharePool {
    /// `None` when the prompt has no ceiling: the unlimited pool, which never
    /// refuses and never releases.
    shares: Option<Mutex<Vec<ChildShare>>>,
}

impl SharePool {
    /// A pool splitting `headroom` equally among `children` (floor).
    ///
    /// `headroom` is the prompt's ceiling less what it has spent — [`headroom`]
    /// computes it — and `None` when the prompt has no ceiling, which makes
    /// every share `None`.
    #[must_use]
    pub fn new(headroom: Option<u64>, children: &[ChildId]) -> Arc<Self> {
        let shares = headroom.map(|headroom| {
            let each = split(headroom, children.len());
            Mutex::new(
                children
                    .iter()
                    .map(|child| ChildShare {
                        child: child.clone(),
                        ceiling: each,
                        ended: false,
                    })
                    .collect(),
            )
        });
        Arc::new(Self { shares })
    }

    /// `child`'s ceiling **now** — read by its egress at every check, so a
    /// release that raised it reaches the child's next call.
    ///
    /// `None` only for the unlimited pool. A child this pool was not built with
    /// gets `Some(0)`: a pool that has a ceiling never answers "no ceiling" for
    /// a child it cannot place, because that would send the child's calls
    /// uncounted. After the child ends this is its final ceiling.
    #[must_use]
    pub fn share_of(&self, child: &ChildId) -> Option<u64> {
        let shares = lock(self.shares.as_ref()?);
        Some(
            shares
                .iter()
                .find(|share| &share.child == child)
                .map_or(0, |share| share.ceiling),
        )
    }

    /// `child` reached its terminal status having spent `spent`: split what it
    /// left unspent equally among the children still running (BR-8).
    ///
    /// Returns every recipient with its new ceiling, in the order the pool was
    /// built with — what `agent_child_share_released` announces. Empty when
    /// nothing moved: the unlimited pool, a child that spent its whole share, a
    /// part that floors to zero, no sibling still running, a child the pool does
    /// not know, or a second release of the same child. The amount the event
    /// names as released is [`unspent`] of the child's final ceiling
    /// ([`Self::share_of`]) and `spent`.
    ///
    /// `spent` is the child's own spend in the accumulator's unit —
    /// [`ChildSpend::spent`], or the ledger's `spent_by_child`, which agree.
    #[must_use = "the recipients are what agent_child_share_released announces"]
    pub fn release(&self, child: &ChildId, spent: u64) -> Vec<(ChildId, u64)> {
        let Some(shares) = self.shares.as_ref() else {
            return Vec::new();
        };
        let mut shares = lock(shares);
        let Some(ending) = shares
            .iter_mut()
            .find(|share| &share.child == child && !share.ended)
        else {
            return Vec::new();
        };
        ending.ended = true;
        let left = unspent(ending.ceiling, spent);

        let running = shares.iter().filter(|share| !share.ended).count();
        let (each, _unused) = release_parts(left, running);
        if each == 0 {
            return Vec::new();
        }
        shares
            .iter_mut()
            .filter(|share| !share.ended)
            .map(|share| {
                share.ceiling = share.ceiling.saturating_add(each);
                (share.child.clone(), share.ceiling)
            })
            .collect()
    }
}

/// The pool's lock, recovered from poisoning.
///
/// Every update under it is integral arithmetic that cannot panic, so a poisoned
/// lock still holds a consistent table; refusing to read it would turn one
/// panicked thread into every child of the call losing its ceiling.
fn lock(shares: &Mutex<Vec<ChildShare>>) -> MutexGuard<'_, Vec<ChildShare>> {
    shares.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// The wiring
// ---------------------------------------------------------------------------

/// What a child's egress is built with (ADR-4): where its ceiling is read
/// from, what it is checked against, and where its spend also goes.
///
/// Cheap to clone — every field is shared — so the child runner can hold one
/// and hand clones to each egress it builds.
#[derive(Debug, Clone)]
pub struct ChildSpend {
    child: ChildId,
    pool: Arc<SharePool>,
    /// This child's own spend — what its ceiling is checked against.
    own: Arc<PromptSpend>,
    /// The prompt's accumulator, which every child call also adds into so the
    /// parent pays. `None` exactly when the prompt has no ceiling, as it is on
    /// the prompt turn.
    parent: Option<Arc<PromptSpend>>,
    /// The prompt turn the child runs under, once [`Self::under_turn`] has
    /// named it (REQ-623 BR-8): every row a choke point built with this spend
    /// meters is then written under both ids, so `/cost` nests it.
    parent_turn: Option<TurnId>,
}

impl ChildSpend {
    /// `child`'s spend, under `pool`, paid also into `parent` — the prompt
    /// turn's own accumulator, the same `Arc` every egress of that prompt holds.
    #[must_use]
    pub fn new(child: ChildId, pool: Arc<SharePool>, parent: Option<Arc<PromptSpend>>) -> Self {
        Self {
            child,
            pool,
            own: Arc::new(PromptSpend::new()),
            parent,
            parent_turn: None,
        }
    }

    /// The same spend — same pool, same accumulators — billing its calls to
    /// the child running under `parent_turn_id` (REQ-623 BR-8, AC-12).
    ///
    /// The child runner applies it once, before building any egress, so every
    /// remote call the child makes — its turn's and its duties' alike — writes
    /// a row carrying both `child_id` and `parent_turn_id`. Stamped here, at the
    /// one choke point every such call goes through, rather than at each
    /// source that builds an attribution (LESSON-501).
    #[must_use]
    pub fn under_turn(mut self, parent_turn_id: TurnId) -> Self {
        self.parent_turn = Some(parent_turn_id);
        self
    }

    /// The child this is.
    #[must_use]
    pub fn child(&self) -> &ChildId {
        &self.child
    }

    /// The child's ceiling now ([`SharePool::share_of`]).
    #[must_use]
    pub fn ceiling(&self) -> Option<u64> {
        self.pool.share_of(&self.child)
    }

    /// What the child has spent, in the accumulator's unit.
    #[must_use]
    pub fn spent(&self) -> u64 {
        self.own.spent()
    }

    /// Whether any call the child made could not be priced.
    #[must_use]
    pub fn saw_unpriced(&self) -> bool {
        self.own.saw_unpriced()
    }

    /// Release this child's unspent share to its running siblings
    /// ([`SharePool::release`] with [`Self::spent`]).
    ///
    /// Call it once the child has reached its terminal status and every
    /// response body it drew has been dropped, so its last call is counted.
    #[must_use = "the recipients are what agent_child_share_released announces"]
    pub fn release(&self) -> Vec<(ChildId, u64)> {
        self.pool.release(&self.child, self.spent())
    }

    /// The accumulator the egress check reads.
    pub(crate) fn own(&self) -> &PromptSpend {
        &self.own
    }

    /// Record that a call could not be priced — for the child and the prompt.
    pub(crate) fn note_unpriced(&self) {
        self.own.note_unpriced();
        if let Some(parent) = &self.parent {
            parent.note_unpriced();
        }
    }

    fn add(&self, units: u64) {
        self.own.add(units);
        if let Some(parent) = &self.parent {
            parent.add(units);
        }
    }

    /// Meter `response` so the call's actual cost lands in this child's
    /// accumulator **and** the parent's.
    ///
    /// The meter feeds one accumulator, at the moment the cost is known (when
    /// the body ends or is dropped — REQ-588 ADR-2). So it is handed a fresh one
    /// for this call, and the body is wrapped so that whatever the meter fed it
    /// is forwarded to both, at the same moment. The ledger row is written
    /// exactly as it is for any other call.
    pub(crate) fn meter_response(
        &self,
        meter: &dyn CostMeter,
        response: TransportResponse,
        session_id: Option<SessionId>,
        provider_id: ProviderId,
        attribution: CostAttribution,
    ) -> TransportResponse {
        let attribution = match &self.parent_turn {
            Some(turn) => attribution.for_child(self.child.clone(), turn.clone()),
            None => attribution,
        };
        let call = Arc::new(PromptSpend::new());
        let metered = meter.meter_response(
            response,
            session_id,
            provider_id,
            attribution,
            Some(Arc::clone(&call)),
        );
        TransportResponse {
            status: metered.status,
            location: metered.location,
            body: Box::pin(ChildSpendBody {
                inner: Some(metered.body),
                call,
                child: self.clone(),
                forwarded: 0,
                unpriced_forwarded: false,
            }),
        }
    }
}

/// A metered body that forwards its call's cost to a child and its parent.
///
/// Forwards **what is new** in the call's accumulator each time, rather than
/// once at a single trigger, so neither when the meter records (on the
/// terminal `None`, or from its own `Drop` for an abandoned body) nor how many
/// triggers fire can double-count or miss it.
struct ChildSpendBody {
    /// `Option` so `Drop` can drop it first: an abandoned body is billed from
    /// the inner meter's own `Drop`, and that is the figure forwarded.
    inner: Option<ByteStream>,
    call: Arc<PromptSpend>,
    child: ChildSpend,
    forwarded: u64,
    unpriced_forwarded: bool,
}

impl ChildSpendBody {
    fn forward(&mut self) {
        let total = self.call.spent();
        let new = total.saturating_sub(self.forwarded);
        if new > 0 {
            self.child.add(new);
            self.forwarded = total;
        }
        if !self.unpriced_forwarded && self.call.saw_unpriced() {
            self.unpriced_forwarded = true;
            self.child.note_unpriced();
        }
    }
}

impl Stream for ChildSpendBody {
    type Item = Result<Vec<u8>, TransportError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Unpin: the inner stream is already `Pin<Box<..>>`.
        let this = self.get_mut();
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        match inner.as_mut().poll_next(cx) {
            Poll::Ready(None) => {
                // The meter records a drained call on its terminal `None`, so
                // the cost is known now — before the caller's next check.
                this.forward();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl Drop for ChildSpendBody {
    fn drop(&mut self) {
        // The inner body first, so an abandoned call's record (made from the
        // meter's own `Drop`) is in `call` before it is forwarded.
        drop(self.inner.take());
        self.forward();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::{CostLedger, NoopCostSink, PriceTable};
    use futures::StreamExt;
    use teton_protocol::TurnId;

    fn id(name: &str) -> ChildId {
        ChildId::new("call-1", name)
    }

    fn ids(names: &[&str]) -> Vec<ChildId> {
        names.iter().map(|n| ChildId::new("call-1", n)).collect()
    }

    /// Every child's ceiling, in the pool's order.
    fn ceilings(pool: &SharePool, children: &[ChildId]) -> Vec<Option<u64>> {
        children.iter().map(|c| pool.share_of(c)).collect()
    }

    #[test]
    fn headroom_is_the_ceiling_less_the_spend_and_never_negative() {
        for (ceiling, spent, expected) in [
            (None, 0, None),
            (None, 9_999, None),
            (Some(1_100), 100, Some(1_000)),
            (Some(1_000), 1_000, Some(0)),
            // An overshooting last call (REQ-588 ADR-2) leaves no headroom, not
            // a negative amount of it.
            (Some(1_000), 1_250, Some(0)),
        ] {
            assert_eq!(
                headroom(ceiling, spent),
                expected,
                "headroom({ceiling:?}, {spent})"
            );
        }
    }

    #[test]
    fn split_is_the_floor_of_headroom_over_n() {
        for (headroom, n, expected) in [
            (1_000, 3, 333),
            (1_000, 2, 500),
            (1_000, 1, 1_000),
            (1_000, 5, 200),
            (2, 3, 0),
            (0, 3, 0),
            // Nobody to stamp is zero, not a division by zero.
            (1_000, 0, 0),
            (u64::MAX, 1, u64::MAX),
        ] {
            assert_eq!(split(headroom, n), expected, "split({headroom}, {n})");
        }
    }

    #[test]
    fn release_parts_floor_and_leave_the_remainder_unused() {
        for (unspent, running, expected) in [
            (233, 2, (116, 1)),
            (222, 2, (111, 0)),
            (100, 3, (33, 1)),
            (1, 2, (0, 1)),
            (0, 2, (0, 0)),
            // Nobody running: the whole amount stays unused.
            (233, 0, (0, 233)),
        ] {
            assert_eq!(
                release_parts(unspent, running),
                expected,
                "release_parts({unspent}, {running})"
            );
        }
        for (ceiling, spent, expected) in [(333, 100, 233), (333, 333, 0), (333, 400, 0)] {
            assert_eq!(
                unspent(ceiling, spent),
                expected,
                "unspent({ceiling}, {spent})"
            );
        }
    }

    /// **BR-8.** `headroom 1000, n 3` stamps `333/333/333`; a child that ends
    /// having spent 100 leaves 233, and the two still running gain
    /// `floor(233 / 2) = 116` each, the remainder 1 unused. The benign paths:
    /// a child that spent its whole share releases nothing, and the unlimited
    /// pool neither stamps a share nor releases.
    ///
    /// Mutations (recorded, full `tetond` lib suite): making `release` return
    /// without raising anyone reddens five — this test, `ceiling_only_rises`,
    /// `a_release_never_mints_spend`,
    /// `a_child_releases_once_and_a_stranger_not_at_all`, and
    /// `egress::tests::a_child_over_its_share_is_refused_while_a_raised_sibling_is_not`;
    /// counting the ending child among the running reddens the same five;
    /// giving the remainder to the first recipient reddens two — this test and
    /// `a_release_never_mints_spend`.
    #[test]
    fn release_splits_unspent_equally_floor() {
        let children = ids(&["audit", "lint", "docs"]);
        let [audit, lint, docs] = [&children[0], &children[1], &children[2]];
        let pool = SharePool::new(Some(1_000), &children);
        assert_eq!(
            ceilings(&pool, &children),
            vec![Some(333), Some(333), Some(333)],
            "floor(1000 / 3), the remainder unused"
        );

        let recipients = pool.release(docs, 100);
        assert_eq!(
            recipients,
            vec![(audit.clone(), 333 + 116), (lint.clone(), 333 + 116)],
            "233 unspent, floor(233 / 2) = 116 to each running sibling"
        );
        assert_eq!(
            ceilings(&pool, &children),
            vec![Some(449), Some(449), Some(333)],
            "the recipients rose; the ended child's ceiling is its final one"
        );
        // The remainder stayed unused: both siblings are at 449, not one at 450.

        // Benign: a child that spent its whole share has nothing to release,
        // and its sibling's ceiling does not move.
        assert!(
            pool.release(audit, 449).is_empty(),
            "a child that spent its whole share releases nothing"
        );
        assert_eq!(pool.share_of(lint), Some(449));

        // Benign: no ceiling, no shares, nothing released.
        let unlimited = SharePool::new(None, &children);
        assert_eq!(ceilings(&unlimited, &children), vec![None, None, None]);
        assert!(
            unlimited.release(docs, 0).is_empty(),
            "an unlimited pool releases nothing"
        );
        assert_eq!(
            unlimited.share_of(audit),
            None,
            "and still has no ceiling after a release"
        );
    }

    /// A release is once per child, and only a child the pool knows releases.
    ///
    /// Mutation (recorded): answering `None` for a child the pool does not
    /// know — "no ceiling" in a pool that has one — reddens this test alone.
    #[test]
    fn a_child_releases_once_and_a_stranger_not_at_all() {
        let children = ids(&["a", "b", "c"]);
        let pool = SharePool::new(Some(900), &children);
        assert_eq!(
            pool.release(&children[0], 0),
            vec![(children[1].clone(), 450), (children[2].clone(), 450)]
        );
        assert!(
            pool.release(&children[0], 0).is_empty(),
            "a second release of the same child hands out nothing"
        );
        assert_eq!(
            ceilings(&pool, &children),
            vec![Some(300), Some(450), Some(450)]
        );

        let stranger = ChildId::new("call-2", "a");
        assert!(pool.release(&stranger, 0).is_empty());
        assert_eq!(
            pool.share_of(&stranger),
            Some(0),
            "a pool with a ceiling never answers 'no ceiling' for a child it \
             cannot place — that would send its calls uncounted"
        );
    }

    /// **BR-7.** The share is the one bound that moves after stamping, and only
    /// upward: across a sequence of releases — one that overspent its share,
    /// one that spent nothing, one repeated, one by a stranger, and the last
    /// with nobody left running — no child's ceiling ever falls, and an ended
    /// child's ceiling never moves again.
    ///
    /// Mutations (recorded, full `tetond` lib suite): computing [`unspent`]
    /// with a plain `-` panics on the overshooting release and reddens three —
    /// this test, `a_release_never_mints_spend` and
    /// `release_parts_floor_and_leave_the_remainder_unused`; dropping the
    /// `!share.ended` filter on recipients (ended children keep receiving
    /// parts) reddens four — this test, `release_splits_unspent_equally_floor`,
    /// `a_child_releases_once_and_a_stranger_not_at_all` and
    /// `egress::tests::a_child_over_its_share_is_refused_while_a_raised_sibling_is_not`.
    #[test]
    fn ceiling_only_rises() {
        let children = ids(&["a", "b", "c", "d"]);
        let pool = SharePool::new(Some(1_000), &children);
        assert_eq!(ceilings(&pool, &children), vec![Some(250); 4]);

        let mut before = ceilings(&pool, &children);
        let mut frozen: Vec<Option<u64>> = vec![None; children.len()];
        let mut rises = 0;
        // (who ends, what it spent)
        let steps: [(ChildId, u64); 6] = [
            // Overshot its 250 — an overshoot is never charged to siblings.
            (children[0].clone(), 400),
            // Spent nothing: all 250 goes to the two still running.
            (children[1].clone(), 0),
            // Again: a second release is nothing.
            (children[1].clone(), 0),
            // Not in this pool.
            (ChildId::new("call-2", "a"), 0),
            (children[2].clone(), 10),
            // The last one: nobody left to receive.
            (children[3].clone(), 0),
        ];
        for (who, spent) in steps {
            let recipients = pool.release(&who, spent);
            let after = ceilings(&pool, &children);
            for (i, child) in children.iter().enumerate() {
                assert!(
                    after[i] >= before[i],
                    "{child}'s ceiling fell from {:?} to {:?} on {who}'s release",
                    before[i],
                    after[i]
                );
                if let Some(final_ceiling) = frozen[i] {
                    assert_eq!(
                        after[i],
                        Some(final_ceiling),
                        "{child} has ended; its final ceiling must not move"
                    );
                }
                if after[i] > before[i] {
                    rises += 1;
                    assert!(
                        recipients.contains(&(child.clone(), after[i].unwrap())),
                        "every rise is announced with the new ceiling"
                    );
                }
            }
            if let Some(i) = children.iter().position(|c| *c == who) {
                frozen[i].get_or_insert(after[i].unwrap());
            }
            before = after;
        }
        // Non-vacuity: the sequence really raised ceilings, or "never fell"
        // would be a statement about a pool that never moved.
        assert_eq!(rises, 3, "b's release raises c and d; c's raises d");
        assert_eq!(
            before,
            vec![Some(250), Some(250), Some(375), Some(740)],
            "a overshot and released nothing; c: 250 + floor(250 / 2); \
             d: 375 + floor(365 / 1); d's own release had nobody to receive it"
        );
    }

    /// A release never hands out more than the child left: what the running
    /// children may still spend, plus what the ended ones were charged (their
    /// spend, capped at their final ceiling), never exceeds the headroom the
    /// call started with — and falls short of it only by the floors'
    /// remainders.
    #[test]
    fn a_release_never_mints_spend() {
        let children = ids(&["a", "b", "c", "d", "e"]);
        let headroom = 1_003;
        let pool = SharePool::new(Some(headroom), &children);
        let mut ended: Vec<usize> = Vec::new();
        let mut charged = 0;
        // (who ends, what it spent): an early small spender, an overshooter,
        // a child that spent nothing, and one that received parts first.
        for (i, spent) in [(4, 7), (0, 900), (2, 0), (1, 13)] {
            let final_ceiling = pool.share_of(&children[i]).expect("limited");
            let _ = pool.release(&children[i], spent);
            charged += spent.min(final_ceiling);
            ended.push(i);
            let spendable: u64 = (0..children.len())
                .filter(|j| !ended.contains(j))
                .map(|j| pool.share_of(&children[j]).expect("limited"))
                .sum();
            assert!(
                charged + spendable <= headroom,
                "after {i} ended: {charged} charged + {spendable} spendable > {headroom}"
            );
        }
        // 1003 / 5 leaves 3 unused at stamping; e's 193 / 4 leaves 1. Nothing
        // else is lost: a's overshoot released 0, c's 248 split evenly, b's
        // 359 went whole to d.
        assert_eq!(pool.share_of(&children[3]), Some(731));
        assert_eq!(charged + 731, headroom - 3 - 1);
    }

    /// The child's accumulator and the parent's are fed by the real ledger's
    /// meter, in the unit `spent_by_child` sums in — on a drained body and on
    /// an abandoned one.
    ///
    /// The expected figure is the price table's; the subject is the wiring.
    ///
    /// Mutations (recorded, full `tetond` lib suite): forwarding before
    /// dropping the inner body in `ChildSpendBody::drop` reddens this test
    /// alone (the abandoned call is in the ledger and in neither accumulator);
    /// not forwarding on the terminal `None` reddens two — this test and
    /// `an_unpriced_child_call_is_noted_on_both_accumulators` — while the egress
    /// tests stay green, because their bodies are dropped before the next
    /// check; dropping the parent add in `ChildSpend::add` reddens three —
    /// this test,
    /// `egress::tests::a_child_over_its_share_is_refused_while_a_raised_sibling_is_not`
    /// and `egress::tests::a_childs_choke_point_ignores_the_prompt_pair_and_counts_once`.
    #[tokio::test]
    async fn a_childs_spend_reaches_its_own_and_the_parents_accumulator_in_ledger_units() {
        let ledger = CostLedger::open_in_memory(PriceTable::bundled(), Arc::new(NoopCostSink))
            .expect("open in-memory ledger");
        let prices = PriceTable::bundled();
        let child = id("audit");
        let pool = SharePool::new(Some(1_000_000_000), std::slice::from_ref(&child));
        let parent = Arc::new(PromptSpend::new());
        parent.add(5_000);
        let spend = ChildSpend::new(child.clone(), pool, Some(Arc::clone(&parent)));
        let session = SessionId::from("s1");

        let response = |chunks: Vec<&'static str>| TransportResponse {
            status: 200,
            location: None,
            body: Box::pin(futures::stream::iter(
                chunks
                    .into_iter()
                    .map(|c| Ok::<_, TransportError>(c.as_bytes().to_vec()))
                    .collect::<Vec<_>>(),
            )),
        };
        let meter = |r| {
            spend.meter_response(
                &ledger,
                r,
                Some(session.clone()),
                ProviderId::from("anthropic"),
                CostAttribution::new("claude-fable-5")
                    .for_child(child.clone(), TurnId::from("turn-1")),
            )
        };

        // Drained.
        let mut body = meter(response(vec![
            "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":1200,\"output_tokens\":1}}}\n\n",
            "event: message_delta\ndata: {\"usage\":{\"output_tokens\":340}}\n\n",
        ]))
        .body;
        while body.next().await.is_some() {}
        let first = prices
            .price("claude-fable-5", 1200, 340)
            .expect("bundled price")
            .unsigned_abs();
        assert!(first > 0, "non-vacuity: the call cost something");
        assert_eq!(spend.spent(), first, "counted on the terminal None");
        drop(body);
        assert_eq!(spend.spent(), first, "and not again on drop");

        // Abandoned after one chunk: the meter bills it from its own Drop.
        let mut body = meter(response(vec![
            "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":3000,\"output_tokens\":1}}}\n\n",
            "event: message_delta\ndata: {\"usage\":{\"output_tokens\":900}}\n\n",
        ]))
        .body;
        let _ = body.next().await;
        drop(body);
        let second = prices
            .price("claude-fable-5", 3000, 1)
            .expect("bundled price")
            .unsigned_abs();
        assert!(second > 0);

        assert_eq!(spend.spent(), first + second);
        assert_eq!(
            ledger.spent_by_child(&session, &child).expect("query"),
            spend.spent(),
            "the live figure and the ledger's agree on units"
        );
        assert_eq!(
            parent.spent(),
            5_000 + first + second,
            "the parent paid both calls on top of its own"
        );
        assert!(!spend.saw_unpriced() && !parent.saw_unpriced());
    }

    /// An unpriced call is a fact about the child **and** the prompt.
    #[tokio::test]
    async fn an_unpriced_child_call_is_noted_on_both_accumulators() {
        let ledger = CostLedger::open_in_memory(PriceTable::bundled(), Arc::new(NoopCostSink))
            .expect("open in-memory ledger");
        let child = id("audit");
        let parent = Arc::new(PromptSpend::new());
        let spend = ChildSpend::new(
            child.clone(),
            SharePool::new(Some(1_000), std::slice::from_ref(&child)),
            Some(Arc::clone(&parent)),
        );
        let response = TransportResponse {
            status: 200,
            location: None,
            body: Box::pin(futures::stream::iter(vec![Ok::<_, TransportError>(
                b"data: {\"usage\":{\"prompt_tokens\":80,\"completion_tokens\":42}}\n\n".to_vec(),
            )])),
        };
        let mut body = spend
            .meter_response(
                &ledger,
                response,
                Some(SessionId::from("s1")),
                ProviderId::from("nowhere"),
                CostAttribution::new("no-such-model").for_child(child, TurnId::from("turn-1")),
            )
            .body;
        while body.next().await.is_some() {}
        assert!(spend.saw_unpriced() && parent.saw_unpriced());
        assert_eq!((spend.spent(), parent.spent()), (0, 0));
    }

    /// **REQ-623 BR-8 / AC-12: a spend under a turn bills every call it
    /// meters to the child and its parent turn**, whatever attribution the
    /// caller built — the child's own turn and its duties alike, because both
    /// meter through here.
    ///
    /// Benign path: a spend never put under a turn leaves the caller's
    /// attribution exactly as built, so every choke point that is not a
    /// child's writes the row it always wrote.
    ///
    /// Mutation (run 2026-10-05, reverted): dropping the stamp in
    /// `meter_response` reddens at the second row's `child_id`.
    #[tokio::test]
    async fn a_spend_under_a_turn_writes_both_ids_on_every_row() {
        let ledger = CostLedger::open_in_memory(PriceTable::bundled(), Arc::new(NoopCostSink))
            .expect("open in-memory ledger");
        let child = id("audit");
        let pool = SharePool::new(None, std::slice::from_ref(&child));
        let plain = ChildSpend::new(child.clone(), pool, None);
        let billed = plain.clone().under_turn(TurnId::from("turn-9"));
        for spend in [&plain, &billed] {
            let response = TransportResponse {
                status: 200,
                location: None,
                body: Box::pin(futures::stream::iter(vec![Ok::<_, TransportError>(
                    "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n"
                        .as_bytes()
                        .to_vec(),
                )])),
            };
            let mut body = spend
                .meter_response(
                    &ledger,
                    response,
                    Some(SessionId::from("s1")),
                    ProviderId::from("anthropic"),
                    CostAttribution::new("claude-fable-5")
                        .with_category(teton_protocol::Category::Review),
                )
                .body;
            while body.next().await.is_some() {}
        }
        let rows = ledger.all_records().expect("read");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].child_id.clone(), rows[0].parent_turn_id.clone()),
            (None, None),
            "benign: not under a turn, the caller's attribution stands"
        );
        assert_eq!(rows[1].child_id, Some(child));
        assert_eq!(rows[1].parent_turn_id, Some(TurnId::from("turn-9")));
        assert_eq!(
            rows[1].category,
            Some(teton_protocol::Category::Review),
            "the ids ride beside the caller's attribution, not over it"
        );
    }
}
