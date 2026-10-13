//! Presentation and execution policy for the actions decoded in `keys`.

use crate::keys::{Action, BINDINGS, Binding, Scope};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Playback,
    Selection,
    Queue,
    Navigation,
    View,
    Application,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Playback => "PLAYBACK",
            Group::Selection => "SELECTION",
            Group::Queue => "QUEUE",
            Group::Navigation => "NAVIGATION",
            Group::View => "VIEWS",
            Group::Application => "APPLICATION",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timing {
    Immediate,
    Deferred,
    Confirmation,
    Prefix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectRule {
    None,
    Playing,
    Selected,
    SelectedOrPlaying,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Global,
    List,
    Selection,
    Playback,
    Queue,
}

pub struct Command {
    pub binding: &'static Binding,
    pub group: Group,
    pub timing: Timing,
    pub subject: SubjectRule,
    pub context: Context,
}

impl Command {
    pub fn search_text(&self) -> String {
        format!(
            "{} {} {:?}",
            self.binding.key, self.binding.description, self.group
        )
        .to_ascii_lowercase()
    }
}

pub fn catalogue() -> impl Iterator<Item = Command> {
    BINDINGS.iter().map(|binding| Command {
        binding,
        group: group(binding.action),
        timing: timing(binding.action),
        subject: subject(binding.action),
        context: context(binding.action),
    })
}

pub fn context(action: Action) -> Context {
    if matches!(action, Action::MoveQueueUp | Action::MoveQueueDown) {
        return Context::Queue;
    }
    match subject(action) {
        SubjectRule::Playing => Context::Playback,
        SubjectRule::Selected | SubjectRule::SelectedOrPlaying => Context::Selection,
        SubjectRule::None => match BINDINGS
            .iter()
            .find(|binding| binding.action == action)
            .map(|binding| binding.scope)
        {
            Some(Scope::List) => Context::List,
            Some(Scope::Global) | None => Context::Global,
        },
    }
}

/// Stable timing phase for user actions whose latency is recorded.
pub fn input_phase(action: Action) -> Option<&'static str> {
    match action {
        Action::Open => Some("input.enter"),
        Action::TogglePause => Some("input.space"),
        Action::NextTrack => Some("input.next"),
        Action::PreviousTrack => Some("input.previous"),
        Action::Seek(-5) => Some("input.seek_back_5"),
        Action::Seek(5) => Some("input.seek_forward_5"),
        Action::Seek(-30) => Some("input.seek_back_30"),
        Action::Seek(30) => Some("input.seek_forward_30"),
        Action::Volume(volume_delta) if volume_delta < 0 => Some("input.volume_down"),
        Action::Volume(volume_delta) if volume_delta > 0 => Some("input.volume_up"),
        Action::Radio => Some("input.radio"),
        Action::CycleRepeat => Some("input.repeat"),
        Action::ToggleShuffle => Some("input.shuffle"),
        Action::ToggleMute => Some("input.mute"),
        Action::SetVolume => Some("input.set_volume"),
        Action::ClearQueue => Some("input.clear_queue"),
        Action::RemoveFromQueue => Some("input.remove_queue"),
        Action::MoveQueueUp => Some("input.move_queue_up"),
        Action::MoveQueueDown => Some("input.move_queue_down"),
        Action::LikeHighlighted => Some("input.like_highlighted"),
        Action::Logout => Some("input.logout"),
        _ => None,
    }
}

pub fn group(action: Action) -> Group {
    match action {
        Action::TogglePause
        | Action::NextTrack
        | Action::PreviousTrack
        | Action::Seek(_)
        | Action::Volume(_)
        | Action::ToggleShuffle
        | Action::CycleRepeat
        | Action::ToggleMute
        | Action::SetVolume => Group::Playback,
        Action::RemoveFromQueue
        | Action::ClearQueue
        | Action::MoveQueueUp
        | Action::MoveQueueDown
        | Action::JumpToQueue => Group::Queue,
        Action::Radio
        | Action::LikeHighlighted
        | Action::RemoveFromPlaylist
        | Action::AddToPlaylist
        | Action::AddHighlightedToPlaylist
        | Action::DeleteHighlighted
        | Action::Open
        | Action::OpenAlbum => Group::Selection,
        Action::PreviousSection
        | Action::NextSection
        | Action::PreviousTab
        | Action::NextTab
        | Action::FindOnPage
        | Action::FocusAccount
        | Action::Back
        | Action::CursorDown
        | Action::CursorUp
        | Action::First
        | Action::Last
        | Action::PageDown
        | Action::PageUp => Group::Navigation,
        Action::SwitchTab(_) | Action::Reload | Action::ToggleEq | Action::CycleFilter => Group::View,
        Action::ToggleHelp
        | Action::ShowNotification
        | Action::OpenAccount
        | Action::Logout
        | Action::Quit => Group::Application,
    }
}

pub fn timing(action: Action) -> Timing {
    match action {
        Action::ClearQueue | Action::Logout => Timing::Confirmation,
        Action::First => Timing::Prefix,
        Action::Radio
        | Action::LikeHighlighted
        | Action::RemoveFromPlaylist
        | Action::AddToPlaylist
        | Action::AddHighlightedToPlaylist
        | Action::Reload
        | Action::Open
        | Action::OpenAlbum
        | Action::RemoveFromQueue
        | Action::DeleteHighlighted
        | Action::MoveQueueUp
        | Action::MoveQueueDown => Timing::Deferred,
        _ => Timing::Immediate,
    }
}

pub fn subject(action: Action) -> SubjectRule {
    match action {
        Action::TogglePause
        | Action::NextTrack
        | Action::PreviousTrack
        | Action::Seek(_)
        | Action::JumpToQueue
        | Action::AddToPlaylist => SubjectRule::Playing,
        Action::Radio
        | Action::LikeHighlighted
        | Action::AddHighlightedToPlaylist
        | Action::DeleteHighlighted
        | Action::RemoveFromQueue
        | Action::MoveQueueUp
        | Action::MoveQueueDown
        | Action::Open
        | Action::OpenAlbum => SubjectRule::Selected,
        Action::RemoveFromPlaylist => SubjectRule::SelectedOrPlaying,
        _ => SubjectRule::None,
    }
}
