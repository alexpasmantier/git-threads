//! The thread state fold (SPEC.md §2.4).
//!
//! Threads are append-only sets of events; current state is a deterministic
//! fold over them. The fold is a pure function of the event *set* — input
//! order must not matter (events arrive in arbitrary order across merges),
//! which the property tests pin down.

use crate::event::{Event, EventKind};
use crate::id::EventId;
use std::collections::{BTreeMap, BTreeSet};

/// A displayable event (comment or reply) with its folded state applied.
#[derive(Clone, Debug, PartialEq)]
pub struct FoldedEvent {
    pub id: EventId,
    pub event: Event,
    /// Body after applying the winning edit chain (SPEC.md §2.4 rule 1).
    pub effective_body: Option<String>,
    /// Last event of the winning edit chain (the event itself when unedited).
    /// A further edit should supersede this to keep the chain linear.
    pub chain_tip: EventId,
    pub edited: bool,
    pub retracted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FoldedThread {
    /// SPEC.md §2.4 rule 2: latest `resolve` wins; default unresolved.
    pub resolved: bool,
    /// SPEC.md §2.4 rule 5: the latest `move` event, whose `anchor` is where
    /// re-anchoring starts from. `None` means the thread never moved and the
    /// original anchor applies.
    pub moved: Option<(EventId, Event)>,
    /// Comments and replies in display order (`ts`, then event ID), flat.
    pub events: Vec<FoldedEvent>,
}

/// Fold a thread's event set into its current state.
///
/// Duplicate IDs are collapsed (content addressing makes duplicates
/// byte-identical, so which copy wins is immaterial). Edits and deletes only
/// take effect when their author's email matches the target's author — the
/// spec restricts edits to "the same author", and moderation by others is an
/// explicit open question.
pub fn fold_thread(events: impl IntoIterator<Item = (EventId, Event)>) -> FoldedThread {
    // BTreeMap dedups by ID and gives us a deterministic base order.
    let events: BTreeMap<EventId, Event> = events.into_iter().collect();

    let sort_key = |id: &EventId, event: &Event| (event.ts.clone(), id.clone());

    // Rule 2: resolved = value of the latest resolve event by (ts, id).
    let resolved = events
        .iter()
        .filter(|(_, e)| e.kind == EventKind::Resolve)
        .max_by_key(|(id, e)| sort_key(id, e))
        .and_then(|(_, e)| e.resolved)
        .unwrap_or(false);

    // Rule 5: the latest move (by ts, id) carrying an anchor re-pins the
    // thread. A move without an anchor is malformed and ignored.
    let moved = events
        .iter()
        .filter(|(_, e)| e.kind == EventKind::Move && e.anchor.is_some())
        .max_by_key(|(id, e)| sort_key(id, e))
        .map(|(id, e)| (id.clone(), e.clone()));

    // Index edits and deletes by the event they supersede.
    let mut edits_by_target: BTreeMap<&EventId, Vec<(&EventId, &Event)>> = BTreeMap::new();
    let mut deletes: Vec<(&EventId, &Event)> = Vec::new();
    for (id, event) in &events {
        match event.kind {
            EventKind::Edit => {
                if let Some(target) = &event.supersedes {
                    edits_by_target.entry(target).or_default().push((id, event));
                }
            }
            EventKind::Delete if event.supersedes.is_some() => {
                deletes.push((id, event));
            }
            _ => {}
        }
    }

    // Rules 1, 3, 4: displayable events in (ts, id) order, each with its
    // winning edit chain applied.
    let mut folded: Vec<FoldedEvent> = events
        .iter()
        .filter(|(_, e)| matches!(e.kind, EventKind::Comment | EventKind::Reply))
        .map(|(id, event)| {
            let author_email = &event.author.email;
            let mut chain: BTreeSet<&EventId> = BTreeSet::from([id]);
            let mut current = id;
            let mut effective_body = event.body.clone();
            let mut edited = false;
            // Follow the supersede chain: at each node, the latest same-author
            // edit (by ts, id) wins; continue from the winner. The chain set
            // guards against cycles in malformed data.
            while let Some(winner) = edits_by_target
                .get(current)
                .into_iter()
                .flatten()
                .filter(|(edit_id, edit)| {
                    edit.author.email == *author_email && !chain.contains(edit_id)
                })
                .max_by_key(|(edit_id, edit)| sort_key(edit_id, edit))
            {
                let (edit_id, edit) = *winner;
                chain.insert(edit_id);
                effective_body = edit.body.clone();
                edited = true;
                current = edit_id;
            }
            // Rule 1: a delete tombstone targeting any node of the chain wins
            // over edits regardless of order.
            let retracted = deletes.iter().any(|(_, delete)| {
                delete.author.email == *author_email
                    && delete.supersedes.as_ref().is_some_and(|target| chain.contains(target))
            });
            FoldedEvent {
                id: id.clone(),
                event: event.clone(),
                effective_body,
                chain_tip: current.clone(),
                edited,
                retracted,
            }
        })
        .collect();
    folded.sort_by_key(|f| sort_key(&f.id, &f.event));

    FoldedThread { resolved, moved, events: folded }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Author, Timestamp};

    fn event(kind: EventKind, email: &str, ts: &str) -> Event {
        Event {
            v: 1,
            kind,
            author: Author { name: email.to_string(), email: email.to_string() },
            ts: Timestamp::parse(ts).unwrap(),
            body: None,
            in_reply_to: None,
            supersedes: None,
            resolved: None,
            anchor: None,
            of: None,
            extra: Default::default(),
        }
    }

    fn with_id(event: Event) -> (EventId, Event) {
        (event.id().unwrap(), event)
    }

    fn comment(email: &str, ts: &str, body: &str) -> (EventId, Event) {
        let mut e = event(EventKind::Comment, email, ts);
        e.body = Some(body.into());
        with_id(e)
    }

    fn edit(email: &str, ts: &str, target: &EventId, body: &str) -> (EventId, Event) {
        let mut e = event(EventKind::Edit, email, ts);
        e.supersedes = Some(target.clone());
        e.body = Some(body.into());
        with_id(e)
    }

    fn delete(email: &str, ts: &str, target: &EventId) -> (EventId, Event) {
        let mut e = event(EventKind::Delete, email, ts);
        e.supersedes = Some(target.clone());
        with_id(e)
    }

    fn resolve(email: &str, ts: &str, resolved: bool) -> (EventId, Event) {
        let mut e = event(EventKind::Resolve, email, ts);
        e.resolved = Some(resolved);
        with_id(e)
    }

    fn move_to(email: &str, ts: &str, path: &str) -> (EventId, Event) {
        use crate::anchor::{Anchor, AnchorKind, DiffRef, Side};
        use crate::id::GitOid;
        let oid = || GitOid::from_hex("a".repeat(40)).unwrap();
        let mut e = event(EventKind::Move, email, ts);
        e.anchor = Some(Anchor {
            v: 1,
            kind: AnchorKind::File,
            diff: DiffRef { base: oid(), head: oid() },
            path: Some(path.into()),
            old_path: None,
            side: Some(Side::New),
            lines: None,
            blob: Some(oid()),
            cols: None,
            extra: Default::default(),
        });
        with_id(e)
    }

    #[test]
    fn latest_move_wins() {
        let root = comment("a@x", "2026-01-01T00:00:00Z", "b");
        let state = fold_thread([root.clone()]);
        assert_eq!(state.moved, None);

        let m1 = move_to("a@x", "2026-01-01T01:00:00Z", "first.rs");
        let m2 = move_to("b@x", "2026-01-01T02:00:00Z", "second.rs");
        let state = fold_thread([root, m2.clone(), m1]);
        let (id, event) = state.moved.expect("moved");
        assert_eq!(id, m2.0);
        assert_eq!(event.anchor.unwrap().path.as_deref(), Some("second.rs"));
        // Moves are not conversation; they never display as messages.
        assert_eq!(state.events.len(), 1);
    }

    #[test]
    fn edit_chain_latest_wins_and_follows_winner() {
        let root = comment("a@x", "2026-01-01T00:00:00Z", "v1");
        let edit_early = edit("a@x", "2026-01-01T01:00:00Z", &root.0, "v2-early");
        let edit_late = edit("a@x", "2026-01-01T02:00:00Z", &root.0, "v2-late");
        let edit_of_late = edit("a@x", "2026-01-01T03:00:00Z", &edit_late.0, "v3");

        let state = fold_thread([root, edit_early, edit_late, edit_of_late]);
        assert_eq!(state.events.len(), 1);
        assert_eq!(state.events[0].effective_body.as_deref(), Some("v3"));
        assert!(state.events[0].edited);
    }

    #[test]
    fn edits_by_another_author_are_ignored() {
        let root = comment("a@x", "2026-01-01T00:00:00Z", "original");
        let foreign = edit("mallory@x", "2026-01-01T01:00:00Z", &root.0, "hijacked");

        let state = fold_thread([root, foreign]);
        assert_eq!(state.events[0].effective_body.as_deref(), Some("original"));
        assert!(!state.events[0].edited);
    }

    #[test]
    fn delete_wins_regardless_of_order() {
        let root = comment("a@x", "2026-01-01T00:00:00Z", "original");
        // The delete's timestamp is *earlier* than the later edit; it must
        // still retract (SPEC.md §2.4 rule 1).
        let del = delete("a@x", "2026-01-01T01:00:00Z", &root.0);
        let later_edit = edit("a@x", "2026-01-01T02:00:00Z", &root.0, "revived?");

        let state = fold_thread([root, del, later_edit]);
        assert!(state.events[0].retracted);
    }

    #[test]
    fn resolve_last_writer_wins_with_id_tie_break() {
        let root = comment("a@x", "2026-01-01T00:00:00Z", "b");
        let r1 = resolve("a@x", "2026-01-01T01:00:00Z", true);
        let r2 = resolve("b@x", "2026-01-01T01:00:00Z", false);
        // Same ts: the greater event ID wins deterministically.
        let expected = if r1.0 > r2.0 { r1.1.resolved } else { r2.1.resolved };

        let state = fold_thread([root, r1.clone(), r2.clone()]);
        assert_eq!(Some(state.resolved), expected);
    }

    #[test]
    fn orphaned_reply_still_renders() {
        let mut reply = event(EventKind::Reply, "a@x", "2026-01-01T00:00:00Z");
        reply.body = Some("re".into());
        reply.in_reply_to = Some(EventId::from_hex("f".repeat(40)).unwrap());

        let state = fold_thread([with_id(reply)]);
        assert_eq!(state.events.len(), 1);
    }

    #[test]
    fn fold_is_input_order_invariant() {
        let root = comment("a@x", "2026-01-01T00:00:00Z", "v1");
        let e1 = edit("a@x", "2026-01-01T01:00:00Z", &root.0, "v2");
        let e2 = edit("a@x", "2026-01-01T02:00:00Z", &e1.0, "v3");
        let del = delete("a@x", "2026-01-01T03:00:00Z", &e1.0);
        let res = resolve("b@x", "2026-01-01T04:00:00Z", true);
        let reply = {
            let mut e = event(EventKind::Reply, "b@x", "2026-01-01T00:30:00Z");
            e.body = Some("re".into());
            e.in_reply_to = Some(root.0.clone());
            with_id(e)
        };

        let all = [root, e1, e2, del, res, reply];
        let reference = fold_thread(all.clone());
        // All rotations and a reversal — cheap stand-ins for arbitrary arrival order.
        for rotation in 0..all.len() {
            let mut permuted = all.to_vec();
            permuted.rotate_left(rotation);
            assert_eq!(fold_thread(permuted), reference);
        }
        let mut reversed = all.to_vec();
        reversed.reverse();
        assert_eq!(fold_thread(reversed), reference);
    }
}
