//! Text to key presses, for driving the desktop through the stream.
//!
//! waylanddisplaysrc maps an X keysym NAME (`a`, `Return`, `Shift_L`) to an evdev
//! scancode and presses that PHYSICAL key on its seat; the desktop's XKB layout then
//! decides which character it produces. So typing a character means pressing the
//! physical key a US layout puts it on, with Shift held where the US layout needs it.
//! Sending a shifted keysym name (`exclam`) would press the key WITHOUT Shift and type
//! the unshifted character, which is why this table maps to base names only.

/// The physical key (as a keysym name) and whether Shift is needed, for one
/// character on a US layout. `None` for a character no single key produces.
pub fn key_for_char(c: char) -> Option<(&'static str, bool)> {
    const LETTERS: &str = "abcdefghijklmnopqrstuvwxyz";
    const DIGITS: &[&str] = &["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
    if c.is_ascii_lowercase() {
        let i = LETTERS.find(c)?;
        return Some((LETTER_NAMES[i], false));
    }
    if c.is_ascii_uppercase() {
        let i = LETTERS.find(c.to_ascii_lowercase())?;
        return Some((LETTER_NAMES[i], true));
    }
    if let Some(d) = c.to_digit(10) {
        return Some((DIGITS[d as usize], false));
    }
    Some(match c {
        ' ' => ("space", false),
        '\n' => ("Return", false),
        '\t' => ("Tab", false),
        '-' => ("minus", false),
        '_' => ("minus", true),
        '=' => ("equal", false),
        '+' => ("equal", true),
        '[' => ("bracketleft", false),
        '{' => ("bracketleft", true),
        ']' => ("bracketright", false),
        '}' => ("bracketright", true),
        '\\' => ("backslash", false),
        '|' => ("backslash", true),
        ';' => ("semicolon", false),
        ':' => ("semicolon", true),
        '\'' => ("apostrophe", false),
        '"' => ("apostrophe", true),
        '`' => ("grave", false),
        '~' => ("grave", true),
        ',' => ("comma", false),
        '<' => ("comma", true),
        '.' => ("period", false),
        '>' => ("period", true),
        '/' => ("slash", false),
        '?' => ("slash", true),
        '!' => ("1", true),
        '@' => ("2", true),
        '#' => ("3", true),
        '$' => ("4", true),
        '%' => ("5", true),
        '^' => ("6", true),
        '&' => ("7", true),
        '*' => ("8", true),
        '(' => ("9", true),
        ')' => ("0", true),
        _ => return None,
    })
}

static LETTER_NAMES: [&str; 26] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s",
    "t", "u", "v", "w", "x", "y", "z",
];

/// One key transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStep {
    Press(&'static str),
    Release(&'static str),
}

/// The key transitions that type `text`, or the first character that cannot be
/// typed. Shift is pressed around each shifted character and released after it, so
/// the sequence never leaves a modifier held.
pub fn steps_for_text(text: &str) -> Result<Vec<KeyStep>, char> {
    let mut out = Vec::new();
    for c in text.chars() {
        let (key, shift) = key_for_char(c).ok_or(c)?;
        if shift {
            out.push(KeyStep::Press("Shift_L"));
        }
        out.push(KeyStep::Press(key));
        out.push(KeyStep::Release(key));
        if shift {
            out.push(KeyStep::Release("Shift_L"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use KeyStep::*;

    #[test]
    fn letters_digits_and_space_are_unshifted_base_keys() {
        assert_eq!(key_for_char('a'), Some(("a", false)));
        assert_eq!(key_for_char('z'), Some(("z", false)));
        assert_eq!(key_for_char('7'), Some(("7", false)));
        assert_eq!(key_for_char(' '), Some(("space", false)));
    }

    /// A shifted character is its BASE key plus Shift. Sending the shifted keysym
    /// name instead (`exclam`) presses KEY_1 without Shift and types `1`.
    #[test]
    fn shifted_characters_press_the_base_key_with_shift() {
        assert_eq!(key_for_char('A'), Some(("a", true)));
        assert_eq!(key_for_char('!'), Some(("1", true)));
        assert_eq!(key_for_char('>'), Some(("period", true)));
        assert_eq!(key_for_char('"'), Some(("apostrophe", true)));
    }

    #[test]
    fn typing_wraps_shift_around_each_shifted_character_only() {
        assert_eq!(
            steps_for_text("aB").unwrap(),
            vec![
                Press("a"),
                Release("a"),
                Press("Shift_L"),
                Press("b"),
                Release("b"),
                Release("Shift_L"),
            ]
        );
    }

    #[test]
    fn an_untypeable_character_is_named() {
        assert_eq!(steps_for_text("ok€"), Err('€'));
    }

    /// Every key this table can press is one waylanddisplaysrc's keysym table maps
    /// (its `gst_key_to_scancode`); a name outside it is dropped by the element.
    #[test]
    fn every_key_is_a_name_the_element_maps() {
        const ELEMENT_NAMES: &[&str] = &[
            "space",
            "Return",
            "Tab",
            "minus",
            "equal",
            "bracketleft",
            "bracketright",
            "backslash",
            "semicolon",
            "apostrophe",
            "grave",
            "comma",
            "period",
            "slash",
            "Shift_L",
            "0",
            "1",
            "2",
            "3",
            "4",
            "5",
            "6",
            "7",
            "8",
            "9",
        ];
        let all: String = (32u8..127).map(char::from).collect::<String>() + "\n\t";
        for c in all.chars() {
            let (key, _) = key_for_char(c).unwrap_or_else(|| panic!("{c:?} is untypeable"));
            assert!(
                key.len() == 1
                    && key
                        .chars()
                        .all(|k| k.is_ascii_lowercase() || k.is_ascii_digit())
                    || ELEMENT_NAMES.contains(&key),
                "{c:?} maps to {key}, which the element does not map"
            );
        }
    }
}
