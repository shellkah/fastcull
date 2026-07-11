use culler_core::model::Tier;

/// A semantic key, decoded from the string the Slint FocusScope forwards.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Left,
    Right,
    Space,
    Backspace,
    Tab,
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
}

/// Decode the FocusScope-forwarded text into a semantic `Key`.
/// The `.slint` side normalizes special keys to these names; printable text passes through.
pub fn to_key(text: &str) -> Option<Key> {
    match text {
        "Left" => Some(Key::Left),
        "Right" => Some(Key::Right),
        "Tab" => Some(Key::Tab),
        "Backspace" => Some(Key::Backspace),
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
        _ => None,
    }
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
}
