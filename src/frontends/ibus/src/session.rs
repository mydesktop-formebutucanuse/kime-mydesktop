//! Engine state of one IBus input context, independent of D-Bus.
//!
//! Every entry point returns the [`Event`]s the client must see, in order;
//! the D-Bus layer only decides whether they go out as signals or are queued
//! for `PostProcessKeyEvent`.

use std::sync::Arc;

use kime_engine_core::{Config, InputEngine, InputResult, ModifierState};

/// `IBusModifierType` bits (`src/ibustypes.h`).
pub mod mask {
    pub const SHIFT: u32 = 1 << 0;
    pub const CONTROL: u32 = 1 << 2;
    /// Alt
    pub const MOD1: u32 = 1 << 3;
    /// Num Lock
    pub const MOD2: u32 = 1 << 4;
    /// Super
    pub const MOD4: u32 = 1 << 6;
    /// Set on events IBus already forwarded back to the client.
    pub const FORWARD: u32 = 1 << 25;
    pub const SUPER: u32 = 1 << 26;
    pub const RELEASE: u32 = 1 << 30;
}

/// IBus clients send evdev keycodes (`hardware_keycode - 8`, see
/// `ibusimcontext.c`); kime's keycode table is keyed by X11 keycodes.
const EVDEV_TO_X11: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Commit(String),
    /// The full preedit the client must now display; empty hides it.
    Preedit(String),
}

pub struct Session {
    engine: InputEngine,
    config: Arc<Config>,
    engine_ready: bool,
    /// The preedit the client currently displays.
    shown_preedit: String,
}

impl Session {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            engine: InputEngine::new(&config),
            config,
            engine_ready: true,
            shown_preedit: String::new(),
        }
    }

    fn show_preedit(&mut self, preedit: &str, events: &mut Vec<Event>) {
        if self.shown_preedit != preedit {
            self.shown_preedit.clear();
            self.shown_preedit.push_str(preedit);
            events.push(Event::Preedit(preedit.to_owned()));
        }
    }

    fn commit(&mut self, events: &mut Vec<Event>) {
        if self.engine.commit_str().is_empty() {
            return;
        }

        let commit = self.engine.commit_str().to_owned();
        self.engine.clear_commit();
        // End the old composition before committing, as the XIM frontend
        // does: a commit landing inside a live preedit makes some clients
        // replace the preedit with it and then draw the stale preedit again.
        self.show_preedit("", events);
        events.push(Event::Commit(commit));
    }

    fn process_input_result(&mut self, ret: InputResult) -> Vec<Event> {
        log::trace!("{:?}", ret);

        let mut events = Vec::new();

        if ret.contains(InputResult::LANGUAGE_CHANGED) {
            self.engine.update_layout_state().ok();
        }

        if ret.contains(InputResult::HAS_COMMIT) {
            self.commit(&mut events);
        }

        let preedit = if ret.contains(InputResult::HAS_PREEDIT) {
            self.engine.preedit_str().to_owned()
        } else {
            String::new()
        };
        self.show_preedit(&preedit, &mut events);

        self.engine_ready = !ret.contains(InputResult::NOT_READY);

        events
    }

    /// `ProcessKeyEvent`: returns whether the key was consumed.
    pub fn process_key_event(
        &mut self,
        _keyval: u32,
        keycode: u32,
        state: u32,
    ) -> (bool, Vec<Event>) {
        // kime acts on presses only; a key we forwarded must not loop back.
        if state & (mask::RELEASE | mask::FORWARD) != 0 {
            return (false, Vec::new());
        }

        let mut modifiers = ModifierState::empty();
        if state & mask::SHIFT != 0 {
            modifiers.insert(ModifierState::SHIFT);
        }
        if state & mask::CONTROL != 0 {
            modifiers.insert(ModifierState::CONTROL);
        }
        if state & mask::MOD1 != 0 {
            modifiers.insert(ModifierState::ALT);
        }
        if state & (mask::MOD4 | mask::SUPER) != 0 {
            modifiers.insert(ModifierState::SUPER);
        }
        let numlock = state & mask::MOD2 != 0;

        let hardware_code = keycode
            .checked_add(EVDEV_TO_X11)
            .and_then(|code| u16::try_from(code).ok())
            // 0 is no X11 keycode: kime treats it like any unknown key
            .unwrap_or(0);

        let ret = self
            .engine
            .press_key_code(hardware_code, modifiers, numlock, &self.config);

        (
            ret.contains(InputResult::CONSUMED),
            self.process_input_result(ret),
        )
    }

    /// `Reset`: commit what is being composed and start over.
    pub fn reset(&mut self) -> Vec<Event> {
        let mut events = Vec::new();

        self.engine.clear_preedit();
        self.commit(&mut events);
        self.show_preedit("", &mut events);
        self.engine.reset();

        events
    }

    pub fn focus_in(&mut self) -> Vec<Event> {
        self.engine.update_layout_state().ok();

        // Focus comes back from the hanja candidate window: apply its pick.
        if !self.engine_ready && self.engine.check_ready() {
            let ret = self.engine.end_ready();
            let events = self.process_input_result(ret);
            self.engine_ready = true;
            return events;
        }

        Vec::new()
    }

    pub fn focus_out(&mut self) -> Vec<Event> {
        // Losing focus to kime's own candidate window must keep the
        // composition alive for the pick to apply to.
        if self.engine_ready {
            self.reset()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kime_engine_core::InputCategory;

    // evdev codes (X11 keycode - 8) on a US layout
    const KEY_R: u32 = 19; // ㄱ in dubeolsik
    const KEY_K: u32 = 37; // ㅏ
    const KEY_S: u32 = 31; // ㄴ
    const KEY_SPACE: u32 = 57;

    // IBus keysyms are only informational for kime
    const XK_R: u32 = 0x72;

    fn hangul_session() -> Session {
        let config = Config {
            default_category: InputCategory::Hangul,
            ..Default::default()
        };
        Session::new(Arc::new(config))
    }

    fn preedit(s: &str) -> Event {
        Event::Preedit(s.into())
    }

    fn commit(s: &str) -> Event {
        Event::Commit(s.into())
    }

    #[test]
    fn composes_and_commits() {
        let mut s = hangul_session();

        assert_eq!(
            s.process_key_event(XK_R, KEY_R, 0),
            (true, vec![preedit("ㄱ")])
        );
        assert_eq!(
            s.process_key_event(0, KEY_K, 0),
            (true, vec![preedit("가")])
        );
        assert_eq!(
            s.process_key_event(0, KEY_S, 0),
            (true, vec![preedit("간")])
        );
        // The jongseong moves on: the old preedit ends, then 가 commits.
        assert_eq!(
            s.process_key_event(0, KEY_K, 0),
            (true, vec![preedit(""), commit("가"), preedit("나")])
        );
    }

    #[test]
    fn unhandled_key_commits_before_reaching_the_app() {
        let mut s = hangul_session();
        s.process_key_event(0, KEY_R, 0);
        s.process_key_event(0, KEY_K, 0);

        let (consumed, events) = s.process_key_event(0x20, KEY_SPACE, 0);
        assert!(!consumed, "space must still reach the application");
        assert_eq!(events, vec![preedit(""), commit("가")]);
    }

    #[test]
    fn releases_and_forwarded_keys_are_ignored() {
        let mut s = hangul_session();

        assert_eq!(
            s.process_key_event(0, KEY_R, mask::RELEASE),
            (false, vec![])
        );
        assert_eq!(
            s.process_key_event(0, KEY_R, mask::FORWARD),
            (false, vec![])
        );
        assert!(s.reset().is_empty(), "nothing was composed");
    }

    #[test]
    fn control_shortcuts_pass_through() {
        let mut s = hangul_session();
        assert_eq!(
            s.process_key_event(0, KEY_R, mask::CONTROL),
            (false, vec![])
        );
    }

    #[test]
    fn reset_and_focus_out_commit_the_preedit() {
        let mut s = hangul_session();
        s.process_key_event(0, KEY_R, 0);
        assert_eq!(s.reset(), vec![preedit(""), commit("ㄱ")]);

        s.process_key_event(0, KEY_R, 0);
        s.process_key_event(0, KEY_K, 0);
        assert_eq!(s.focus_out(), vec![preedit(""), commit("가")]);
        assert!(s.focus_out().is_empty());
    }

    #[test]
    fn bogus_keycodes_do_not_panic() {
        let mut s = hangul_session();
        s.process_key_event(0, KEY_R, 0);

        let (consumed, events) = s.process_key_event(0, u32::MAX, 0);
        assert!(!consumed);
        assert_eq!(events, vec![preedit(""), commit("ㄱ")]);
    }
}
