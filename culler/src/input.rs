//! Pure input logic: keymap, filters, actions. Wired into the event loop by main (Task 11).

use culler_core::model::{Decision, Session, Tier};

/// A semantic key, decoded from the string the Slint FocusScope forwards.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Left,
    Right,
    Space,
    Backspace,
    Tab,
    Escape,
    Char(char),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Modifiers {
    pub control: bool,
    pub shift: bool,
    pub alt: bool,
}

/// Which surface currently owns the keyboard. The loupe keymap is inert unless `Loupe`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputContext {
    Loupe,
    TagEntry,
    ApplyDialog,
    Help,
}

/// One user intent. Model-mutating variants are executed by `apply_action`;
/// UI-only variants (OpenTagEntry, ToggleZoom, CycleFilter, OpenApply, ForceSave)
/// are dispatched by the ui glue.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    Prev,
    Next,
    NextUnvisited,
    SetTier(Option<Tier>), // Some(t) = keep/pick/best/reject; None = clear -> Rest
    Undo,
    OpenTagEntry,
    ToggleZoom,
    CycleFilter,
    OpenApply,
    ForceSave,
    ToggleHelp,
}

/// Decode the FocusScope-forwarded text into a semantic `Key`.
/// The `.slint` side normalizes special keys to these names; printable text passes through.
pub fn to_key(text: &str) -> Option<Key> {
    match text {
        "Left" => Some(Key::Left),
        "Right" => Some(Key::Right),
        "Tab" => Some(Key::Tab),
        "Backspace" => Some(Key::Backspace),
        "Escape" => Some(Key::Escape),
        " " => Some(Key::Space),
        _ => text.chars().next().map(Key::Char),
    }
}

/// Pure §9 keymap. Returns None when a modal (`TagEntry`/`ApplyDialog`) owns keys,
/// or when the key is unbound.
pub fn key_to_action(key: Key, mods: Modifiers, ctx: InputContext) -> Option<Action> {
    if ctx != InputContext::Loupe {
        return None;
    }
    if mods.control {
        return match key {
            Key::Char('s') | Key::Char('S') => Some(Action::ForceSave),
            _ => None,
        };
    }
    match key {
        Key::Left | Key::Backspace => Some(Action::Prev),
        Key::Right | Key::Space => Some(Action::Next),
        Key::Tab => Some(Action::NextUnvisited),
        Key::Char('1') => Some(Action::SetTier(Some(Tier::Keep))),
        Key::Char('2') => Some(Action::SetTier(Some(Tier::Pick))),
        Key::Char('3') => Some(Action::SetTier(Some(Tier::Best))),
        Key::Char('x') | Key::Char('X') => Some(Action::SetTier(Some(Tier::Reject))),
        Key::Char('`') | Key::Char('0') => Some(Action::SetTier(None)),
        Key::Char('u') | Key::Char('U') => Some(Action::Undo),
        Key::Char('t') | Key::Char('T') => Some(Action::OpenTagEntry),
        Key::Char('z') | Key::Char('Z') => Some(Action::ToggleZoom),
        Key::Char('f') | Key::Char('F') => Some(Action::CycleFilter),
        Key::Char('a') | Key::Char('A') => Some(Action::OpenApply),
        Key::Char('?') => Some(Action::ToggleHelp),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Filter {
    All,
    Keep,    // >= Keep
    Pick,    // >= Pick
    Best,    // >= Best (only Best)
    Rejects, // only Reject
}

/// §9 filter cycle: All -> >=Keep -> >=Pick -> >=Best -> Rejects -> All.
pub fn next_filter(f: Filter) -> Filter {
    match f {
        Filter::All => Filter::Keep,
        Filter::Keep => Filter::Pick,
        Filter::Pick => Filter::Best,
        Filter::Best => Filter::Rejects,
        Filter::Rejects => Filter::All,
    }
}

/// Pure predicate: does this decision pass the active filter?
/// Ladder is Reject(-1) < Rest/None(0) < Keep(1) < Pick(2) < Best(3).
pub fn passes(filter: Filter, d: &Decision) -> bool {
    match filter {
        Filter::All => true,
        Filter::Keep => d.tier.is_some_and(|t| t.rank() >= 1),
        Filter::Pick => d.tier.is_some_and(|t| t.rank() >= 2),
        Filter::Best => d.tier.is_some_and(|t| t.rank() >= 3),
        Filter::Rejects => d.tier == Some(culler_core::model::Tier::Reject),
    }
}

/// Next/previous index whose decision passes `filter`. With `Filter::All`
/// this is a plain +/-1 (first candidate always passes). None at either end.
pub fn step_filtered(session: &Session, filter: Filter, forward: bool) -> Option<usize> {
    let n = session.shots.len();
    if n == 0 {
        return None;
    }
    let mut i = session.current;
    loop {
        if forward {
            if i + 1 >= n {
                return None;
            }
            i += 1;
        } else {
            if i == 0 {
                return None;
            }
            i -= 1;
        }
        if passes(filter, session.decision(i)) {
            return Some(i);
        }
    }
}

/// Turn comma-separated tag-entry text into clean, order-preserving, deduped tags.
pub fn parse_tags(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in input.split(',') {
        let t = raw.trim();
        if !t.is_empty() && !out.iter().any(|e| e == t) {
            out.push(t.to_string());
        }
    }
    out
}

#[cfg(test)]
mod key_tests {
    use super::*;
    use culler_core::model::Tier;

    const LOUPE: InputContext = InputContext::Loupe;
    fn m() -> Modifiers {
        Modifiers::default()
    }

    #[test]
    fn arrows_space_backspace_navigate() {
        assert_eq!(key_to_action(Key::Left, m(), LOUPE), Some(Action::Prev));
        assert_eq!(
            key_to_action(Key::Backspace, m(), LOUPE),
            Some(Action::Prev)
        );
        assert_eq!(key_to_action(Key::Right, m(), LOUPE), Some(Action::Next));
        assert_eq!(key_to_action(Key::Space, m(), LOUPE), Some(Action::Next));
    }

    #[test]
    fn tier_keys_map_to_settier_some() {
        assert_eq!(
            key_to_action(Key::Char('1'), m(), LOUPE),
            Some(Action::SetTier(Some(Tier::Keep)))
        );
        assert_eq!(
            key_to_action(Key::Char('2'), m(), LOUPE),
            Some(Action::SetTier(Some(Tier::Pick)))
        );
        assert_eq!(
            key_to_action(Key::Char('3'), m(), LOUPE),
            Some(Action::SetTier(Some(Tier::Best)))
        );
        assert_eq!(
            key_to_action(Key::Char('x'), m(), LOUPE),
            Some(Action::SetTier(Some(Tier::Reject)))
        );
        assert_eq!(
            key_to_action(Key::Char('X'), m(), LOUPE),
            Some(Action::SetTier(Some(Tier::Reject)))
        );
    }

    #[test]
    fn clear_keys_map_to_settier_none() {
        assert_eq!(
            key_to_action(Key::Char('`'), m(), LOUPE),
            Some(Action::SetTier(None))
        );
        assert_eq!(
            key_to_action(Key::Char('0'), m(), LOUPE),
            Some(Action::SetTier(None))
        );
    }

    #[test]
    fn command_keys_cover_the_keymap() {
        assert_eq!(
            key_to_action(Key::Char('u'), m(), LOUPE),
            Some(Action::Undo)
        );
        assert_eq!(
            key_to_action(Key::Char('t'), m(), LOUPE),
            Some(Action::OpenTagEntry)
        );
        assert_eq!(
            key_to_action(Key::Char('z'), m(), LOUPE),
            Some(Action::ToggleZoom)
        );
        assert_eq!(
            key_to_action(Key::Char('f'), m(), LOUPE),
            Some(Action::CycleFilter)
        );
        assert_eq!(
            key_to_action(Key::Tab, m(), LOUPE),
            Some(Action::NextUnvisited)
        );
        assert_eq!(
            key_to_action(Key::Char('a'), m(), LOUPE),
            Some(Action::OpenApply)
        );
    }

    #[test]
    fn ctrl_s_force_saves_but_plain_s_does_not() {
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(
            key_to_action(Key::Char('s'), ctrl, LOUPE),
            Some(Action::ForceSave)
        );
        assert_eq!(
            key_to_action(Key::Char('S'), ctrl, LOUPE),
            Some(Action::ForceSave)
        );
        assert_eq!(key_to_action(Key::Char('s'), m(), LOUPE), None);
    }

    #[test]
    fn keymap_is_inert_outside_the_loupe() {
        assert_eq!(
            key_to_action(Key::Char('1'), m(), InputContext::TagEntry),
            None
        );
        assert_eq!(
            key_to_action(Key::Left, m(), InputContext::ApplyDialog),
            None
        );
    }

    #[test]
    fn to_key_normalizes_specials_and_printables() {
        assert_eq!(to_key("Left"), Some(Key::Left));
        assert_eq!(to_key("Right"), Some(Key::Right));
        assert_eq!(to_key("Tab"), Some(Key::Tab));
        assert_eq!(to_key("Backspace"), Some(Key::Backspace));
        assert_eq!(to_key(" "), Some(Key::Space));
        assert_eq!(to_key("a"), Some(Key::Char('a')));
        assert_eq!(to_key(""), None);
    }

    #[test]
    fn to_key_normalizes_escape() {
        assert_eq!(to_key("Escape"), Some(Key::Escape));
    }

    #[test]
    fn question_mark_toggles_help_in_loupe() {
        assert_eq!(
            key_to_action(Key::Char('?'), m(), LOUPE),
            Some(Action::ToggleHelp)
        );
    }

    #[test]
    fn help_context_is_inert() {
        assert_eq!(key_to_action(Key::Char('1'), m(), InputContext::Help), None);
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;
    use culler_core::model::{CaptureTime, Decision, Session, Shot, Tier};

    fn mk_session(tiers: &[Option<Tier>]) -> Session {
        let mut shots = Vec::new();
        let mut decisions = std::collections::HashMap::new();
        for (i, t) in tiers.iter().enumerate() {
            let stem = format!("IMG_{i:04}");
            shots.push(Shot {
                stem: stem.clone(),
                jpeg: std::path::PathBuf::from(format!("/src/{stem}.JPG")),
                raw: None,
                sidecar: None,
                capture: CaptureTime::default(),
            });
            decisions.insert(
                stem,
                Decision {
                    tier: *t,
                    tags: vec![],
                    visited: false,
                },
            );
        }
        Session {
            source_dir: "/src".into(),
            shots,
            decisions,
            current: 0,
            pending_apply: None,
            undo: Vec::new(),
        }
    }

    #[test]
    fn filter_cycles_all_keep_pick_best_rejects() {
        assert_eq!(next_filter(Filter::All), Filter::Keep);
        assert_eq!(next_filter(Filter::Keep), Filter::Pick);
        assert_eq!(next_filter(Filter::Pick), Filter::Best);
        assert_eq!(next_filter(Filter::Best), Filter::Rejects);
        assert_eq!(next_filter(Filter::Rejects), Filter::All);
    }

    #[test]
    fn passes_respects_quality_ladder() {
        let none = Decision::default();
        let keep = Decision {
            tier: Some(Tier::Keep),
            ..Default::default()
        };
        let pick = Decision {
            tier: Some(Tier::Pick),
            ..Default::default()
        };
        let best = Decision {
            tier: Some(Tier::Best),
            ..Default::default()
        };
        let rej = Decision {
            tier: Some(Tier::Reject),
            ..Default::default()
        };

        for d in [&none, &keep, &pick, &best, &rej] {
            assert!(passes(Filter::All, d));
        }
        // >= Keep : keep, pick, best (never rest/none or reject)
        assert!(!passes(Filter::Keep, &none));
        assert!(!passes(Filter::Keep, &rej));
        assert!(passes(Filter::Keep, &keep));
        assert!(passes(Filter::Keep, &pick));
        assert!(passes(Filter::Keep, &best));
        // >= Pick
        assert!(!passes(Filter::Pick, &keep));
        assert!(passes(Filter::Pick, &pick));
        assert!(passes(Filter::Pick, &best));
        // >= Best (only best)
        assert!(!passes(Filter::Best, &pick));
        assert!(passes(Filter::Best, &best));
        // Rejects (only reject)
        assert!(passes(Filter::Rejects, &rej));
        assert!(!passes(Filter::Rejects, &keep));
        assert!(!passes(Filter::Rejects, &none));
    }

    #[test]
    fn step_filtered_skips_non_passing_forward_and_back() {
        // Keep, None, Pick, None, Best
        let mut s = mk_session(&[
            Some(Tier::Keep),
            None,
            Some(Tier::Pick),
            None,
            Some(Tier::Best),
        ]);
        s.current = 0;
        assert_eq!(step_filtered(&s, Filter::Pick, true), Some(2)); // next >=Pick after Keep@0
        s.current = 2;
        assert_eq!(step_filtered(&s, Filter::Pick, true), Some(4)); // Best@4
        s.current = 4;
        assert_eq!(step_filtered(&s, Filter::Pick, true), None); // nothing after
        s.current = 4;
        assert_eq!(step_filtered(&s, Filter::Pick, false), Some(2)); // back to Pick@2
    }

    #[test]
    fn step_filtered_all_is_plain_pm1() {
        let mut s = mk_session(&[None, None, None]);
        s.current = 1;
        assert_eq!(step_filtered(&s, Filter::All, true), Some(2));
        assert_eq!(step_filtered(&s, Filter::All, false), Some(0));
        s.current = 0;
        assert_eq!(step_filtered(&s, Filter::All, false), None); // clamp at start
    }

    #[test]
    fn parse_tags_splits_trims_dedupes() {
        assert_eq!(
            parse_tags("sky, tree ,  sky , , water"),
            vec!["sky".to_string(), "tree".to_string(), "water".to_string()]
        );
        assert!(parse_tags("   ").is_empty());
        assert!(parse_tags("").is_empty());
    }
}

/// Execute a model-mutating action. UI-only actions are no-ops (handled by the ui glue).
/// `filter` confines prev/next and auto-advance to the working set; `auto_advance`
/// only affects `SetTier(Some(_))` (clear never advances).
pub fn apply_action(action: Action, session: &mut Session, auto_advance: bool, filter: Filter) {
    match action {
        Action::Prev => {
            if let Some(i) = step_filtered(session, filter, false) {
                session.current = i;
                session.mark_visited(i);
            }
        }
        Action::Next => {
            if let Some(i) = step_filtered(session, filter, true) {
                session.current = i;
                session.mark_visited(i);
            }
        }
        Action::NextUnvisited => {
            if let Some(i) = session.next_unvisited(session.current) {
                session.current = i;
                session.mark_visited(i);
            }
        }
        Action::SetTier(tier) => {
            let idx = session.current;
            session.set_tier(idx, tier);
            if tier.is_some()
                && auto_advance
                && let Some(i) = step_filtered(session, filter, true)
            {
                session.current = i;
                session.mark_visited(i);
            }
        }
        Action::Undo => {
            session.undo();
        }
        // UI-only — the ui glue handles these; no model mutation here.
        Action::OpenTagEntry
        | Action::ToggleZoom
        | Action::CycleFilter
        | Action::OpenApply
        | Action::ForceSave
        | Action::ToggleHelp => {}
    }
}

#[cfg(test)]
mod action_tests {
    use super::*;
    use culler_core::model::{CaptureTime, Decision, Session, Shot, Tier};

    fn mk_session(tiers: &[Option<Tier>]) -> Session {
        let mut shots = Vec::new();
        let mut decisions = std::collections::HashMap::new();
        for (i, t) in tiers.iter().enumerate() {
            let stem = format!("IMG_{i:04}");
            shots.push(Shot {
                stem: stem.clone(),
                jpeg: std::path::PathBuf::from(format!("/src/{stem}.JPG")),
                raw: None,
                sidecar: None,
                capture: CaptureTime::default(),
            });
            decisions.insert(
                stem,
                Decision {
                    tier: *t,
                    tags: vec![],
                    visited: false,
                },
            );
        }
        Session {
            source_dir: "/src".into(),
            shots,
            decisions,
            current: 0,
            pending_apply: None,
            undo: Vec::new(),
        }
    }

    #[test]
    fn settier_some_records_and_autoadvances() {
        let mut s = mk_session(&[None, None, None]);
        apply_action(Action::SetTier(Some(Tier::Keep)), &mut s, true, Filter::All);
        assert_eq!(s.decision(0).tier, Some(Tier::Keep));
        assert_eq!(s.current, 1); // advanced
        assert!(s.decision(1).visited);
    }

    #[test]
    fn settier_some_no_autoadvance_when_disabled() {
        let mut s = mk_session(&[None, None]);
        apply_action(
            Action::SetTier(Some(Tier::Pick)),
            &mut s,
            false,
            Filter::All,
        );
        assert_eq!(s.current, 0);
    }

    #[test]
    fn clear_never_autoadvances_even_when_enabled() {
        let mut s = mk_session(&[Some(Tier::Keep), None]);
        apply_action(Action::SetTier(None), &mut s, true, Filter::All);
        assert_eq!(s.decision(0).tier, None);
        assert_eq!(s.current, 0);
    }

    #[test]
    fn undo_reverts_last_tier_change() {
        let mut s = mk_session(&[None]);
        apply_action(
            Action::SetTier(Some(Tier::Best)),
            &mut s,
            false,
            Filter::All,
        );
        apply_action(Action::Undo, &mut s, false, Filter::All);
        assert_eq!(s.decision(0).tier, None);
    }

    #[test]
    fn next_prev_move_and_mark_visited() {
        let mut s = mk_session(&[None, None]);
        apply_action(Action::Next, &mut s, false, Filter::All);
        assert_eq!(s.current, 1);
        assert!(s.decision(1).visited);
        apply_action(Action::Prev, &mut s, false, Filter::All);
        assert_eq!(s.current, 0);
    }

    #[test]
    fn autoadvance_respects_active_filter() {
        // Keep, None, Keep : tiering @0 with >=Keep filter should skip None@1 to Keep@2
        let mut s = mk_session(&[None, None, None]);
        // set up so 2 already passes >=Keep, 1 does not
        apply_action(
            Action::SetTier(Some(Tier::Keep)),
            &mut s,
            false,
            Filter::All,
        ); // s.current stays 0
        s.decisions.get_mut("IMG_0002").unwrap().tier = Some(Tier::Keep);
        s.current = 0;
        apply_action(
            Action::SetTier(Some(Tier::Keep)),
            &mut s,
            true,
            Filter::Keep,
        );
        assert_eq!(s.current, 2); // skipped the un-tiered @1
    }

    #[test]
    fn ui_only_actions_do_not_mutate_model() {
        let mut s = mk_session(&[None, None]);
        for a in [
            Action::OpenTagEntry,
            Action::ToggleZoom,
            Action::CycleFilter,
            Action::OpenApply,
            Action::ForceSave,
            Action::ToggleHelp,
        ] {
            apply_action(a, &mut s, true, Filter::All);
        }
        assert_eq!(s.current, 0);
        assert_eq!(s.decision(0), &Decision::default());
    }
}
