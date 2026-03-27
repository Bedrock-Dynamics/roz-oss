/// Insert a character at the given char index position.
pub fn insert_char(text: &str, pos: usize, ch: char) -> (String, usize) {
    let byte_pos = char_to_byte_pos(text, pos);
    let mut result = String::with_capacity(text.len() + ch.len_utf8());
    result.push_str(&text[..byte_pos]);
    result.push(ch);
    result.push_str(&text[byte_pos..]);
    (result, pos + 1)
}

/// Delete the character before the cursor (backspace).
pub fn delete_before(text: &str, pos: usize) -> (String, usize) {
    if pos == 0 {
        return (text.to_string(), 0);
    }
    let byte_pos = char_to_byte_pos(text, pos);
    let prev_byte_pos = char_to_byte_pos(text, pos - 1);
    let mut result = String::with_capacity(text.len());
    result.push_str(&text[..prev_byte_pos]);
    result.push_str(&text[byte_pos..]);
    (result, pos - 1)
}

/// Delete the character at the cursor (delete key).
pub fn delete_at(text: &str, pos: usize) -> String {
    let char_count = text.chars().count();
    if pos >= char_count {
        return text.to_string();
    }
    let byte_pos = char_to_byte_pos(text, pos);
    let next_byte_pos = char_to_byte_pos(text, pos + 1);
    let mut result = String::with_capacity(text.len());
    result.push_str(&text[..byte_pos]);
    result.push_str(&text[next_byte_pos..]);
    result
}

/// Delete the word before the cursor (Ctrl-W).
pub fn delete_word_back(text: &str, pos: usize) -> (String, usize) {
    if pos == 0 {
        return (text.to_string(), 0);
    }
    let chars: Vec<char> = text.chars().collect();

    // Skip trailing whitespace
    let mut new_pos = pos;
    while new_pos > 0 && chars[new_pos - 1].is_whitespace() {
        new_pos -= 1;
    }
    // Skip word chars
    while new_pos > 0 && !chars[new_pos - 1].is_whitespace() {
        new_pos -= 1;
    }

    let start_byte = char_to_byte_pos(text, new_pos);
    let end_byte = char_to_byte_pos(text, pos);
    let mut result = String::with_capacity(text.len());
    result.push_str(&text[..start_byte]);
    result.push_str(&text[end_byte..]);
    (result, new_pos)
}

/// Convert a char index to a byte position.
fn char_to_byte_pos(text: &str, char_pos: usize) -> usize {
    text.char_indices()
        .nth(char_pos)
        .map_or(text.len(), |(byte_pos, _)| byte_pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_at_start() {
        let (result, pos) = insert_char("hello", 0, 'x');
        assert_eq!(result, "xhello");
        assert_eq!(pos, 1);
    }

    #[test]
    fn insert_at_end() {
        let (result, pos) = insert_char("hello", 5, '!');
        assert_eq!(result, "hello!");
        assert_eq!(pos, 6);
    }

    #[test]
    fn insert_in_middle() {
        let (result, pos) = insert_char("hllo", 1, 'e');
        assert_eq!(result, "hello");
        assert_eq!(pos, 2);
    }

    #[test]
    fn backspace_at_start() {
        let (result, pos) = delete_before("hello", 0);
        assert_eq!(result, "hello");
        assert_eq!(pos, 0);
    }

    #[test]
    fn backspace_at_end() {
        let (result, pos) = delete_before("hello", 5);
        assert_eq!(result, "hell");
        assert_eq!(pos, 4);
    }

    #[test]
    fn delete_at_cursor() {
        let result = delete_at("hello", 2);
        assert_eq!(result, "helo");
    }

    #[test]
    fn delete_at_end_is_noop() {
        let result = delete_at("hello", 5);
        assert_eq!(result, "hello");
    }

    #[test]
    fn delete_word_basic() {
        let (result, pos) = delete_word_back("hello world", 11);
        assert_eq!(result, "hello ");
        assert_eq!(pos, 6);
    }

    #[test]
    fn delete_word_with_trailing_space() {
        let (result, pos) = delete_word_back("hello  ", 7);
        assert_eq!(result, "");
        assert_eq!(pos, 0);
    }

    #[test]
    fn unicode_insert() {
        let (result, pos) = insert_char("caf", 3, '\u{00e9}');
        assert_eq!(result, "caf\u{00e9}");
        assert_eq!(pos, 4);
    }
}
