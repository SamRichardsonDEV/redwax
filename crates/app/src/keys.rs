//! Every key redwax answers to, in one table. The Python `app.py` bindings and
//! the `widgets.py` list bindings are the specification, including the order:
//! the focused list is asked first, so `j` moves the cursor rather than doing
//! nothing, and `l` falls through to the tab switch the list has no binding for.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::Tab;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    TogglePause,
    /// Resume the current playback item, never the list cursor.
    Play,
    NextTrack,
    PreviousTrack,
    Seek(i32),
    Volume(i16),
    AddToQueue,
    PlayNext,
    Radio,
    /// Like or unlike the track that plays, never the list cursor.
    Like,
    /// Like or unlike the highlighted track, whatever plays.
    LikeHighlighted,
    ToggleShuffle,
    CycleRepeat,
    ToggleMute,
    /// Type a volume in rather than nudging it five at a time.
    SetVolume,
    RemoveFromQueue,
    ClearQueue,
    MoveQueueUp,
    MoveQueueDown,
    /// The queue tab, with the cursor put on the track that plays.
    JumpToQueue,
    /// Take the highlighted row out of the playlist whose page is open, or
    /// open the picker that takes the playing track out of a playlist.
    RemoveFromPlaylist,
    /// Open the picker that adds the playing track to a playlist.
    AddToPlaylist,
    AddHighlightedToPlaylist,
    DeleteHighlighted,
    /// The head of the previous or next section of the open page.
    PreviousSection,
    NextSection,
    PreviousTab,
    NextTab,
    SwitchTab(Tab),
    /// Find on the open page, which is not the same as searching YouTube.
    FindOnPage,
    Back,
    Reload,
    ToggleHelp,
    ShowNotification,
    ToggleEq,
    OpenActions,
    OpenAlbum,
    CycleFilter,
    RetryPlayback,
    Logout,
    Quit,
    CursorDown,
    CursorUp,
    First,
    Last,
    PageDown,
    PageUp,
    Open,
}

/// One row of the help panel. `show` is false for the aliases Python hides.
pub struct Binding {
    pub key: &'static str,
    pub description: &'static str,
    pub action: Action,
    pub show: bool,
    pub scope: Scope,
    input: Option<Input>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    List,
}

#[derive(Debug, Clone, Copy)]
enum Input {
    Char(char),
    Control(char),
    Code(KeyCode),
}

impl Input {
    fn matches(self, key_event: KeyEvent) -> bool {
        let control_pressed = key_event.modifiers.contains(KeyModifiers::CONTROL);
        match self {
            Input::Char(character) => !control_pressed && key_event.code == KeyCode::Char(character),
            Input::Control(character) => control_pressed && key_event.code == KeyCode::Char(character),
            Input::Code(key_code) => !control_pressed && key_event.code == key_code,
        }
    }
}

pub const BINDINGS: &[Binding] = &[
    bound(Input::Char(' '), "space", "Play/Pause", Action::TogglePause),
    bound(Input::Char('x'), "x", "Resume current playback", Action::Play),
    bound(Input::Char('n'), "n", "Next track", Action::NextTrack),
    bound(Input::Char('p'), "p", "Previous track", Action::PreviousTrack),
    bound(Input::Char(','), ",", "Seek back 5s", Action::Seek(-5)),
    bound(Input::Char('.'), ".", "Seek forward 5s", Action::Seek(5)),
    bound(Input::Char('<'), "<", "Seek back 30s", Action::Seek(-30)),
    bound(Input::Char('>'), ">", "Seek forward 30s", Action::Seek(30)),
    bound(Input::Char('-'), "-", "Volume down", Action::Volume(-5)),
    bound(Input::Char('='), "=", "Volume up", Action::Volume(5)),
    bound(Input::Char('a'), "a", "Add to queue", Action::AddToQueue),
    bound(Input::Char('A'), "A", "Play next", Action::PlayNext),
    bound(Input::Char('r'), "r", "Radio from track", Action::Radio),
    bound(
        Input::Char('L'),
        "L",
        "Like / unlike what is playing",
        Action::Like,
    ),
    bound(
        Input::Char('+'),
        "+",
        "Like / unlike the highlighted row",
        Action::LikeHighlighted,
    ),
    bound(Input::Char('s'), "s", "Shuffle on/off", Action::ToggleShuffle),
    bound(Input::Char('R'), "R", "Repeat off/all/one", Action::CycleRepeat),
    bound(Input::Char('m'), "m", "Mute", Action::ToggleMute),
    bound(
        Input::Char('!'),
        "!",
        "Read latest message",
        Action::ShowNotification,
    ),
    bound(Input::Char('v'), "v", "Type a volume", Action::SetVolume),
    bound(
        Input::Char('d'),
        "d",
        "Remove from queue",
        Action::RemoveFromQueue,
    ),
    bound(Input::Char('c'), "c", "Clear queue", Action::ClearQueue),
    list_bound(Input::Char('K'), "K", "Move queue entry up", Action::MoveQueueUp),
    list_bound(
        Input::Char('J'),
        "J",
        "Move queue entry down",
        Action::MoveQueueDown,
    ),
    bound(
        Input::Char('q'),
        "q",
        "Queue tab, on the playing track",
        Action::JumpToQueue,
    ),
    bound(
        Input::Char('D'),
        "D",
        "Remove from a playlist",
        Action::RemoveFromPlaylist,
    ),
    bound(
        Input::Char('P'),
        "P",
        "Add playing song to a playlist",
        Action::AddToPlaylist,
    ),
    bound(
        Input::Control('p'),
        "ctrl+p",
        "Add highlighted item to a playlist",
        Action::AddHighlightedToPlaylist,
    ),
    bound(
        Input::Code(KeyCode::Delete),
        "delete",
        "Remove highlighted song from this list",
        Action::DeleteHighlighted,
    ),
    bound(Input::Char('h'), "h", "Previous tab", Action::PreviousTab),
    bound(Input::Char('l'), "l", "Next tab", Action::NextTab),
    hidden(
        Input::Code(KeyCode::Left),
        "left",
        "Previous tab",
        Action::PreviousTab,
    ),
    hidden(Input::Code(KeyCode::Right), "right", "Next tab", Action::NextTab),
    bound(
        Input::Char('/'),
        "/",
        "Find on this page, or the search box",
        Action::FindOnPage,
    ),
    list_bound(
        Input::Code(KeyCode::Enter),
        "enter",
        "Play, or enter the search box",
        Action::Open,
    ),
    bound(Input::Code(KeyCode::Backspace), "backspace", "Back", Action::Back),
    bound(
        Input::Code(KeyCode::Esc),
        "escape",
        "Back, or leave the box",
        Action::Back,
    ),
    bound(Input::Control('r'), "ctrl+r", "Reload tab", Action::Reload),
    bound(Input::Char('1'), "1", "Home", Action::SwitchTab(Tab::Home)),
    bound(Input::Char('2'), "2", "Search", Action::SwitchTab(Tab::Search)),
    bound(Input::Char('3'), "3", "Library", Action::SwitchTab(Tab::Library)),
    bound(Input::Char('4'), "4", "Queue", Action::SwitchTab(Tab::Queue)),
    bound(Input::Char('5'), "5", "Lyrics", Action::SwitchTab(Tab::Lyrics)),
    bound(Input::Char('6'), "6", "Themes", Action::SwitchTab(Tab::Themes)),
    bound(
        Input::Char('7'),
        "7",
        "Settings",
        Action::SwitchTab(Tab::Settings),
    ),
    list_bound(Input::Char('j'), "j", "Move down", Action::CursorDown),
    list_hidden(
        Input::Code(KeyCode::Down),
        "down",
        "Move down",
        Action::CursorDown,
    ),
    list_bound(Input::Char('k'), "k", "Move up", Action::CursorUp),
    list_hidden(Input::Code(KeyCode::Up), "up", "Move up", Action::CursorUp),
    list_action_only("g g", "Top of this section", Action::First),
    list_hidden(
        Input::Code(KeyCode::Home),
        "home",
        "Top of this section",
        Action::First,
    ),
    list_bound(Input::Char('G'), "G", "Bottom of this section", Action::Last),
    list_hidden(
        Input::Code(KeyCode::End),
        "end",
        "Bottom of this section",
        Action::Last,
    ),
    list_hidden(Input::Control('d'), "ctrl+d", "Page down", Action::PageDown),
    list_hidden(
        Input::Code(KeyCode::PageDown),
        "page down",
        "Page down",
        Action::PageDown,
    ),
    list_hidden(Input::Control('u'), "ctrl+u", "Page up", Action::PageUp),
    list_hidden(Input::Code(KeyCode::PageUp), "page up", "Page up", Action::PageUp),
    bound(Input::Char('{'), "{", "Previous section", Action::PreviousSection),
    bound(Input::Char('}'), "}", "Next section", Action::NextSection),
    bound(Input::Char('e'), "e", "Equaliser", Action::ToggleEq),
    bound(
        Input::Char('o'),
        "o",
        "Actions for the highlighted item",
        Action::OpenActions,
    ),
    hidden_action("Open the highlighted track album", Action::OpenAlbum),
    bound(Input::Char('f'), "f", "Filter result kind", Action::CycleFilter),
    listed(
        "o menu",
        "Retry playback after a failed restart",
        Action::RetryPlayback,
    ),
    listed("o menu", "Sign out of YouTube Music", Action::Logout),
    bound(Input::Char('?'), "?", "Keys", Action::ToggleHelp),
    bound(Input::Char('Q'), "Q", "Quit", Action::Quit),
];

const fn bound(
    input: Input,
    display_key: &'static str,
    description: &'static str,
    action: Action,
) -> Binding {
    Binding {
        key: display_key,
        description,
        action,
        show: true,
        scope: Scope::Global,
        input: Some(input),
    }
}

const fn listed(display_key: &'static str, description: &'static str, action: Action) -> Binding {
    Binding {
        key: display_key,
        description,
        action,
        show: true,
        scope: Scope::Global,
        input: None,
    }
}

const fn list_bound(
    input: Input,
    display_key: &'static str,
    description: &'static str,
    action: Action,
) -> Binding {
    Binding {
        key: display_key,
        description,
        action,
        show: true,
        scope: Scope::List,
        input: Some(input),
    }
}

const fn list_hidden(
    input: Input,
    display_key: &'static str,
    description: &'static str,
    action: Action,
) -> Binding {
    Binding {
        key: display_key,
        description,
        action,
        show: false,
        scope: Scope::List,
        input: Some(input),
    }
}

const fn list_action_only(display_key: &'static str, description: &'static str, action: Action) -> Binding {
    Binding {
        key: display_key,
        description,
        action,
        show: true,
        scope: Scope::List,
        input: None,
    }
}

const fn hidden(
    input: Input,
    display_key: &'static str,
    description: &'static str,
    action: Action,
) -> Binding {
    Binding {
        key: display_key,
        description,
        action,
        show: false,
        scope: Scope::Global,
        input: Some(input),
    }
}

const fn hidden_action(description: &'static str, action: Action) -> Binding {
    Binding {
        key: "",
        description,
        action,
        show: false,
        scope: Scope::Global,
        input: None,
    }
}

/// The bindings of the focused list, asked before the application table.
pub fn list_action(key_event: KeyEvent) -> Option<Action> {
    action_in(Scope::List, key_event)
}

pub fn action(key_event: KeyEvent) -> Option<Action> {
    action_in(Scope::Global, key_event)
}

fn action_in(scope: Scope, key_event: KeyEvent) -> Option<Action> {
    BINDINGS
        .iter()
        .find(|binding| {
            binding.scope == scope
                && binding
                    .input
                    .is_some_and(|configured_input| configured_input.matches(key_event))
        })
        .map(|binding| binding.action)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_event(key_code: KeyCode) -> KeyEvent {
        KeyEvent::new(key_code, KeyModifiers::NONE)
    }

    #[test]
    fn list_and_global_decoders_use_the_catalogued_scope() {
        assert_eq!(
            list_action(key_event(KeyCode::Char('j'))),
            Some(Action::CursorDown)
        );
        assert_eq!(action(key_event(KeyCode::Char('j'))), None);
        assert_eq!(action(key_event(KeyCode::Char('n'))), Some(Action::NextTrack));
        assert_eq!(list_action(key_event(KeyCode::Char('n'))), None);
        assert_eq!(list_action(key_event(KeyCode::Char('G'))), Some(Action::Last));
        assert_eq!(action(key_event(KeyCode::Char('G'))), None);
    }

    #[test]
    fn every_decodable_binding_has_one_explicit_scope() {
        for binding in BINDINGS.iter().filter(|binding| binding.input.is_some()) {
            let configured_input = binding.input.expect("filtered input");
            let decoded_event = match configured_input {
                Input::Char(character) => key_event(KeyCode::Char(character)),
                Input::Control(character) => KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL),
                Input::Code(key_code) => key_event(key_code),
            };

            assert_eq!(action_in(binding.scope, decoded_event), Some(binding.action));
        }
    }
}
