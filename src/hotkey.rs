//! Global shortcuts as text: "Ctrl+Alt+Z", "Win+Shift+F5". Settings store
//! them like that, the engine turns them into `RegisterHotKey` arguments and
//! the settings window records them from key presses.

/// `RegisterHotKey` modifier bits.
pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;
pub const MOD_WIN: u32 = 0x8;

/// Modifiers in the order they're written.
const MODS: [(&str, u32); 4] = [("Ctrl", MOD_CONTROL), ("Alt", MOD_ALT), ("Shift", MOD_SHIFT), ("Win", MOD_WIN)];

/// Named keys: (written name, virtual-key code, GPUI's name for it).
const KEYS: &[(&str, u32, &str)] = &[
    ("Space", 0x20, "space"),
    ("Enter", 0x0D, "enter"),
    ("Tab", 0x09, "tab"),
    ("Backspace", 0x08, "backspace"),
    ("Delete", 0x2E, "delete"),
    ("Insert", 0x2D, "insert"),
    ("Home", 0x24, "home"),
    ("End", 0x23, "end"),
    ("PageUp", 0x21, "pageup"),
    ("PageDown", 0x22, "pagedown"),
    ("Left", 0x25, "left"),
    ("Up", 0x26, "up"),
    ("Right", 0x27, "right"),
    ("Down", 0x28, "down"),
    (";", 0xBA, ";"),
    ("=", 0xBB, "="),
    (",", 0xBC, ","),
    ("-", 0xBD, "-"),
    (".", 0xBE, "."),
    ("/", 0xBF, "/"),
    ("`", 0xC0, "`"),
    ("[", 0xDB, "["),
    ("\\", 0xDC, "\\"),
    ("]", 0xDD, "]"),
    ("'", 0xDE, "'"),
];

/// A key's virtual-key code from its written name.
fn vk_of(name: &str) -> Option<u32> {
    let up = name.to_ascii_uppercase();
    if up.len() == 1 {
        let c = up.as_bytes()[0];
        if c.is_ascii_alphanumeric() {
            return Some(c as u32);
        }
    }
    if let Some(n) = up.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x70 + n - 1);
        }
    }
    KEYS.iter().find(|(k, _, _)| k.eq_ignore_ascii_case(name)).map(|&(_, vk, _)| vk)
}

fn is_function_key(name: &str) -> bool {
    name.len() > 1 && name.starts_with(['F', 'f']) && name[1..].parse::<u32>().is_ok()
}

/// `(modifiers, virtual key)` for `RegisterHotKey`, or None if `s` is empty
/// or not a shortcut.
pub fn parse(s: &str) -> Option<(u32, u32)> {
    let mut mods = 0;
    let mut key = None;
    for part in s.split('+').map(str::trim) {
        match MODS.iter().find(|(m, _)| m.eq_ignore_ascii_case(part)) {
            Some(&(_, bit)) => mods |= bit,
            // "+" itself can't be bound, so an empty part is a mistake.
            None if key.is_none() && !part.is_empty() => key = Some(part),
            None => return None,
        }
    }
    let key = key?;
    let vk = vk_of(key)?;
    // A bare letter would steal typing from every app; F-keys are fine.
    let needs_mod = mods & (MOD_CONTROL | MOD_ALT | MOD_WIN) == 0 && !is_function_key(key);
    if needs_mod {
        return None;
    }
    Some((mods, vk))
}

/// The canonical spelling of `s` ("ctrl+alt+z" -> "Ctrl+Alt+Z"); anything
/// unparseable becomes unbound.
pub fn normalized(s: &str) -> String {
    let Some((mods, _)) = parse(s) else { return String::new() };
    let key = s.split('+').map(str::trim).find(|p| !MODS.iter().any(|(m, _)| m.eq_ignore_ascii_case(p)));
    let key = key.map(|k| match KEYS.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(k)) {
        Some(&(n, _, _)) => n.to_string(),
        None => k.to_ascii_uppercase(),
    });
    join(mods, &key.unwrap_or_default())
}

fn join(mods: u32, key: &str) -> String {
    let mut parts: Vec<&str> = MODS.iter().filter(|(_, b)| mods & b != 0).map(|(m, _)| *m).collect();
    parts.push(key);
    parts.join("+")
}

/// What a key press in the settings window records: the shortcut, or None
/// while only modifiers are down or the key can't be a shortcut. `key` is
/// GPUI's key name ("a", "f5", "pageup").
pub fn from_press(key: &str, ctrl: bool, alt: bool, shift: bool, win: bool) -> Option<String> {
    let name = match KEYS.iter().find(|(_, _, g)| *g == key) {
        Some(&(n, _, _)) => n.to_string(),
        None if key.chars().count() == 1 || is_function_key(key) => key.to_ascii_uppercase(),
        None => return None,
    };
    let mods = [(ctrl, MOD_CONTROL), (alt, MOD_ALT), (shift, MOD_SHIFT), (win, MOD_WIN)]
        .iter()
        .filter(|(on, _)| *on)
        .fold(0, |m, (_, b)| m | b);
    let s = join(mods, &name);
    parse(&s).map(|_| s)
}

/// The pieces to draw as key caps: "Ctrl+Alt+Z" -> ["Ctrl", "Alt", "Z"].
pub fn caps(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split('+').map(|p| p.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalizes() {
        assert_eq!(parse("Ctrl+Alt+Z"), Some((MOD_CONTROL | MOD_ALT, 'Z' as u32)));
        assert_eq!(parse("win+shift+f5"), Some((MOD_WIN | MOD_SHIFT, 0x74)));
        assert_eq!(parse("F9"), Some((0, 0x78)));
        assert_eq!(parse("Z"), None);
        assert_eq!(parse("Shift+Z"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("Ctrl+Alt"), None);
        assert_eq!(parse("Ctrl+A+B"), None);
        assert_eq!(normalized("alt+ctrl+pageup"), "Ctrl+Alt+PageUp");
        assert_eq!(normalized("ctrl+alt+z"), "Ctrl+Alt+Z");
        assert_eq!(normalized("nonsense"), "");
    }

    #[test]
    fn records_presses() {
        assert_eq!(from_press("z", true, true, false, false).as_deref(), Some("Ctrl+Alt+Z"));
        assert_eq!(from_press("pageup", false, false, true, true).as_deref(), Some("Shift+Win+PageUp"));
        assert_eq!(from_press("f2", false, false, false, false).as_deref(), Some("F2"));
        assert_eq!(from_press("a", false, false, false, false), None);
        assert_eq!(from_press("control", true, false, false, false), None);
        assert_eq!(caps("Ctrl+Alt+Z"), vec!["Ctrl", "Alt", "Z"]);
    }
}
