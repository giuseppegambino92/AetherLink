//! Wire-format checks: the JSON the frontend builds in `ui/js/lib/hid.js`
//! must deserialize into `aether_protocol::InputEvent` / `DomAction`, and the
//! JS HID table must agree with `HidUsage::from_dom_code`.

use aether_protocol::{DomAction, GesturePhase, HidUsage, InputEvent, Modifiers, MouseButton, ScrollUnit};

fn parse(json: &str) -> InputEvent {
    serde_json::from_str(json).unwrap_or_else(|e| panic!("{json}: {e}"))
}

#[test]
fn frontend_input_events_deserialize() {
    assert_eq!(
        parse(r#"{"MouseMove":{"display_id":1,"x":0.5,"y":0.25}}"#),
        InputEvent::MouseMove { display_id: 1, x: 0.5, y: 0.25 }
    );
    assert_eq!(
        parse(r#"{"MouseButton":{"button":"Left","pressed":true}}"#),
        InputEvent::MouseButton { button: MouseButton::Left, pressed: true }
    );
    assert_eq!(
        parse(r#"{"Scroll":{"dx":0,"dy":120,"unit":"Pixel","phase":null}}"#),
        InputEvent::Scroll { dx: 0.0, dy: 120.0, unit: ScrollUnit::Pixel, phase: None }
    );
    assert_eq!(
        parse(r#"{"Scroll":{"dx":1,"dy":-3,"unit":"Line","phase":"Momentum"}}"#),
        InputEvent::Scroll { dx: 1.0, dy: -3.0, unit: ScrollUnit::Line, phase: Some(GesturePhase::Momentum) }
    );
    assert_eq!(
        parse(r#"{"Key":{"code":4,"pressed":true,"modifiers":8}}"#),
        InputEvent::Key { code: HidUsage::A, pressed: true, modifiers: Modifiers::META }
    );
    assert_eq!(parse(r#"{"Text":{"text":"è"}}"#), InputEvent::Text { text: "è".into() });
    assert_eq!(parse(r#""ReleaseAll""#), InputEvent::ReleaseAll);
    assert_eq!(
        serde_json::from_str::<DomAction>(r#""Press""#).unwrap(),
        DomAction::Press
    );
}

/// Mirrors the table in `ui/js/lib/hid.js` (`domCodeToHid`).
#[test]
fn js_hid_table_matches_protocol() {
    let src = include_str!("../../ui/js/lib/hid.js");
    for (code, usage) in [
        ("Enter", 0x28), ("NumpadEnter", 0x28), ("Escape", 0x29), ("Backspace", 0x2A), ("Tab", 0x2B),
        ("Space", 0x2C), ("Backquote", 0x35), ("IntlBackslash", 0x64), ("ContextMenu", 0x65),
        ("AudioVolumeMute", 0x7F), ("MetaLeft", 0xE3), ("OSRight", 0xE7), ("ArrowUp", 0x52),
    ] {
        assert_eq!(HidUsage::from_dom_code(code), Some(HidUsage(usage)), "{code}");
        let needle = format!("{code}: 0x{usage:02x}");
        assert!(src.contains(&needle), "hid.js is missing `{needle}`");
    }
}
