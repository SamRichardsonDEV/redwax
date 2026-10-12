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

pub fn command(action: Action) -> Option<Command> {
    catalogue().find(|command| command.binding.action == action)
}

/// Stable timing phase for user actions whose latency is recorded.
pub fn input_phase(action: Action) -> Option<&'static str> {
    match action {
        Action::Open => Some("input.enter"),
        Action::TogglePause => Some("input.space"),
        Action::Play => Some("input.play"),
        Action::NextTrack => Some("input.next"),
        Action::PreviousTrack => Some("input.previous"),
        Action::Seek(-5) => Some("input.seek_back_5"),
        Action::Seek(5) => Some("input.seek_forward_5"),
        Action::Seek(-30) => Some("input.seek_back_30"),
        Action::Seek(30) => Some("input.seek_forward_30"),
        Action::Volume(volume_delta) if volume_delta < 0 => Some("input.volume_down"),
        Action::Volume(volume_delta) if volume_delta > 0 => Some("input.volume_up"),
        Action::AddToQueue => Some("input.enqueue"),
        Action::PlayNext => Some("input.play_next"),
        Action::Radio => Some("input.radio"),
        Action::CycleRepeat => Some("input.repeat"),
        Action::ToggleShuffle => Some("input.shuffle"),
        Action::ToggleMute => Some("input.mute"),
        Action::SetVolume => Some("input.set_volume"),
        Action::ClearQueue => Some("input.clear_queue"),
        Action::RemoveFromQueue => Some("input.remove_queue"),
        Action::MoveQueueUp => Some("input.move_queue_up"),
        Action::MoveQueueDown => Some("input.move_queue_down"),
        Action::Like => Some("input.like_playing"),
        Action::LikeHighlighted => Some("input.like_highlighted"),
        Action::RetryPlayback => Some("input.retry_playback"),
        Action::Logout => Some("input.logout"),
        _ => None,
    }
}

pub fn group(action: Action) -> Group {
    match action {
        Action::TogglePause
        | Action::Play
        | Action::NextTrack
        | Action::PreviousTrack
        | Action::Seek(_)
        | Action::Volume(_)
        | Action::Like
        | Action::ToggleShuffle
        | Action::CycleRepeat
        | Action::ToggleMute
        | Action::SetVolume
        | Action::RetryPlayback => Group::Playback,
        Action::AddToQueue
        | Action::PlayNext
        | Action::RemoveFromQueue
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
        | Action::OpenActions
        | Action::OpenAlbum => Group::Selection,
        Action::PreviousSection
        | Action::NextSection
        | Action::PreviousTab
        | Action::NextTab
        | Action::FindOnPage
        | Action::Back
        | Action::CursorDown
        | Action::CursorUp
        | Action::First
        | Action::Last
        | Action::PageDown
        | Action::PageUp => Group::Navigation,
        Action::SwitchTab(_) | Action::Reload | Action::ToggleEq | Action::CycleFilter => Group::View,
        Action::ToggleHelp | Action::ShowNotification | Action::Logout | Action::Quit => Group::Application,
    }
}

pub fn timing(action: Action) -> Timing {
    match action {
        Action::ClearQueue | Action::Logout => Timing::Confirmation,
        Action::First => Timing::Prefix,
        Action::Play
        | Action::AddToQueue
        | Action::PlayNext
        | Action::Radio
        | Action::Like
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
        | Action::MoveQueueDown
        | Action::RetryPlayback => Timing::Deferred,
        _ => Timing::Immediate,
    }
}

pub fn subject(action: Action) -> SubjectRule {
    match action {
        Action::TogglePause
        | Action::Play
        | Action::NextTrack
        | Action::PreviousTrack
        | Action::Seek(_)
        | Action::Like
        | Action::JumpToQueue
        | Action::AddToPlaylist => SubjectRule::Playing,
        Action::AddToQueue
        | Action::PlayNext
        | Action::Radio
        | Action::LikeHighlighted
        | Action::AddHighlightedToPlaylist
        | Action::DeleteHighlighted
        | Action::RemoveFromQueue
        | Action::MoveQueueUp
        | Action::MoveQueueDown
        | Action::Open
        | Action::OpenAlbum => SubjectRule::Selected,
        Action::RemoveFromPlaylist | Action::OpenActions => SubjectRule::SelectedOrPlaying,
        _ => SubjectRule::None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectKind {
    Track,
    Album,
    Playlist,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionSubject {
    /// Stable track or browse identity. Titles can collide.
    pub identity: String,
    pub kind: SubjectKind,
    pub title: String,
    pub has_album: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionTarget {
    Selected,
    Playing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub action: Action,
    pub label: String,
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionInvocation {
    pub action: Action,
    pub target: Option<ActionTarget>,
}

pub struct ActionMenuContext {
    pub selected: Option<ActionSubject>,
    pub playing: Option<ActionSubject>,
    pub selected_is_queue_entry: bool,
    pub selected_playlist: Option<String>,
    pub playlist_editable: bool,
    pub signed_in: bool,
    pub playback_retry_available: bool,
}

pub struct ActionsMenu {
    action_subjects: Vec<(ActionTarget, ActionSubject)>,
    subject_index: usize,
    menu_entries: Vec<MenuEntry>,
    selected_entry_index: usize,
    menu_context: ActionMenuContext,
}

impl ActionsMenu {
    pub fn open(menu_context: ActionMenuContext) -> Option<ActionsMenu> {
        let mut action_subjects = Vec::new();
        if let Some(selected_subject) = menu_context.selected.clone() {
            action_subjects.push((ActionTarget::Selected, selected_subject));
        }
        if let Some(playing_subject) = menu_context.playing.clone()
            && action_subjects
                .iter()
                .all(|(_, subject)| subject.identity != playing_subject.identity)
        {
            action_subjects.push((ActionTarget::Playing, playing_subject));
        }
        let mut actions_menu = ActionsMenu {
            action_subjects,
            subject_index: 0,
            menu_entries: Vec::new(),
            selected_entry_index: 0,
            menu_context,
        };
        actions_menu.rebuild();
        Some(actions_menu)
    }

    pub fn subject(&self) -> Option<&(ActionTarget, ActionSubject)> {
        self.action_subjects.get(self.subject_index)
    }

    pub fn subject_count(&self) -> usize {
        self.action_subjects.len()
    }

    pub fn entries(&self) -> &[MenuEntry] {
        &self.menu_entries
    }

    pub fn cursor(&self) -> usize {
        self.selected_entry_index
    }

    pub fn move_cursor(&mut self, row_delta: isize) {
        self.selected_entry_index = self
            .selected_entry_index
            .saturating_add_signed(row_delta)
            .min(self.menu_entries.len().saturating_sub(1));
    }

    pub fn choose_subject(&mut self, subject_delta: isize) {
        self.subject_index = self
            .subject_index
            .saturating_add_signed(subject_delta)
            .min(self.action_subjects.len().saturating_sub(1));
        self.selected_entry_index = 0;
        self.rebuild();
    }

    pub fn selected(&self) -> Result<ActionInvocation, &str> {
        let selected_entry = &self.menu_entries[self.selected_entry_index];
        match selected_entry.disabled_reason.as_deref() {
            Some(disabled_reason) => Err(disabled_reason),
            None => Ok(ActionInvocation {
                action: selected_entry.action,
                target: match subject(selected_entry.action) {
                    SubjectRule::None => None,
                    _ => self.subject().map(|(action_target, _)| *action_target),
                },
            }),
        }
    }

    fn rebuild(&mut self) {
        let mut menu_entries = Vec::new();
        if let Some((action_target, action_subject)) = self.subject() {
            let targets_selected_item = *action_target == ActionTarget::Selected;
            let subject_is_track = action_subject.kind == SubjectKind::Track;
            let open_label = match action_subject.kind {
                SubjectKind::Track => "Play",
                SubjectKind::Album | SubjectKind::Playlist => "Open",
            };
            menu_entries.extend([
                menu_entry(
                    Action::Open,
                    open_label,
                    (!targets_selected_item).then_some("Choose the selected item"),
                ),
                menu_entry(Action::AddToQueue, "Add to queue", None),
                menu_entry(Action::PlayNext, "Play next", None),
                menu_entry(
                    Action::Radio,
                    "Start radio",
                    (!subject_is_track).then_some("Radio needs a track"),
                ),
                menu_entry(
                    Action::LikeHighlighted,
                    "Like or unlike",
                    (!subject_is_track)
                        .then_some("Likes need a track")
                        .or_else(|| (!self.menu_context.signed_in).then_some("Sign in to like tracks")),
                ),
                menu_entry(
                    Action::AddHighlightedToPlaylist,
                    "Add to playlist",
                    (!self.menu_context.signed_in).then_some("Sign in to edit playlists"),
                ),
            ]);
            if targets_selected_item && self.menu_context.selected_is_queue_entry {
                menu_entries.push(menu_entry(Action::MoveQueueUp, "Move up", None));
                menu_entries.push(menu_entry(Action::MoveQueueDown, "Move down", None));
                menu_entries.push(menu_entry(Action::RemoveFromQueue, "Remove from queue", None));
            }
            if targets_selected_item && self.menu_context.selected_playlist.is_some() {
                menu_entries.push(menu_entry(
                    Action::RemoveFromPlaylist,
                    "Remove from this playlist",
                    (!self.menu_context.playlist_editable).then_some("This playlist is not editable"),
                ));
            }
            if action_subject.has_album {
                menu_entries.push(menu_entry(Action::OpenAlbum, "Open album", None));
            }
        }
        menu_entries.push(menu_entry(
            Action::RetryPlayback,
            "Retry playback",
            (!self.menu_context.playback_retry_available).then_some("Playback has not failed"),
        ));
        menu_entries.push(menu_entry(
            Action::Logout,
            "Sign out of YouTube Music",
            (!self.menu_context.signed_in).then_some("Already signed out"),
        ));
        self.menu_entries = menu_entries;
    }
}

fn menu_entry(action: Action, label: &str, disabled_reason: Option<&str>) -> MenuEntry {
    MenuEntry {
        action,
        label: label.to_string(),
        disabled_reason: disabled_reason.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_menu_names_disabled_capabilities_and_offers_playing_subject() {
        let mut menu = ActionsMenu::open(ActionMenuContext {
            selected: Some(ActionSubject {
                identity: "album-1".into(),
                kind: SubjectKind::Album,
                title: "Selected album".into(),
                has_album: false,
            }),
            playing: Some(ActionSubject {
                identity: "video-1".into(),
                kind: SubjectKind::Track,
                title: "Playing track".into(),
                has_album: true,
            }),
            selected_is_queue_entry: false,
            selected_playlist: Some("Shared playlist".into()),
            playlist_editable: false,
            signed_in: false,
            playback_retry_available: true,
        })
        .unwrap();

        assert!(
            menu.entries()
                .iter()
                .any(|entry| entry.disabled_reason.as_deref() == Some("Radio needs a track"))
        );
        assert!(
            menu.entries()
                .iter()
                .any(|entry| { entry.disabled_reason.as_deref() == Some("This playlist is not editable") })
        );
        assert!(
            menu.entries()
                .iter()
                .any(|entry| { entry.action == Action::RetryPlayback && entry.disabled_reason.is_none() })
        );
        assert!(menu.entries().iter().any(|entry| {
            entry.action == Action::Logout && entry.disabled_reason.as_deref() == Some("Already signed out")
        }));
        menu.choose_subject(1);
        assert_eq!(menu.subject().unwrap().1.title, "Playing track");
        assert!(
            menu.entries()
                .iter()
                .any(|entry| entry.action == Action::OpenAlbum)
        );
        menu.selected_entry_index = menu
            .entries()
            .iter()
            .position(|entry| entry.action == Action::RetryPlayback)
            .unwrap();
        assert_eq!(menu.selected().unwrap().target, None);
    }

    #[test]
    fn queue_subject_offers_identity_preserving_move_actions() {
        let menu = ActionsMenu::open(ActionMenuContext {
            selected: Some(ActionSubject {
                identity: "queue-entry-7".into(),
                kind: SubjectKind::Track,
                title: "Duplicate".into(),
                has_album: false,
            }),
            playing: None,
            selected_is_queue_entry: true,
            selected_playlist: None,
            playlist_editable: false,
            signed_in: true,
            playback_retry_available: false,
        })
        .unwrap();

        for action in [
            Action::MoveQueueUp,
            Action::MoveQueueDown,
            Action::RemoveFromQueue,
        ] {
            assert!(menu.entries().iter().any(|entry| entry.action == action));
        }
    }

    #[test]
    fn selected_subject_opens_while_playing_subject_cannot_redirect_open() {
        assert_eq!(group(Action::Play), Group::Playback);
        assert_eq!(subject(Action::Play), SubjectRule::Playing);

        let mut track_menu = ActionsMenu::open(ActionMenuContext {
            selected: Some(ActionSubject {
                identity: "selected-track".into(),
                kind: SubjectKind::Track,
                title: "Selected track".into(),
                has_album: true,
            }),
            playing: Some(ActionSubject {
                identity: "playing-track".into(),
                kind: SubjectKind::Track,
                title: "Playing track".into(),
                has_album: true,
            }),
            selected_is_queue_entry: false,
            selected_playlist: None,
            playlist_editable: false,
            signed_in: true,
            playback_retry_available: false,
        })
        .unwrap();

        assert_eq!(track_menu.entries()[0].action, Action::Open);
        assert_eq!(track_menu.entries()[0].label, "Play");
        assert!(track_menu.entries()[0].disabled_reason.is_none());

        track_menu.choose_subject(1);
        assert_eq!(track_menu.entries()[0].action, Action::Open);
        assert_eq!(track_menu.entries()[0].label, "Play");
        assert_eq!(
            track_menu.entries()[0].disabled_reason.as_deref(),
            Some("Choose the selected item")
        );

        let album_menu = ActionsMenu::open(ActionMenuContext {
            selected: Some(ActionSubject {
                identity: "selected-album".into(),
                kind: SubjectKind::Album,
                title: "Selected album".into(),
                has_album: false,
            }),
            playing: None,
            selected_is_queue_entry: false,
            selected_playlist: None,
            playlist_editable: false,
            signed_in: true,
            playback_retry_available: false,
        })
        .unwrap();
        assert_eq!(album_menu.entries()[0].label, "Open");
    }

    #[test]
    fn application_actions_open_without_a_media_subject() {
        let menu = ActionsMenu::open(ActionMenuContext {
            selected: None,
            playing: None,
            selected_is_queue_entry: false,
            selected_playlist: None,
            playlist_editable: false,
            signed_in: true,
            playback_retry_available: true,
        })
        .unwrap();

        assert_eq!(menu.subject(), None);
        assert_eq!(menu.entries().len(), 2);
        assert!(menu.entries().iter().all(|entry| entry.disabled_reason.is_none()));
        assert!(menu.entries().iter().any(|entry| entry.action == Action::Logout));
        assert!(
            menu.entries()
                .iter()
                .any(|entry| entry.action == Action::RetryPlayback)
        );
    }
}
