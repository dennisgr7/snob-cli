//! What the page in the person's browser posts back, and the protocol
//! command each piece becomes.
//!
//! Pure, so every translation is tested without a browser. The page sends
//! coordinates already in the tab's own CSS pixels (it knows the size of the
//! frame it was sent), so nothing here scales; it only refuses what no mouse
//! or keyboard would produce.

use serde::Deserialize;
use serde_json::{Value, json};

/// How many events one post may carry. The page sends a handful at a time.
pub(super) const MOST_EVENTS: usize = 256;

/// The longest text one event may insert: a password, a code, a pasted
/// address.
const MOST_TEXT: usize = 4096;

/// The farthest a coordinate may be from the corner: well past any screen
/// the tab declares, and far short of anything that would overflow.
const MOST_COORDINATE: f64 = 10_000.0;

/// One thing the person did, as the page describes it.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "lowercase")]
pub(super) enum Input {
    /// A mouse button pressed or let go, or the pointer moved.
    Mouse {
        kind: MouseKind,
        x: f64,
        y: f64,
        #[serde(default)]
        button: Button,
        /// The buttons held, as `MouseEvent.buttons` counts them.
        #[serde(default)]
        buttons: u8,
        #[serde(default)]
        clicks: u8,
        #[serde(default)]
        mods: u8,
    },
    /// The wheel turned, or a finger dragged across a touch screen.
    Wheel {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        #[serde(default)]
        mods: u8,
    },
    /// A key pressed or let go, as `KeyboardEvent` names it.
    Key {
        down: bool,
        key: String,
        #[serde(default)]
        code: String,
        #[serde(default, rename = "keyCode")]
        key_code: u16,
        /// The character it types, when it types one.
        #[serde(default)]
        text: String,
        #[serde(default)]
        mods: u8,
        #[serde(default)]
        repeat: bool,
    },
    /// Text that arrived whole: a paste, or what an on-screen keyboard or an
    /// input method composed.
    Text {
        text: String,
    },
    /// One of the page's own buttons.
    Back,
    Forward,
    Reload,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(super) enum MouseKind {
    Down,
    Up,
    Move,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub(super) enum Button {
    #[default]
    None,
    Left,
    Middle,
    Right,
}

/// The modifier bits, which the page and the protocol count the same way:
/// Alt 1, Ctrl 2, Meta 4, Shift 8.
const CTRL: u8 = 2;
const META: u8 = 4;
const ALL_MODIFIERS: u8 = 15;

/// What an input becomes.
#[derive(Debug, PartialEq)]
pub(super) enum Command {
    /// A command for the tab.
    Send(&'static str, Value),
    /// A move through the tab's history, by this many entries.
    History(i64),
    Reload,
}

/// Reads one post's events, refusing the post whole when any of it is not
/// something the page sends.
pub(super) fn parse(body: &[u8]) -> Option<Vec<Input>> {
    let events: Vec<Input> = serde_json::from_slice(body).ok()?;
    (events.len() <= MOST_EVENTS).then_some(events)
}

/// The command `input` becomes, or `None` when it is out of range.
pub(super) fn command(input: Input) -> Option<Command> {
    Some(match input {
        Input::Mouse {
            kind,
            x,
            y,
            button,
            buttons,
            clicks,
            mods,
        } => Command::Send(
            "Input.dispatchMouseEvent",
            json!({
                "type": match kind {
                    MouseKind::Down => "mousePressed",
                    MouseKind::Up => "mouseReleased",
                    MouseKind::Move => "mouseMoved",
                },
                "x": coordinate(x)?,
                "y": coordinate(y)?,
                "button": match button {
                    Button::None => "none",
                    Button::Left => "left",
                    Button::Middle => "middle",
                    Button::Right => "right",
                },
                "buttons": buttons & 31,
                "clickCount": clicks.min(3),
                "modifiers": mods & ALL_MODIFIERS,
                "pointerType": "mouse",
            }),
        ),
        Input::Wheel { x, y, dx, dy, mods } => Command::Send(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseWheel",
                "x": coordinate(x)?,
                "y": coordinate(y)?,
                "deltaX": delta(dx)?,
                "deltaY": delta(dy)?,
                "modifiers": mods & ALL_MODIFIERS,
                "pointerType": "mouse",
            }),
        ),
        Input::Key {
            down,
            key,
            code,
            key_code,
            text,
            mods,
            repeat,
        } => {
            if key.is_empty() || key.len() > 32 || code.len() > 32 || text.chars().count() > 1 {
                return None;
            }
            let mods = mods & ALL_MODIFIERS;
            // A shortcut types nothing, and Enter types a carriage return,
            // which is what submits a form.
            let text = if mods & (CTRL | META) != 0 {
                String::new()
            } else if key == "Enter" {
                "\r".to_string()
            } else {
                text
            };
            let kind = match (down, text.is_empty()) {
                (false, _) => "keyUp",
                (true, true) => "rawKeyDown",
                (true, false) => "keyDown",
            };
            Command::Send(
                "Input.dispatchKeyEvent",
                json!({
                    "type": kind,
                    "key": key,
                    "code": code,
                    "windowsVirtualKeyCode": key_code,
                    "nativeVirtualKeyCode": key_code,
                    "text": text,
                    "unmodifiedText": text,
                    "modifiers": mods,
                    "autoRepeat": repeat,
                }),
            )
        }
        Input::Text { text } => {
            if text.is_empty() || text.len() > MOST_TEXT {
                return None;
            }
            Command::Send("Input.insertText", json!({ "text": text }))
        }
        Input::Back => Command::History(-1),
        Input::Forward => Command::History(1),
        Input::Reload => Command::Reload,
    })
}

fn coordinate(value: f64) -> Option<f64> {
    (value.is_finite() && (0.0..=MOST_COORDINATE).contains(&value)).then_some(value)
}

fn delta(value: f64) -> Option<f64> {
    (value.is_finite() && value.abs() <= MOST_COORDINATE).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(json: &str) -> Option<Command> {
        let mut events = parse(json.as_bytes()).expect("the page's shape");
        assert_eq!(events.len(), 1);
        command(events.remove(0))
    }

    fn sent(json: &str) -> (&'static str, Value) {
        match one(json) {
            Some(Command::Send(method, params)) => (method, params),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_click_is_a_press_and_a_release_where_it_happened() {
        let (method, params) = sent(
            r#"[{"t":"mouse","kind":"down","x":10.5,"y":20,"button":"left","buttons":1,"clicks":1}]"#,
        );
        assert_eq!(method, "Input.dispatchMouseEvent");
        assert_eq!(params["type"], "mousePressed");
        assert_eq!(params["x"], 10.5);
        assert_eq!(params["button"], "left");
        assert_eq!(params["clickCount"], 1);
        let (_, params) = sent(r#"[{"t":"mouse","kind":"up","x":10,"y":20,"button":"left"}]"#);
        assert_eq!(params["type"], "mouseReleased");
        let (_, params) = sent(r#"[{"t":"mouse","kind":"move","x":1,"y":2}]"#);
        assert_eq!(params["type"], "mouseMoved");
        assert_eq!(params["button"], "none");
    }

    /// Off the screen, or not a number at all, is not where a mouse is.
    #[test]
    fn a_pointer_off_the_screen_is_dropped() {
        assert_eq!(one(r#"[{"t":"mouse","kind":"move","x":-1,"y":2}]"#), None);
        assert_eq!(one(r#"[{"t":"mouse","kind":"move","x":1,"y":1e9}]"#), None);
        assert_eq!(one(r#"[{"t":"wheel","x":1,"y":1,"dx":0,"dy":1e12}]"#), None);
    }

    #[test]
    fn a_character_is_typed_and_a_shortcut_is_not() {
        let (method, params) =
            sent(r#"[{"t":"key","down":true,"key":"a","code":"KeyA","keyCode":65,"text":"a"}]"#);
        assert_eq!(method, "Input.dispatchKeyEvent");
        assert_eq!(params["type"], "keyDown");
        assert_eq!(params["text"], "a");
        assert_eq!(params["windowsVirtualKeyCode"], 65);

        let (_, params) = sent(
            r#"[{"t":"key","down":true,"key":"a","code":"KeyA","keyCode":65,"text":"a","mods":2}]"#,
        );
        assert_eq!(params["type"], "rawKeyDown");
        assert_eq!(params["text"], "");
        assert_eq!(params["modifiers"], 2);

        let (_, params) = sent(r#"[{"t":"key","down":false,"key":"a","code":"KeyA"}]"#);
        assert_eq!(params["type"], "keyUp");
    }

    /// Enter carries the carriage return that submits a form; Backspace
    /// types nothing.
    #[test]
    fn enter_submits_and_backspace_deletes() {
        let (_, params) =
            sent(r#"[{"t":"key","down":true,"key":"Enter","code":"Enter","keyCode":13}]"#);
        assert_eq!(params["type"], "keyDown");
        assert_eq!(params["text"], "\r");
        let (_, params) =
            sent(r#"[{"t":"key","down":true,"key":"Backspace","code":"Backspace","keyCode":8}]"#);
        assert_eq!(params["type"], "rawKeyDown");
        assert_eq!(params["text"], "");
    }

    #[test]
    fn a_paste_is_inserted_whole() {
        let (method, params) = sent(r#"[{"t":"text","text":"correct horse"}]"#);
        assert_eq!(method, "Input.insertText");
        assert_eq!(params["text"], "correct horse");
        assert_eq!(one(r#"[{"t":"text","text":""}]"#), None);
        let long = format!(r#"[{{"t":"text","text":"{}"}}]"#, "x".repeat(MOST_TEXT + 1));
        assert_eq!(one(&long), None);
    }

    #[test]
    fn the_pages_buttons_move_through_the_history() {
        assert_eq!(one(r#"[{"t":"back"}]"#), Some(Command::History(-1)));
        assert_eq!(one(r#"[{"t":"forward"}]"#), Some(Command::History(1)));
        assert_eq!(one(r#"[{"t":"reload"}]"#), Some(Command::Reload));
    }

    /// A post the page would never send is refused whole.
    #[test]
    fn what_the_page_never_sends_is_refused() {
        assert!(parse(br#"[{"t":"navigate","url":"https://example.com"}]"#).is_none());
        assert!(parse(br#"{"t":"back"}"#).is_none());
        assert!(parse(b"not json").is_none());
        let many = format!("[{}]", vec![r#"{"t":"back"}"#; MOST_EVENTS + 1].join(","));
        assert!(parse(many.as_bytes()).is_none());
    }
}
