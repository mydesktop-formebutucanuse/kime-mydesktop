#[test]
fn check_default_config() {
    assert_eq!(
        serde_yaml::to_string(&kime_engine_core::RawConfig::default()).unwrap(),
        include_str!("../../../../res/default_config.yaml")
    );
}

// X11 hardware keycode (evdev + 8) of the right control key.
const CONTROL_R: u16 = 105;

/// The default config binds `ControlR` to Hanja mode with `Consume`. A build
/// without hanja must drop that binding instead of swallowing the key.
#[test]
fn hotkeys_of_missing_modes_are_dropped() {
    use kime_engine_core::{Config, InputCategory, InputEngine, InputResult, ModifierState};

    let config = Config::default();
    let mut engine = InputEngine::new(&config);
    engine.set_input_category(InputCategory::Hangul);

    let ret = engine.press_key_code(CONTROL_R, ModifierState::empty(), false, &config);
    assert_eq!(
        ret.contains(InputResult::CONSUMED),
        cfg!(feature = "hanja"),
        "ControlR must reach the app exactly when hanja is not built"
    );
    if !cfg!(feature = "hanja") {
        assert!(
            config.candidate_font.0.is_empty(),
            "no hanja, no candidate font"
        );
    }
}
