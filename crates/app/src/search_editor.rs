use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditOutcome {
    Consumed,
    Submit,
    Blur,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditorMode {
    Standard,
    Insert,
    Normal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operator {
    Change,
    Delete,
}

pub struct SearchEditor {
    editing_mode: EditorMode,
    cursor_byte_index: usize,
    pending_operator: Option<Operator>,
    undo_stack: Vec<(String, usize)>,
    redo_stack: Vec<(String, usize)>,
}

impl SearchEditor {
    pub fn new(vim_enabled: bool) -> Self {
        Self {
            editing_mode: if vim_enabled {
                EditorMode::Insert
            } else {
                EditorMode::Standard
            },
            cursor_byte_index: 0,
            pending_operator: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    pub fn focus(&mut self, query: &str) {
        if self.editing_mode != EditorMode::Standard {
            self.editing_mode = EditorMode::Insert;
        }
        self.cursor_byte_index = query.len();
        self.pending_operator = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
    }

    pub fn set_vim(&mut self, vim_enabled: bool, query: &str) {
        self.editing_mode = if vim_enabled {
            EditorMode::Insert
        } else {
            EditorMode::Standard
        };
        self.focus(query);
    }

    /// A UTF-8 byte offset, so a renderer can split the query without re-encoding it.
    pub fn cursor(&self) -> usize {
        self.cursor_byte_index
    }

    pub fn label(&self) -> &'static str {
        match self.editing_mode {
            EditorMode::Standard => "TYPE",
            EditorMode::Insert => "INSERT",
            EditorMode::Normal => "NORMAL",
        }
    }

    pub fn handle_key(&mut self, query: &mut String, key_event: KeyEvent) -> EditOutcome {
        self.repair_cursor(query);

        match key_event.code {
            KeyCode::Enter => return EditOutcome::Submit,
            KeyCode::Tab | KeyCode::Up | KeyCode::Down => return EditOutcome::Blur,
            KeyCode::Esc if matches!(self.editing_mode, EditorMode::Standard | EditorMode::Normal) => {
                return EditOutcome::Blur;
            }
            _ => {}
        }

        if self.editing_mode == EditorMode::Normal {
            self.handle_normal(query, key_event);
        } else {
            self.handle_insert(query, key_event);
        }
        EditOutcome::Consumed
    }

    fn handle_insert(&mut self, query: &mut String, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc if self.editing_mode == EditorMode::Insert => {
                self.editing_mode = EditorMode::Normal;
                self.cursor_byte_index = normal_cursor(query, self.cursor_byte_index);
                self.pending_operator = None;
            }
            KeyCode::Left => self.cursor_byte_index = previous_boundary(query, self.cursor_byte_index),
            KeyCode::Right => self.cursor_byte_index = next_boundary(query, self.cursor_byte_index),
            KeyCode::Home => self.cursor_byte_index = 0,
            KeyCode::End => self.cursor_byte_index = query.len(),
            KeyCode::Backspace => {
                let previous_character = previous_boundary(query, self.cursor_byte_index);
                self.replace_range(query, previous_character, self.cursor_byte_index, "");
                self.cursor_byte_index = previous_character;
            }
            KeyCode::Delete => {
                let next_character = next_boundary(query, self.cursor_byte_index);
                self.replace_range(query, self.cursor_byte_index, next_character, "");
            }
            KeyCode::Char(character) if text_modifiers(key_event.modifiers) => {
                let insertion_point = self.cursor_byte_index;
                let mut encoded_character = [0; 4];
                self.replace_range(
                    query,
                    insertion_point,
                    insertion_point,
                    character.encode_utf8(&mut encoded_character),
                );
                self.cursor_byte_index = insertion_point + character.len_utf8();
            }
            _ => {}
        }
    }

    fn handle_normal(&mut self, query: &mut String, key_event: KeyEvent) {
        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            if key_event.code == KeyCode::Char('r') {
                self.redo(query);
            }
            self.pending_operator = None;
            return;
        }
        if key_event.modifiers.contains(KeyModifiers::ALT) {
            self.pending_operator = None;
            return;
        }

        if let Some(operator) = self.pending_operator.take() {
            self.handle_operator(query, operator, key_event.code);
            return;
        }

        match key_event.code {
            KeyCode::Char('i') => self.editing_mode = EditorMode::Insert,
            KeyCode::Char('a') => {
                self.cursor_byte_index = next_boundary(query, self.cursor_byte_index);
                self.editing_mode = EditorMode::Insert;
            }
            KeyCode::Char('I') => {
                self.cursor_byte_index = 0;
                self.editing_mode = EditorMode::Insert;
            }
            KeyCode::Char('A') => {
                self.cursor_byte_index = query.len();
                self.editing_mode = EditorMode::Insert;
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.cursor_byte_index = previous_boundary(query, self.cursor_byte_index);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.cursor_byte_index = normal_next_boundary(query, self.cursor_byte_index);
            }
            KeyCode::Char('w') => self.cursor_byte_index = next_word_start(query, self.cursor_byte_index),
            KeyCode::Char('b') => self.cursor_byte_index = previous_word_start(query, self.cursor_byte_index),
            KeyCode::Char('e') => self.cursor_byte_index = word_end(query, self.cursor_byte_index),
            KeyCode::Char('0') | KeyCode::Home => self.cursor_byte_index = 0,
            KeyCode::Char('$') | KeyCode::End => self.cursor_byte_index = last_boundary(query),
            KeyCode::Char('x') | KeyCode::Delete => self.delete_under_cursor(query),
            KeyCode::Char('X') | KeyCode::Backspace => self.delete_before_cursor(query),
            KeyCode::Char('d') => self.pending_operator = Some(Operator::Delete),
            KeyCode::Char('c') => self.pending_operator = Some(Operator::Change),
            KeyCode::Char('D') => self.delete_to_end(query, false),
            KeyCode::Char('C') => self.delete_to_end(query, true),
            KeyCode::Char('u') => self.undo(query),
            _ => {}
        }
    }

    fn handle_operator(&mut self, query: &mut String, operator: Operator, key_code: KeyCode) {
        match (operator, key_code) {
            (Operator::Delete, KeyCode::Char('d')) => {
                self.replace_range(query, 0, query.len(), "");
                self.cursor_byte_index = 0;
            }
            (Operator::Delete, KeyCode::Char('w')) => {
                let deletion_end = delete_word_end(query, self.cursor_byte_index);
                self.replace_range(query, self.cursor_byte_index, deletion_end, "");
                self.cursor_byte_index = normal_cursor(query, self.cursor_byte_index);
            }
            (Operator::Change, KeyCode::Char('w')) => {
                let change_end = change_word_end(query, self.cursor_byte_index);
                self.replace_range(query, self.cursor_byte_index, change_end, "");
                self.editing_mode = EditorMode::Insert;
            }
            _ => {}
        }
    }

    fn delete_under_cursor(&mut self, query: &mut String) {
        let next_character = next_boundary(query, self.cursor_byte_index);
        self.replace_range(query, self.cursor_byte_index, next_character, "");
        self.cursor_byte_index = normal_cursor(query, self.cursor_byte_index);
    }

    fn delete_before_cursor(&mut self, query: &mut String) {
        let previous_character = previous_boundary(query, self.cursor_byte_index);
        self.replace_range(query, previous_character, self.cursor_byte_index, "");
        self.cursor_byte_index = normal_cursor(query, previous_character);
    }

    fn delete_to_end(&mut self, query: &mut String, enter_insert: bool) {
        self.replace_range(query, self.cursor_byte_index, query.len(), "");
        if enter_insert {
            self.editing_mode = EditorMode::Insert;
        } else {
            self.cursor_byte_index = normal_cursor(query, self.cursor_byte_index);
        }
    }

    fn replace_range(&mut self, query: &mut String, range_start: usize, range_end: usize, replacement: &str) {
        if range_start == range_end && replacement.is_empty() {
            return;
        }
        self.undo_stack.push((query.clone(), self.cursor_byte_index));
        self.redo_stack.clear();
        query.replace_range(range_start..range_end, replacement);
    }

    fn undo(&mut self, query: &mut String) {
        let Some(previous_snapshot) = self.undo_stack.pop() else {
            return;
        };
        self.redo_stack.push((query.clone(), self.cursor_byte_index));
        let (previous_query, previous_cursor_byte_index) = previous_snapshot;
        *query = previous_query;
        self.cursor_byte_index = normal_cursor(query, previous_cursor_byte_index);
    }

    fn redo(&mut self, query: &mut String) {
        let Some(next_snapshot) = self.redo_stack.pop() else {
            return;
        };
        self.undo_stack.push((query.clone(), self.cursor_byte_index));
        let (next_query, next_cursor_byte_index) = next_snapshot;
        *query = next_query;
        self.cursor_byte_index = normal_cursor(query, next_cursor_byte_index);
    }

    fn repair_cursor(&mut self, query: &str) {
        self.cursor_byte_index = self.cursor_byte_index.min(query.len());
        while !query.is_char_boundary(self.cursor_byte_index) {
            self.cursor_byte_index -= 1;
        }
        if self.editing_mode == EditorMode::Normal {
            self.cursor_byte_index = normal_cursor(query, self.cursor_byte_index);
        }
    }
}

fn text_modifiers(modifiers: KeyModifiers) -> bool {
    !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn previous_boundary(query: &str, cursor_byte_index: usize) -> usize {
    query[..cursor_byte_index]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_boundary(query: &str, cursor_byte_index: usize) -> usize {
    query[cursor_byte_index..]
        .chars()
        .next()
        .map_or(query.len(), |character| cursor_byte_index + character.len_utf8())
}

fn normal_next_boundary(query: &str, cursor_byte_index: usize) -> usize {
    let next_character = next_boundary(query, cursor_byte_index);
    if next_character == query.len() {
        cursor_byte_index
    } else {
        next_character
    }
}

fn last_boundary(query: &str) -> usize {
    query.char_indices().next_back().map_or(0, |(index, _)| index)
}

fn normal_cursor(query: &str, cursor_byte_index: usize) -> usize {
    if cursor_byte_index == query.len() && !query.is_empty() {
        previous_boundary(query, cursor_byte_index)
    } else {
        cursor_byte_index
    }
}

fn next_word_start(query: &str, cursor_byte_index: usize) -> usize {
    following_word_start(query, cursor_byte_index).unwrap_or_else(|| last_boundary(query))
}

fn following_word_start(query: &str, cursor_byte_index: usize) -> Option<usize> {
    let first_character = query[cursor_byte_index..].chars().next()?;
    let first_class = word_class(first_character);
    let mut left_first_class = false;
    for (relative_offset, character) in query[cursor_byte_index..].char_indices().skip(1) {
        let character_class = word_class(character);
        if !left_first_class && character_class != first_class {
            left_first_class = true;
        }
        if left_first_class && character_class != 0 {
            return Some(cursor_byte_index + relative_offset);
        }
    }
    None
}

fn previous_word_start(query: &str, cursor_byte_index: usize) -> usize {
    let mut preceding_characters = query[..cursor_byte_index].char_indices().rev();
    let Some((mut word_start, mut character)) = preceding_characters.next() else {
        return 0;
    };
    while word_class(character) == 0 {
        let Some((character_index, previous_character)) = preceding_characters.next() else {
            return 0;
        };
        word_start = character_index;
        character = previous_character;
    }
    let target_class = word_class(character);
    for (character_index, previous_character) in preceding_characters {
        if word_class(previous_character) != target_class {
            break;
        }
        word_start = character_index;
    }
    word_start
}

fn word_end(query: &str, cursor_byte_index: usize) -> usize {
    let mut following_characters = query[cursor_byte_index..].char_indices().peekable();
    let Some((_, character)) = following_characters.next() else {
        return cursor_byte_index;
    };

    let target_class = word_class(character);
    if target_class != 0 {
        let mut word_end = cursor_byte_index;
        while let Some(&(relative_offset, next_character)) = following_characters.peek() {
            if word_class(next_character) != target_class {
                break;
            }
            let _ = following_characters.next();
            word_end = cursor_byte_index + relative_offset;
        }
        if word_end != cursor_byte_index {
            return word_end;
        }
    }

    while let Some(&(_, next_character)) = following_characters.peek() {
        if word_class(next_character) != 0 {
            break;
        }
        let _ = following_characters.next();
    }
    let Some((relative_offset, next_character)) = following_characters.next() else {
        return cursor_byte_index;
    };
    let target_class = word_class(next_character);
    let mut word_end = cursor_byte_index + relative_offset;
    for (relative_offset, next_character) in following_characters {
        if word_class(next_character) != target_class {
            break;
        }
        word_end = cursor_byte_index + relative_offset;
    }
    word_end
}

fn delete_word_end(query: &str, cursor_byte_index: usize) -> usize {
    following_word_start(query, cursor_byte_index).unwrap_or(query.len())
}

fn change_word_end(query: &str, cursor_byte_index: usize) -> usize {
    let Some(first_character) = query[cursor_byte_index..].chars().next() else {
        return cursor_byte_index;
    };
    let target_class = word_class(first_character);
    if target_class == 0 {
        return following_word_start(query, cursor_byte_index).unwrap_or(query.len());
    }

    let mut change_end = cursor_byte_index + first_character.len_utf8();
    for (relative_offset, next_character) in query[cursor_byte_index..].char_indices().skip(1) {
        if word_class(next_character) != target_class {
            break;
        }
        change_end = cursor_byte_index + relative_offset + next_character.len_utf8();
    }
    change_end
}

fn word_class(character: char) -> u8 {
    if character.is_whitespace() {
        0
    } else if character.is_alphanumeric() || character == '_' {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(editor: &mut SearchEditor, query: &mut String, code: KeyCode) -> EditOutcome {
        editor.handle_key(query, key(code))
    }

    #[test]
    fn vim_focus_starts_in_insert_and_escape_enters_normal() {
        let mut editor = SearchEditor::new(true);
        let mut query = String::from("music");

        editor.focus(&query);
        assert_eq!(editor.label(), "INSERT");
        assert_eq!(editor.cursor(), query.len());

        press(&mut editor, &mut query, KeyCode::Esc);
        assert_eq!(editor.label(), "NORMAL");
        assert_eq!(editor.cursor(), 4);
        assert_eq!(press(&mut editor, &mut query, KeyCode::Esc), EditOutcome::Blur);
    }

    #[test]
    fn standard_mode_edits_utf8_at_character_boundaries() {
        let mut editor = SearchEditor::new(false);
        let mut query = String::from("aé界");
        editor.focus(&query);

        press(&mut editor, &mut query, KeyCode::Left);
        press(&mut editor, &mut query, KeyCode::Backspace);
        assert_eq!(query, "a界");
        assert_eq!(editor.cursor(), 1);

        press(&mut editor, &mut query, KeyCode::Delete);
        assert_eq!(query, "a");
        assert_eq!(editor.cursor(), 1);
    }

    #[test]
    fn vim_word_commands_change_and_delete_ranges() {
        let mut editor = SearchEditor::new(true);
        let mut query = String::from("one two three");
        editor.focus(&query);
        press(&mut editor, &mut query, KeyCode::Esc);
        press(&mut editor, &mut query, KeyCode::Char('0'));

        press(&mut editor, &mut query, KeyCode::Char('d'));
        press(&mut editor, &mut query, KeyCode::Char('w'));
        assert_eq!(query, "two three");

        press(&mut editor, &mut query, KeyCode::Char('c'));
        press(&mut editor, &mut query, KeyCode::Char('w'));
        assert_eq!(query, " three");
        assert_eq!(editor.label(), "INSERT");
    }

    #[test]
    fn change_word_at_boundaries_stays_with_the_current_word() {
        let mut editor = SearchEditor::new(true);
        let mut query = String::from("one two");
        editor.focus(&query);
        press(&mut editor, &mut query, KeyCode::Esc);
        press(&mut editor, &mut query, KeyCode::Char('0'));
        press(&mut editor, &mut query, KeyCode::Char('l'));
        press(&mut editor, &mut query, KeyCode::Char('l'));

        press(&mut editor, &mut query, KeyCode::Char('c'));
        press(&mut editor, &mut query, KeyCode::Char('w'));
        assert_eq!(query, "on two");
        assert_eq!(editor.label(), "INSERT");

        let mut editor = SearchEditor::new(true);
        let mut query = String::from("one  two");
        editor.focus(&query);
        press(&mut editor, &mut query, KeyCode::Esc);
        press(&mut editor, &mut query, KeyCode::Char('0'));
        for _ in 0..3 {
            press(&mut editor, &mut query, KeyCode::Char('l'));
        }

        press(&mut editor, &mut query, KeyCode::Char('c'));
        press(&mut editor, &mut query, KeyCode::Char('w'));
        assert_eq!(query, "onetwo");
        assert_eq!(editor.label(), "INSERT");
    }

    #[test]
    fn vim_word_moves_to_last_character_when_no_word_follows() {
        let mut editor = SearchEditor::new(true);
        let mut query = String::from("one τέλος");
        editor.focus(&query);
        press(&mut editor, &mut query, KeyCode::Esc);
        press(&mut editor, &mut query, KeyCode::Char('0'));

        press(&mut editor, &mut query, KeyCode::Char('w'));
        assert_eq!(editor.cursor(), 4);
        press(&mut editor, &mut query, KeyCode::Char('w'));
        assert_eq!(editor.cursor(), 12);
        press(&mut editor, &mut query, KeyCode::Char('w'));
        assert_eq!(editor.cursor(), 12);
    }

    #[test]
    fn repeated_end_motion_crosses_punctuation_and_whitespace() {
        let mut editor = SearchEditor::new(true);
        let mut query = String::from("cat,  élan");
        editor.focus(&query);
        press(&mut editor, &mut query, KeyCode::Esc);
        press(&mut editor, &mut query, KeyCode::Char('0'));

        press(&mut editor, &mut query, KeyCode::Char('e'));
        assert_eq!(editor.cursor(), 2);
        press(&mut editor, &mut query, KeyCode::Char('e'));
        assert_eq!(editor.cursor(), 3);
        press(&mut editor, &mut query, KeyCode::Char('e'));
        assert_eq!(editor.cursor(), 10);
        press(&mut editor, &mut query, KeyCode::Char('e'));
        assert_eq!(editor.cursor(), 10);
    }

    #[test]
    fn vim_delete_line_undo_and_redo_restore_text() {
        let mut editor = SearchEditor::new(true);
        let mut query = String::from("red wax");
        editor.focus(&query);
        press(&mut editor, &mut query, KeyCode::Esc);

        press(&mut editor, &mut query, KeyCode::Char('d'));
        press(&mut editor, &mut query, KeyCode::Char('d'));
        assert!(query.is_empty());

        press(&mut editor, &mut query, KeyCode::Char('u'));
        assert_eq!(query, "red wax");

        editor.handle_key(
            &mut query,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        );
        assert!(query.is_empty());
    }

    #[test]
    fn modified_characters_do_not_enter_the_query() {
        let mut editor = SearchEditor::new(false);
        let mut query = String::new();

        editor.handle_key(
            &mut query,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        editor.handle_key(&mut query, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));

        assert!(query.is_empty());
    }
}
