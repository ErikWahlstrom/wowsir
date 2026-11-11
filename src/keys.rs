//! Key name → CDP Input.dispatchKeyEvent parameters.

use serde_json::{json, Value};

pub struct Key {
    pub key: String,
    pub code: String,
    pub vk: i64,
    pub text: Option<String>,
}

pub fn modifier_bit(name: &str) -> Option<i64> {
    match name.to_ascii_lowercase().as_str() {
        "alt" | "option" => Some(1),
        "control" | "ctrl" => Some(2),
        "meta" | "cmd" | "command" | "super" => Some(4),
        "shift" => Some(8),
        "mod" => Some(if cfg!(target_os = "macos") { 4 } else { 2 }),
        _ => None,
    }
}

fn modifier_key(bit: i64) -> Key {
    let (k, c, vk) = match bit {
        1 => ("Alt", "AltLeft", 18),
        2 => ("Control", "ControlLeft", 17),
        4 => ("Meta", "MetaLeft", 91),
        _ => ("Shift", "ShiftLeft", 16),
    };
    Key { key: k.into(), code: c.into(), vk, text: None }
}

pub fn lookup(name: &str) -> Option<Key> {
    let named = |k: &str, c: &str, vk: i64, text: Option<&str>| Key {
        key: k.into(),
        code: c.into(),
        vk,
        text: text.map(String::from),
    };
    let lower = name.to_ascii_lowercase();
    let k = match lower.as_str() {
        "enter" | "return" => named("Enter", "Enter", 13, Some("\r")),
        "tab" => named("Tab", "Tab", 9, None),
        "escape" | "esc" => named("Escape", "Escape", 27, None),
        "backspace" => named("Backspace", "Backspace", 8, None),
        "delete" | "del" => named("Delete", "Delete", 46, None),
        "space" => named(" ", "Space", 32, Some(" ")),
        "arrowup" | "up" => named("ArrowUp", "ArrowUp", 38, None),
        "arrowdown" | "down" => named("ArrowDown", "ArrowDown", 40, None),
        "arrowleft" | "left" => named("ArrowLeft", "ArrowLeft", 37, None),
        "arrowright" | "right" => named("ArrowRight", "ArrowRight", 39, None),
        "home" => named("Home", "Home", 36, None),
        "end" => named("End", "End", 35, None),
        "pageup" => named("PageUp", "PageUp", 33, None),
        "pagedown" => named("PageDown", "PageDown", 34, None),
        "insert" => named("Insert", "Insert", 45, None),
        _ => {
            if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<i64>().ok()) {
                if (1..=12).contains(&n) {
                    return Some(Key { key: format!("F{n}"), code: format!("F{n}"), vk: 111 + n, text: None });
                }
            }
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            return Some(char_key(c));
        }
    };
    Some(k)
}

pub fn char_key(c: char) -> Key {
    let (code, vk) = if c.is_ascii_alphabetic() {
        (format!("Key{}", c.to_ascii_uppercase()), c.to_ascii_uppercase() as i64)
    } else if c.is_ascii_digit() {
        (format!("Digit{c}"), c as i64)
    } else if c == ' ' {
        ("Space".into(), 32)
    } else if c == '\n' {
        return Key { key: "Enter".into(), code: "Enter".into(), vk: 13, text: Some("\r".into()) };
    } else {
        (String::new(), 0)
    };
    Key { key: c.to_string(), code, vk, text: Some(c.to_string()) }
}

/// "Control+Shift+a" → list of (down, up) event param pairs, in press order.
pub fn chord(spec: &str) -> Result<Vec<Value>, String> {
    let parts: Vec<&str> = if spec == "+" { vec!["+"] } else { spec.split('+').collect() };
    let mut mods = 0i64;
    let mut mod_keys = vec![];
    let mut main: Option<Key> = None;
    for (i, p) in parts.iter().enumerate() {
        let last = i == parts.len() - 1;
        if !last {
            let b = modifier_bit(p).ok_or_else(|| format!("unknown modifier '{p}'"))?;
            mods |= b;
            mod_keys.push(b);
        } else if let Some(b) = modifier_bit(p) {
            mod_keys.push(b);
            mods |= b;
        } else {
            main = Some(lookup(p).ok_or_else(|| format!("unknown key '{p}'"))?);
        }
    }
    let mut evs = vec![];
    let mut held = 0i64;
    for b in &mod_keys {
        held |= b;
        let k = modifier_key(*b);
        evs.push(json!({"type":"rawKeyDown","key":k.key,"code":k.code,"windowsVirtualKeyCode":k.vk,"modifiers":held}));
    }
    if let Some(k) = main {
        // With Ctrl/Meta held, no text is produced (it's a shortcut).
        let text = if mods & (2 | 4) != 0 { None } else { k.text.clone() };
        let mut down = json!({"type": if text.is_some() {"keyDown"} else {"rawKeyDown"},
            "key":k.key,"code":k.code,"windowsVirtualKeyCode":k.vk,"modifiers":mods});
        if let Some(t) = &text {
            let t = if mods & 8 != 0 && t.len() == 1 { t.to_uppercase() } else { t.clone() };
            down["text"] = json!(t);
            down["unmodifiedText"] = json!(t);
        }
        // macOS editing shortcuts need explicit commands in headless Chrome.
        if cfg!(target_os = "macos") && mods & 4 != 0 {
            let cmd = match k.key.to_ascii_lowercase().as_str() {
                "a" => Some("selectAll"),
                "c" => Some("copy"),
                "x" => Some("cut"),
                "v" => Some("paste"),
                "z" => Some(if mods & 8 != 0 { "redo" } else { "undo" }),
                _ => None,
            };
            if let Some(c) = cmd {
                down["commands"] = json!([c]);
            }
        }
        evs.push(down);
        evs.push(json!({"type":"keyUp","key":k.key,"code":k.code,"windowsVirtualKeyCode":k.vk,"modifiers":mods}));
    }
    for b in mod_keys.iter().rev() {
        held &= !b;
        let k = modifier_key(*b);
        evs.push(json!({"type":"keyUp","key":k.key,"code":k.code,"windowsVirtualKeyCode":k.vk,"modifiers":held}));
    }
    Ok(evs)
}
