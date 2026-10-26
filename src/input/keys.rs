//! Key notation parsing: `"g g"`, `"ctrl-s"`, `"shift-tab"`, `"<"`.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

/// Symbols that require shift on a US layout; bindings for them match the
/// shifted event terminals actually send.
const SHIFTED: &[char] = &[
    '~', '!', '@', '#', '$', '%', '^', '&', '*', '(', ')', '_', '+', '{', '}', '|', ':', '"', '<',
    '>', '?',
];

/// Parse a single key token such as `ctrl-alt-del`, `W`, `enter` or `<`.
pub fn parse_key(token: &str) -> Option<KeyEvent> {
    // A bare space is a key of its own (`<space>` in a config file).
    if token == " " {
        return Some(event(KeyCode::Char(' '), KeyModifiers::NONE));
    }
    let token = token.trim();
    // `<C-x>`, `<S-Tab>`: vim's angle notation wraps the same name.
    if let Some(inner) = token
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        && !inner.contains(['<', '>'])
    {
        return parse_key(inner);
    }
    if token.is_empty() {
        return None;
    }
    let (modifiers, key_part) = split_modifiers(token)?;

    // A lone modifier is not a key.
    if key_part.is_empty() {
        return None;
    }

    let mut code = parse_key_code(key_part)?;
    // Uppercase letters and shifted symbols imply shift.
    let mut modifiers = modifiers;
    match code {
        KeyCode::Char(c) => {
            if c.is_ascii_uppercase() || SHIFTED.contains(&c) {
                modifiers |= KeyModifiers::SHIFT;
            }
        }
        KeyCode::BackTab => modifiers |= KeyModifiers::SHIFT,
        _ => {}
    }
    // Shifted Tab is its own key: `shift-tab`, `s-tab` and `<S-Tab>` all mean it.
    if code == KeyCode::Tab && modifiers.contains(KeyModifiers::SHIFT) {
        code = KeyCode::BackTab;
    }
    Some(event(code, modifiers))
}

/// Parse a whitespace separated sequence: `"g g"` → `[g, g]`.
/// Key names that stand for one key even when written without separators.
///
/// Every name `parse_key_code` understands belongs here, otherwise `minus`
/// would split into `m`, `i`, `n` … (a test checks the two lists agree).
const KEY_NAMES: &[&str] = &[
    "pagedown",
    "backspace",
    "backquote",
    "backslash",
    "tabulator",
    "spacebar",
    "fullstop",
    "semicolon",
    "underscore",
    "newline",
    "pageup",
    "escape",
    "insert",
    "delete",
    "return",
    "greater",
    "squote",
    "dquote",
    "quote",
    "comma",
    "minus",
    "slash",
    "colon",
    "equal",
    "equals",
    "space",
    "tilde",
    "enter",
    "dash",
    "left",
    "down",
    "right",
    "tab",
    "esc",
    "plus",
    "dot",
    "bar",
    "ins",
    "del",
    "home",
    "lt",
    "gt",
    "less",
    "cr",
    "up",
    "end",
    "pgup",
    "pgdn",
    "pgdown",
    "period",
    "pipe",
    "bs",
];

/// Modifier prefixes, longest first so `control-` wins over `c-`.
const MODIFIER_PREFIXES: &[(&str, KeyModifiers)] = &[
    ("control-", KeyModifiers::CONTROL),
    ("ctrl-", KeyModifiers::CONTROL),
    ("shift-", KeyModifiers::SHIFT),
    ("alt-", KeyModifiers::ALT),
    ("meta-", KeyModifiers::ALT),
    ("c-", KeyModifiers::CONTROL),
    ("s-", KeyModifiers::SHIFT),
    ("a-", KeyModifiers::ALT),
    ("m-", KeyModifiers::ALT),
];

/// Split a key notation into single-key tokens.
///
/// `gg`, `d$` and `ciw` are separate keys with no separator between them, while
/// `ctrl-x`, `<S-Tab>` and `space` each name one key. The longest known name
/// wins at each position, so `space f` is the leader ` ` followed by `f`, and
/// `space` on its own is the space bar.
fn split_notations(text: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let current = chars[index];
        if current.is_whitespace() {
            index += 1;
            continue;
        }
        // `<C-x>` style notation covers exactly one key; a lone `<` is the key.
        if current == '<' {
            match (index + 1..chars.len()).find(|&at| chars[at] == '>') {
                Some(end) => {
                    tokens.push(chars[index..=end].iter().collect());
                    index = end + 1;
                }
                None => {
                    tokens.push("<".to_string());
                    index += 1;
                }
            }
            continue;
        }
        // A bare `"` is the quote key; `"…"` quotes a name.
        if current == '"' {
            if let Some(end) = (index + 1..chars.len()).find(|&at| chars[at] == '"') {
                tokens.push(chars[index + 1..end].iter().collect());
                index = end + 1;
                continue;
            }
            tokens.push("\"".to_string());
            index += 1;
            continue;
        }

        let rest: String = chars[index..].iter().collect();
        let lowered = rest.to_lowercase();

        // Modifiers glue themselves to the key that follows.
        let mut prefix = String::new();
        let mut offset = 0usize;
        loop {
            let tail = &lowered[offset..];
            match MODIFIER_PREFIXES
                .iter()
                .find(|(name, _)| tail.starts_with(name))
            {
                Some((name, _)) => {
                    prefix.push_str(&rest[offset..offset + name.len()]);
                    offset += name.chars().count();
                    if offset >= lowered.len() {
                        break;
                    }
                }
                None => break,
            }
        }
        if !prefix.is_empty() && offset < lowered.len() {
            // The base is a name (`ctrl-pageup`) or a single character (`c-x`).
            let tail = &lowered[offset..];
            let name = longest_name(tail);
            match name {
                Some(name) => {
                    let mut token = prefix;
                    token.push_str(&rest[offset..offset + name.len()]);
                    tokens.push(token);
                    index += offset + name.chars().count();
                }
                None => {
                    let mut token = prefix;
                    token.push(chars[index + offset]);
                    tokens.push(token);
                    index += offset + 1;
                }
            }
            continue;
        }
        if !prefix.is_empty() {
            // Trailing modifier with nothing after it.
            return None;
        }

        // Plain names and function keys, then a single character.
        if let Some(name) = longest_name(&lowered) {
            tokens.push(rest[..name.len()].to_string());
            index += name.chars().count();
            continue;
        }
        tokens.push(current.to_string());
        index += 1;
    }
    if tokens.is_empty() {
        None
    } else {
        Some(tokens)
    }
}

/// The longest key name that `text` starts with.
///
/// A name only counts when it ends on a boundary: otherwise typing `deleted`
/// would be read as `delete` followed by `d`. Names after a modifier prefix
/// (`ctrl-pageup`) are matched by the caller with the prefix already stripped.
fn longest_name(text: &str) -> Option<&'static str> {
    KEY_NAMES
        .iter()
        .chain(F_KEY_NAMES.iter())
        .filter(|name| text.starts_with(**name))
        .filter(|name| {
            let tail = text.chars().nth(name.chars().count());
            !tail.is_some_and(|next| next.is_alphanumeric() || next == '-')
        })
        .max_by_key(|name| name.len())
        .copied()
}

/// `f1` … `f24`, which would otherwise split into `f` and a digit.
static F_KEY_NAMES: &[&str] = &[
    "f12", "f24", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f13", "f14",
    "f15", "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23",
];

/// Parse a key sequence such as `gg`, `d$`, `<C-w>q` or `space f`.
pub fn parse_key_sequence(text: &str) -> Option<Vec<KeyEvent>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut keys = Vec::new();
    for token in split_notations(text)? {
        keys.push(parse_key(&token)?);
    }
    if keys.is_empty() { None } else { Some(keys) }
}

fn split_modifiers(token: &str) -> Option<(KeyModifiers, &str)> {
    let mut modifiers = KeyModifiers::empty();
    let mut rest = token;
    loop {
        let lowered = rest.to_lowercase();
        let (name, modifier) = if lowered.starts_with("ctrl-") {
            ("ctrl-", KeyModifiers::CONTROL)
        } else if lowered.starts_with("control-") {
            ("control-", KeyModifiers::CONTROL)
        } else if lowered.starts_with("c-") {
            ("c-", KeyModifiers::CONTROL)
        } else if lowered.starts_with("alt-") {
            ("alt-", KeyModifiers::ALT)
        } else if lowered.starts_with("meta-") {
            ("meta-", KeyModifiers::ALT)
        } else if lowered.starts_with("m-") {
            ("m-", KeyModifiers::ALT)
        } else if lowered.starts_with("shift-") {
            ("shift-", KeyModifiers::SHIFT)
        } else if lowered.starts_with("s-") {
            ("s-", KeyModifiers::SHIFT)
        } else {
            break;
        };
        modifiers |= modifier;
        rest = &rest[name.len()..];
    }
    Some((modifiers, rest))
}

fn parse_key_code(text: &str) -> Option<KeyCode> {
    let lowered = text.to_lowercase();
    let code = match lowered.as_str() {
        "enter" | "return" | "cr" | "newline" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "space" | "spacebar" | "spc" => KeyCode::Char(' '),
        "tab" | "tabulator" => KeyCode::Tab,
        "backtab" | "shift-tab" => KeyCode::BackTab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        "up" | "k-arrow" => KeyCode::Up,
        "down" | "j-arrow" => KeyCode::Down,
        "left" | "h-arrow" => KeyCode::Left,
        "right" | "l-arrow" => KeyCode::Right,
        "minus" | "dash" => KeyCode::Char('-'),
        "equal" | "equals" => KeyCode::Char('='),
        "plus" => KeyCode::Char('+'),
        "lt" | "less" => KeyCode::Char('<'),
        "gt" | "greater" => KeyCode::Char('>'),
        "comma" => KeyCode::Char(','),
        "semicolon" => KeyCode::Char(';'),
        "colon" => KeyCode::Char(':'),
        "quote" | "squote" => KeyCode::Char('\''),
        "dquote" => KeyCode::Char('"'),
        "backquote" | "tick" => KeyCode::Char('`'),
        "backslash" | "bslash" => KeyCode::Char('\\'),
        "slash" => KeyCode::Char('/'),
        "dot" | "period" | "fullstop" => KeyCode::Char('.'),
        "underscore" => KeyCode::Char('_'),
        "pipe" | "bar" => KeyCode::Char('|'),
        "tilde" => KeyCode::Char('~'),
        other => {
            if let Some(digits) = other.strip_prefix('f')
                && let Ok(number) = digits.parse::<u8>()
                && (1..=24).contains(&number)
            {
                KeyCode::F(number)
            } else {
                let mut chars = text.chars();
                let ch = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                KeyCode::Char(ch)
            }
        }
    };
    Some(code)
}

fn event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

/// Strip information that terminals vary about, so lookups are stable.
pub fn normalize(event: KeyEvent) -> KeyEvent {
    KeyEvent {
        code: event.code,
        modifiers: event.modifiers
            & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT),
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

/// Shifted symbol and the key underneath it, on a US layout.
const SHIFTED_PAIRS: &[(char, char)] = &[
    ('~', '`'),
    ('!', '1'),
    ('@', '2'),
    ('#', '3'),
    ('$', '4'),
    ('%', '5'),
    ('^', '6'),
    ('&', '7'),
    ('*', '8'),
    ('(', '9'),
    (')', '0'),
    ('_', '-'),
    ('+', '='),
    ('{', '['),
    ('}', ']'),
    ('|', '\\'),
    (':', ';'),
    ('"', '\''),
    ('<', ','),
    ('>', '.'),
    ('?', '/'),
];

/// The shifted partner of a symbol, if it has one.
pub fn shifted_of(c: char) -> Option<char> {
    SHIFTED_PAIRS
        .iter()
        .find(|(_, plain)| *plain == c)
        .map(|(shifted, _)| *shifted)
}

/// The unshifted partner of a symbol, if it has one.
pub fn unshifted_of(c: char) -> Option<char> {
    SHIFTED_PAIRS
        .iter()
        .find(|(shifted, _)| *shifted == c)
        .map(|(_, plain)| *plain)
}

/// Alternate spellings of the same physical key press, tried in order.
///
/// Terminals disagree about whether `Shift` is reported for shifted symbols, and
/// some report the unshifted key with the modifier still held.  Letter case is
/// *not* aliased: `w` and `W` are different bindings.
pub fn lookup_variants(event: KeyEvent) -> Vec<KeyEvent> {
    let base = normalize(event);
    let mut variants = vec![base];
    let _shift = base.modifiers.contains(KeyModifiers::SHIFT);
    let toggle = |shift: KeyModifiers| -> KeyModifiers {
        if shift.contains(KeyModifiers::SHIFT) {
            shift.difference(KeyModifiers::SHIFT)
        } else {
            shift.union(KeyModifiers::SHIFT)
        }
    };
    variants.push(KeyEvent {
        modifiers: toggle(base.modifiers),
        ..base
    });

    if let KeyCode::Char(c) = base.code {
        // A symbol can arrive as itself, or as the key underneath it with Shift
        // held — both should find a binding written either way.
        for partner in [shifted_of(c), unshifted_of(c)].into_iter().flatten() {
            variants.push(KeyEvent {
                code: KeyCode::Char(partner),
                modifiers: base.modifiers | KeyModifiers::SHIFT,
                ..base
            });
            variants.push(KeyEvent {
                code: KeyCode::Char(partner),
                modifiers: base.modifiers & !KeyModifiers::SHIFT,
                ..base
            });
        }
    }

    let mut unique: Vec<KeyEvent> = Vec::with_capacity(variants.len());
    for variant in variants {
        let seen = unique.iter().any(|existing: &KeyEvent| {
            existing.code == variant.code && existing.modifiers == variant.modifiers
        });
        if !seen {
            unique.push(variant);
        }
    }
    unique
}

/// Render a key for help output: `ctrl-s`, `shift-tab`, `g`.
pub fn format_key(event: KeyEvent) -> String {
    let mut out = String::new();
    let modifiers = normalize(event).modifiers;
    for (name, bit) in [
        ("ctrl", KeyModifiers::CONTROL),
        ("alt", KeyModifiers::ALT),
        ("shift", KeyModifiers::SHIFT),
    ] {
        if modifiers.contains(bit) {
            out.push_str(name);
            out.push('-');
        }
    }
    match event.code {
        KeyCode::Char(c) => out.push(c),
        // Function keys carry a number, so they cannot use the static names.
        KeyCode::F(number) => out.push_str(&format!("f{number}")),
        code => out.push_str(key_name(code)),
    }
    out
}

fn key_name(code: KeyCode) -> &'static str {
    match code {
        KeyCode::Enter => "enter",
        KeyCode::Tab => "tab",
        KeyCode::BackTab => "shift-tab",
        KeyCode::Backspace => "backspace",
        KeyCode::Delete => "delete",
        KeyCode::Insert => "insert",
        KeyCode::Esc => "esc",
        KeyCode::Up => "up",
        KeyCode::Down => "down",
        KeyCode::Left => "left",
        KeyCode::Right => "right",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::PageUp => "pageup",
        KeyCode::PageDown => "pagedown",
        KeyCode::F(_) => "f",
        _ => "?",
    }
}

/// Human readable form, e.g. for the status line echo of pending keys.
pub fn describe(event: KeyEvent) -> String {
    match event.code {
        KeyCode::Char(c) => {
            let modifiers = normalize(event).modifiers;
            if modifiers.is_empty() {
                c.to_string()
            } else {
                format_key(event)
            }
        }
        KeyCode::F(number) => format!("f{number}"),
        _ => format_key(event),
    }
}

/// A printable character the user typed, if it is one.
pub fn typed_char(event: KeyEvent) -> Option<char> {
    match event.code {
        KeyCode::Char(c)
            if !event
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(c)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<KeyEvent> {
        parse_key_sequence(text).unwrap_or_else(|| panic!("cannot parse {text:?}"))
    }

    #[test]
    fn parses_plain_characters() {
        let keys = keys("w");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].code, KeyCode::Char('w'));
        assert!(!keys[0].modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn uppercase_implies_shift() {
        let keys = keys("W");
        assert_eq!(keys[0].code, KeyCode::Char('W'));
        assert!(keys[0].modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn parses_sequences() {
        let keys = keys("g g");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].code, KeyCode::Char('g'));
        assert_eq!(keys[1].code, KeyCode::Char('g'));
    }

    #[test]
    fn parses_modifiers() {
        let parsed = keys("ctrl-s");
        assert_eq!(parsed[0].code, KeyCode::Char('s'));
        assert!(parsed[0].modifiers.contains(KeyModifiers::CONTROL));

        let parsed = keys("ctrl-alt-q");
        assert!(
            parsed[0]
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        );

        let parsed = keys("shift-tab");
        assert_eq!(parsed[0].code, KeyCode::BackTab);
    }

    #[test]
    fn parses_named_and_punctuation_keys() {
        assert_eq!(keys("enter")[0].code, KeyCode::Enter);
        assert_eq!(keys("esc")[0].code, KeyCode::Esc);
        assert_eq!(keys("space")[0].code, KeyCode::Char(' '));
        assert_eq!(keys("<")[0].code, KeyCode::Char('<'));
        assert!(keys("<")[0].modifiers.contains(KeyModifiers::SHIFT));
        assert_eq!(keys("minus")[0].code, KeyCode::Char('-'));
        assert_eq!(keys("f12")[0].code, KeyCode::F(12));
        // Quotes make a token out of something that contains a space.
        assert_eq!(keys("\"space\"")[0].code, KeyCode::Char(' '));
        assert!(parse_key_sequence("\"g g\"").is_none(), "not a single key");
    }

    #[test]
    fn splits_notations_without_separators() {
        assert_eq!(keys("gg").len(), 2, "`gg` is g then g");
        assert_eq!(keys("ciw").len(), 3);
        assert_eq!(keys("d$").len(), 2);
        assert_eq!(keys("yyp").len(), 3);
        assert_eq!(keys("space f").len(), 2, "leader then f");
        assert_eq!(keys("f1")[0].code, KeyCode::F(1));
        assert_eq!(keys("f12")[0].code, KeyCode::F(12));
        assert_eq!(keys("ctrl-pageup")[0].code, KeyCode::PageUp);
        assert_eq!(keys("<C-x>")[0].code, KeyCode::Char('x'));
        assert_eq!(keys("<S-Tab>")[0].code, KeyCode::BackTab);
        // A lone quote is the quote key, not the start of a quoted name.
        assert_eq!(keys("\"")[0].code, KeyCode::Char('"'));
        assert_eq!(keys("<")[0].code, KeyCode::Char('<'));
    }

    #[test]
    fn every_key_name_stays_one_key() {
        for name in KEY_NAMES {
            let parsed = parse_key_sequence(name);
            assert_eq!(
                parsed.as_deref().map(<[KeyEvent]>::len),
                Some(1),
                "{name} should name one key"
            );
        }
    }

    #[test]
    fn rejects_nonsense() {
        assert!(parse_key_sequence("").is_none());
        assert!(parse_key("ctrl-").is_none());
        assert!(parse_key("notakey").is_none());
    }

    #[test]
    fn variants_cover_shift_disagreements() {
        let event = event(KeyCode::Char('W'), KeyModifiers::SHIFT);
        let variants = lookup_variants(event);
        assert_eq!(variants.len(), 2);
        assert!(!variants[1].modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn formatted_keys_round_trip() {
        for name in [
            "ctrl-s",
            "shift-tab",
            "enter",
            "g",
            "space",
            "f3",
            "backspace",
        ] {
            let parsed = parse_key(name).unwrap();
            let rendered = format_key(parsed);
            assert_eq!(
                parse_key(&rendered).map(normalize),
                Some(normalize(parsed)),
                "{name} → {rendered}"
            );
        }
    }
}
