use redwax_ytm::{Browsable, Item, Page};

use crate::ui;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Visit {
    visit_id: u64,
    content_revision: u64,
    page: Page,
    opened_from: Option<Browsable>,
    cache_key: Option<String>,
}

/// An optimistic playlist removal that can be restored only into the exact
/// visit version it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRemoval {
    visit_id: u64,
    content_revision: u64,
    playlist_id: String,
    shelf_index: usize,
    item_index: usize,
    item: Item,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilterAnchor {
    shelf_index: usize,
    item_index: usize,
    item: Item,
}

impl FilterAnchor {
    fn capture(page: &Page, selection: (usize, usize)) -> Option<FilterAnchor> {
        let (shelf_index, item_index) = selection;
        let item = page.shelves.get(shelf_index)?.items.get(item_index)?.clone();
        Some(FilterAnchor {
            shelf_index,
            item_index,
            item,
        })
    }

    fn resolve(&self, page: &Page) -> Option<(usize, usize)> {
        if page
            .shelves
            .get(self.shelf_index)
            .and_then(|shelf| shelf.items.get(self.item_index))
            == Some(&self.item)
        {
            return Some((self.shelf_index, self.item_index));
        }
        let mut matching_coordinates = page.shelves.iter().enumerate().flat_map(|(shelf_index, shelf)| {
            shelf
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| *item == &self.item)
                .map(move |(item_index, _)| (shelf_index, item_index))
        });
        let unique_coordinates = matching_coordinates.next()?;
        matching_coordinates
            .next()
            .is_none()
            .then_some(unique_coordinates)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ItemKind {
    #[default]
    All,
    Tracks,
    Albums,
    Playlists,
    Artists,
}

impl ItemKind {
    pub const ALL: [ItemKind; 5] = [
        ItemKind::All,
        ItemKind::Tracks,
        ItemKind::Albums,
        ItemKind::Playlists,
        ItemKind::Artists,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ItemKind::All => "all",
            ItemKind::Tracks => "tracks",
            ItemKind::Albums => "albums",
            ItemKind::Playlists => "playlists",
            ItemKind::Artists => "artists",
        }
    }

    fn includes(self, item: &Item) -> bool {
        matches!(self, ItemKind::All)
            || matches!(
                (self, item),
                (ItemKind::Tracks, Item::Track(_))
                    | (ItemKind::Albums, Item::Album(_))
                    | (ItemKind::Playlists, Item::Playlist(_))
                    | (ItemKind::Artists, Item::Artist(_))
            )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Browser {
    visits: Vec<Visit>,
    selected_row_index: Option<usize>,
    scroll_offset: usize,
    page_loading: bool,
    projection_revision: u64,
    next_visit_id: u64,
    item_filter: ItemKind,
    filter_anchor: Option<FilterAnchor>,
}

impl Browser {
    pub fn pages(&self) -> impl DoubleEndedIterator<Item = &Page> + ExactSizeIterator {
        self.visits.iter().map(|visit| &visit.page)
    }

    pub fn depth(&self) -> usize {
        self.visits.len()
    }

    pub fn cursor(&self) -> Option<usize> {
        self.selected_row_index
    }

    pub fn scroll(&self) -> usize {
        self.scroll_offset
    }

    pub fn busy(&self) -> bool {
        self.page_loading
    }

    pub fn revision(&self) -> u64 {
        self.projection_revision
    }

    pub fn filter(&self) -> ItemKind {
        self.item_filter
    }

    /// Change only the display projection. Canonical page order and playback
    /// state remain untouched, and the same canonical item stays selected.
    pub fn set_filter(&mut self, item_filter: ItemKind) {
        if self.item_filter == item_filter {
            return;
        }
        let selected_coordinates = self.selected();
        if self.item_filter == ItemKind::All && item_filter != ItemKind::All {
            let filter_anchor = selected_coordinates
                .and_then(|selection| self.top().and_then(|page| FilterAnchor::capture(page, selection)));
            self.filter_anchor = filter_anchor;
        }
        let anchored_coordinates = self
            .filter_anchor
            .as_ref()
            .and_then(|anchor| self.top().and_then(|page| anchor.resolve(page)));
        let desired_coordinates = if item_filter == ItemKind::All {
            self.filter_anchor = None;
            anchored_coordinates.or(selected_coordinates)
        } else {
            selected_coordinates.or(anchored_coordinates)
        };
        self.item_filter = item_filter;
        self.projection_revision = self.projection_revision.wrapping_add(1);
        let projected_rows = self.rows();
        self.selected_row_index = desired_coordinates
            .and_then(|(shelf_index, item_index)| {
                projected_rows
                    .iter()
                    .position(|projected_row| *projected_row == ui::Row::Item(shelf_index, item_index))
            })
            .or_else(|| {
                projected_rows
                    .iter()
                    .position(|projected_row| projected_row.selectable())
            });
        self.scroll_offset = 0;
    }

    pub fn cycle_filter(&mut self, forward: bool) -> ItemKind {
        let mut available_filters = vec![ItemKind::All];
        if let Some(page) = self.top() {
            for kind in ItemKind::ALL.into_iter().skip(1) {
                if page
                    .shelves
                    .iter()
                    .flat_map(|shelf| &shelf.items)
                    .any(|item| kind.includes(item))
                {
                    available_filters.push(kind);
                }
            }
        }
        if available_filters.len() <= 2 {
            self.set_filter(ItemKind::All);
            return self.item_filter;
        }
        let current_filter_index = available_filters
            .iter()
            .position(|kind| *kind == self.item_filter)
            .unwrap_or(0);
        let next_filter_index = match forward {
            true => (current_filter_index + 1) % available_filters.len(),
            false => (current_filter_index + available_filters.len() - 1) % available_filters.len(),
        };
        self.set_filter(available_filters[next_filter_index]);
        self.item_filter
    }

    pub fn accepts(&self, row_target: &crate::ui::hit::RowTarget) -> bool {
        if row_target.projection_revision != self.projection_revision {
            return false;
        }
        self.top()
            .and_then(|page| page.shelves.get(row_target.shelf_index))
            .and_then(|shelf| shelf.items.get(row_target.item_index))
            == Some(&row_target.item)
    }

    pub(crate) fn set_busy(&mut self, page_loading: bool) {
        self.page_loading = page_loading;
    }

    pub fn select_row(&mut self, projected_row_index: Option<usize>) {
        self.selected_row_index = projected_row_index.filter(|row_index| {
            self.rows()
                .get(*row_index)
                .is_some_and(|projected_row| projected_row.selectable())
        });
        self.settle_cursor();
    }

    pub fn viewport_scroll(&self, viewport_height: usize) -> usize {
        if viewport_height == 0 {
            return self.scroll_offset;
        }
        let mut scroll_offset = self.scroll_offset;
        if let Some(selected_row_index) = self.selected_row_index {
            if selected_row_index < scroll_offset {
                scroll_offset = selected_row_index;
            } else if selected_row_index >= scroll_offset.saturating_add(viewport_height) {
                scroll_offset = selected_row_index + 1 - viewport_height;
            }
        }
        scroll_offset.min(self.rows().len().saturating_sub(viewport_height))
    }

    pub(crate) fn reconcile_viewport(&mut self, scroll_offset: usize) {
        self.scroll_offset = scroll_offset.min(self.rows().len().saturating_sub(1));
    }

    pub(crate) fn cache_key(&self) -> Option<&str> {
        self.visits.last()?.cache_key.as_deref()
    }

    pub(crate) fn bind_cache_key(&mut self, cache_key: Option<String>) {
        if let Some(visit) = self.visits.last_mut() {
            visit.cache_key = cache_key;
        }
    }

    pub(crate) fn append(&mut self, items: Vec<Item>, continuation: Option<redwax_ytm::Continuation>) {
        let Some(visit) = self.visits.last_mut() else {
            return;
        };
        visit.page.more = continuation;
        if let Some(shelf) = visit.page.shelves.last_mut() {
            shelf.items.extend(items);
        } else {
            visit.page.shelves.push(redwax_ytm::Shelf::new("Items", items));
        }
        visit.content_revision = visit.content_revision.wrapping_add(1);
        self.rows_changed();
    }

    pub(crate) fn set_queue_label(&mut self, radio_enabled: bool) -> bool {
        let queue_title = if radio_enabled { "Radio queue" } else { "Queue" };
        let Some(visit) = self
            .visits
            .last_mut()
            .filter(|visit| visit.page.title != queue_title)
        else {
            return false;
        };
        visit.page.title = queue_title.to_owned();
        if let Some(subtitle) = visit.page.subtitle.as_mut() {
            let automatic_suffix = " · continues automatically";
            if radio_enabled {
                subtitle.push_str(automatic_suffix);
            } else if subtitle.ends_with(automatic_suffix) {
                subtitle.truncate(subtitle.len() - automatic_suffix.len());
            }
        }
        true
    }

    pub(crate) fn remove_selected_playlist_item(&mut self) -> Option<BrowserRemoval> {
        let (shelf_index, item_index) = self.selected()?;
        let current_visit = self.visits.last_mut()?;
        let playlist_id = match &current_visit.opened_from {
            Some(Browsable::Playlist(playlist)) => playlist.playlist_id.clone(),
            _ => return None,
        };
        let shelf_items = &mut current_visit.page.shelves.get_mut(shelf_index)?.items;
        if item_index >= shelf_items.len() {
            return None;
        }
        let removed_item = shelf_items.remove(item_index);
        current_visit.content_revision = current_visit.content_revision.wrapping_add(1);
        let removal = BrowserRemoval {
            visit_id: current_visit.visit_id,
            content_revision: current_visit.content_revision,
            playlist_id,
            shelf_index,
            item_index,
            item: removed_item,
        };
        self.rows_changed();
        Some(removal)
    }

    pub(crate) fn restore_removal(&mut self, removal: BrowserRemoval) -> bool {
        let Some(current_visit) = self.visits.last_mut() else {
            return false;
        };
        if current_visit.visit_id != removal.visit_id
            || current_visit.content_revision != removal.content_revision
        {
            return false;
        }
        if !matches!(
            &current_visit.opened_from,
            Some(Browsable::Playlist(playlist)) if playlist.playlist_id == removal.playlist_id
        ) {
            return false;
        }
        let Some(shelf) = current_visit.page.shelves.get_mut(removal.shelf_index) else {
            return false;
        };
        if removal.item_index > shelf.items.len()
            || shelf.items.get(removal.item_index) == Some(&removal.item)
        {
            return false;
        }
        shelf.items.insert(removal.item_index, removal.item);
        current_visit.content_revision = current_visit.content_revision.wrapping_add(1);
        self.rows_changed();
        true
    }

    pub(crate) fn remove_playlist_track(
        &mut self,
        playlist_id: &str,
        video_id: &str,
        playlist_entry: Option<&redwax_ytm::PlaylistEntry>,
    ) {
        let mut any_visit_changed = false;
        for visit in &mut self.visits {
            if !matches!(
                &visit.opened_from,
                Some(Browsable::Playlist(playlist)) if playlist.playlist_id == playlist_id
            ) {
                continue;
            }
            if visit.page.remove_playlist_track(video_id, playlist_entry) {
                visit.content_revision = visit.content_revision.wrapping_add(1);
                any_visit_changed = true;
            }
        }
        if any_visit_changed {
            self.rows_changed();
        }
    }

    pub(crate) fn fill_durations(&mut self, durations: &crate::durations::Durations) -> bool {
        let mut any_duration_filled = false;
        for visit in &mut self.visits {
            if durations.fill(&mut visit.page) {
                any_duration_filled = true;
            }
        }
        if any_duration_filled {
            self.rows_changed();
        }
        any_duration_filled
    }

    pub(crate) fn missing_duration_ids(&self, viewport_height: usize) -> Vec<String> {
        let Some(page) = self.top() else {
            return Vec::new();
        };
        let visible_tracks = self
            .rows()
            .into_iter()
            .skip(self.scroll_offset)
            .take(viewport_height)
            .filter_map(|row| match row {
                ui::Row::Item(shelf_index, item_index) => {
                    page.shelves.get(shelf_index)?.items.get(item_index)?.as_track()
                }
                _ => None,
            });
        let mut seen_video_ids = std::collections::HashSet::new();
        visible_tracks
            .chain(page.tracks())
            .filter(|track| {
                track.duration_seconds.is_none() && seen_video_ids.insert(track.video_id.as_str())
            })
            .map(|track| track.video_id.clone())
            .collect()
    }

    pub fn top(&self) -> Option<&Page> {
        self.visits.last().map(|visit| &visit.page)
    }

    pub fn show(&mut self, page: Page) {
        self.show_from(page, None);
    }

    pub fn show_from(&mut self, page: Page, opened_from: Option<Browsable>) {
        let new_visit = self.new_visit(page, opened_from);
        self.visits.push(new_visit);
        self.selected_row_index = None;
        self.scroll_offset = 0;
        self.item_filter = ItemKind::All;
        self.filter_anchor = None;
        self.rows_changed();
    }

    /// Swap the root page in place, keeping the cursor. For live pages like the queue.
    pub fn replace(&mut self, page: Page) {
        let replacement_visit = self.new_visit(page, None);
        self.visits = vec![replacement_visit];
        self.item_filter = ItemKind::All;
        self.filter_anchor = None;
        self.rows_changed();
    }

    /// Swap the top page for a fresher copy, keeping the cursor. Nothing moves
    /// when it is the same page.
    pub fn refresh_top(&mut self, page: Page) {
        if self.visits.last().map(|visit| &visit.page) == Some(&page) {
            if let Some(top) = self.visits.last_mut() {
                // A confirmed refresh makes an older optimistic rollback
                // obsolete even when the returned page has equal content.
                top.content_revision = top.content_revision.wrapping_add(1);
            }
            return;
        }
        if let Some(top) = self.visits.last_mut() {
            top.page = page;
            top.content_revision = top.content_revision.wrapping_add(1);
        } else {
            let new_visit = self.new_visit(page, None);
            self.visits.push(new_visit);
        }
        self.rows_changed();
    }

    pub fn to_depth(&mut self, visit_depth: usize) -> bool {
        if visit_depth + 1 >= self.visits.len() {
            return false;
        }
        self.visits.truncate(visit_depth + 1);
        self.selected_row_index = None;
        self.scroll_offset = 0;
        self.item_filter = ItemKind::All;
        self.filter_anchor = None;
        self.rows_changed();
        true
    }

    pub fn back(&mut self) -> bool {
        if self.visits.len() <= 1 {
            return false;
        }
        self.visits.pop();
        self.selected_row_index = None;
        self.scroll_offset = 0;
        self.item_filter = ItemKind::All;
        self.filter_anchor = None;
        self.rows_changed();
        true
    }

    /// The highlighted item as (shelf index, item index).
    pub fn selected(&self) -> Option<(usize, usize)> {
        let projected_rows = self.rows();
        match projected_rows.get(self.selected_row_index?) {
            Some(ui::Row::Item(shelf_index, item_index)) => Some((*shelf_index, *item_index)),
            _ => None,
        }
    }

    pub fn selected_item(&self) -> Option<&Item> {
        let (shelf_index, item_index) = self.selected()?;
        self.visits
            .last()
            .map(|visit| &visit.page)?
            .shelves
            .get(shelf_index)?
            .items
            .get(item_index)
    }

    pub fn rows(&self) -> Vec<ui::Row> {
        let Some(page) = self.visits.last().map(|visit| &visit.page) else {
            return Vec::new();
        };
        ui::page_rows(page)
            .into_iter()
            .filter(|row| match row {
                ui::Row::Item(shelf_index, item_index) => self
                    .item_filter
                    .includes(&page.shelves[*shelf_index].items[*item_index]),
                _ => self.item_filter == ItemKind::All,
            })
            .collect()
    }

    /// The playlist whose page is open, `None` on any other page.
    pub fn open_playlist(&self) -> Option<&redwax_ytm::Playlist> {
        match &self.visits.last()?.opened_from {
            Some(Browsable::Playlist(playlist)) => Some(playlist),
            _ => None,
        }
    }

    /// Keep the cursor where it was when that row still holds an item, else
    /// put it on the first item of the page.
    pub(crate) fn settle_cursor(&mut self) {
        let projected_rows = self.rows();
        let retained_row_index = self.selected_row_index.filter(|row_index| {
            projected_rows
                .get(*row_index)
                .is_some_and(|projected_row| projected_row.selectable())
        });
        self.selected_row_index = retained_row_index.or_else(|| {
            projected_rows
                .iter()
                .position(|projected_row| projected_row.selectable())
        });
        if let Some(selected_row_index) = self.selected_row_index {
            self.scroll_offset = self.scroll_offset.min(selected_row_index);
        }
    }

    /// Invalidate targets painted from an older row projection before keeping
    /// the cursor on a valid row in the new projection.
    fn rows_changed(&mut self) {
        self.projection_revision = self.projection_revision.wrapping_add(1);
        self.settle_cursor();
    }

    fn new_visit(&mut self, page: Page, opened_from: Option<Browsable>) -> Visit {
        self.next_visit_id = self.next_visit_id.wrapping_add(1);
        Visit {
            visit_id: self.next_visit_id,
            content_revision: 0,
            page,
            opened_from,
            cache_key: None,
        }
    }

    pub(crate) fn move_cursor(&mut self, row_delta: isize) {
        let projected_rows = self.rows();
        if projected_rows.is_empty() {
            return;
        }
        let mut row_index = self.selected_row_index.unwrap_or(0) as isize;
        let last_row_index = projected_rows.len() as isize - 1;
        loop {
            let next_row_index = (row_index + row_delta).clamp(0, last_row_index);
            if next_row_index == row_index {
                break;
            }
            row_index = next_row_index;
            if projected_rows[row_index as usize].selectable() {
                self.selected_row_index = Some(row_index as usize);
                return;
            }
        }
    }

    /// The shelf the cursor sits in. A page draws its shelves as sections, so
    /// this is what `{` and `}` step between and what `gg` and `G` stay inside.
    pub fn section(&self) -> Option<usize> {
        match self.rows().get(self.selected_row_index?)? {
            ui::Row::Item(shelf_index, _) | ui::Row::Header(shelf_index) => Some(*shelf_index),
            _ => None,
        }
    }

    fn section_edge(&self, shelf_index: usize, select_last: bool) -> Option<usize> {
        let projected_rows = self.rows();
        let mut shelf_rows = projected_rows.iter().enumerate().filter(
            |(_, row)| matches!(row, ui::Row::Item(row_shelf_index, _) if *row_shelf_index == shelf_index),
        );
        match select_last {
            true => shelf_rows.next_back().map(|(row_index, _)| row_index),
            false => shelf_rows.next().map(|(row_index, _)| row_index),
        }
    }

    fn sections(&self) -> Vec<usize> {
        let mut shelf_indexes: Vec<usize> = self
            .rows()
            .iter()
            .filter_map(|row| match row {
                ui::Row::Item(shelf_index, _) => Some(*shelf_index),
                _ => None,
            })
            .collect();
        shelf_indexes.dedup();
        shelf_indexes
    }

    /// Step to the head of the next or previous section. At the last section
    /// `}` runs to its end and `{` to its start, so neither key is ever dead.
    pub fn jump_section(&mut self, forward: bool) {
        let selectable_shelf_indexes = self.sections();
        if selectable_shelf_indexes.is_empty() {
            return;
        }
        let current_section_position = self.section().and_then(|shelf_index| {
            selectable_shelf_indexes
                .iter()
                .position(|section_index| *section_index == shelf_index)
        });
        let destination_section = match (current_section_position, forward) {
            (Some(section_position), true) if section_position + 1 < selectable_shelf_indexes.len() => {
                Some(selectable_shelf_indexes[section_position + 1])
            }
            (Some(section_position), false) if section_position > 0 => {
                Some(selectable_shelf_indexes[section_position - 1])
            }
            (Some(_), _) => None,
            (None, true) => selectable_shelf_indexes.first().copied(),
            (None, false) => selectable_shelf_indexes.last().copied(),
        };
        let destination_row = match destination_section {
            Some(shelf_index) => self.section_edge(shelf_index, false),
            // Already at the end of the run of sections, so go to the end of
            // this one rather than doing nothing at all.
            None => self
                .section()
                .and_then(|shelf_index| self.section_edge(shelf_index, forward)),
        };
        if destination_row.is_some() {
            self.selected_row_index = destination_row;
        }
    }

    /// The top or the bottom of the section the cursor is in. A page with one
    /// section is the whole page, which is what a flat playlist wants.
    pub(crate) fn cursor_to_edge(&mut self, select_last: bool) {
        let destination_row = match self.section() {
            Some(shelf_index) => self.section_edge(shelf_index, select_last),
            None => {
                let projected_rows = self.rows();
                match select_last {
                    true => projected_rows
                        .iter()
                        .rposition(|projected_row| projected_row.selectable()),
                    false => projected_rows
                        .iter()
                        .position(|projected_row| projected_row.selectable()),
                }
            }
        };
        if destination_row.is_some() {
            self.selected_row_index = destination_row;
        }
    }

    /// Put the cursor on the first row whose text holds `needle`, from the top.
    /// Returns false when nothing on the page matches, so the bar can say so.
    pub fn find(&mut self, needle: &str) -> bool {
        let first_matching_row = self.matching_rows(needle).into_iter().next();
        if let Some(row_index) = first_matching_row {
            self.selected_row_index = Some(row_index);
        }
        first_matching_row.is_some()
    }

    /// The selected match and total match count in the current projection.
    pub fn find_position(&self, needle: &str) -> Option<(usize, usize)> {
        let matching_rows = self.matching_rows(needle);
        let current_row = self.selected_row_index?;
        let match_position = matching_rows
            .iter()
            .position(|row_index| *row_index == current_row)?
            + 1;
        Some((match_position, matching_rows.len()))
    }

    pub fn find_next(&mut self, needle: &str, forward: bool) -> Option<(usize, usize)> {
        let matching_rows = self.matching_rows(needle);
        if matching_rows.is_empty() {
            return None;
        }
        let current_row = self.selected_row_index.unwrap_or(0);
        let next_matching_row = match forward {
            true => matching_rows
                .iter()
                .copied()
                .find(|row_index| *row_index > current_row)
                .unwrap_or(matching_rows[0]),
            false => matching_rows
                .iter()
                .rev()
                .copied()
                .find(|row_index| *row_index < current_row)
                .unwrap_or(*matching_rows.last().unwrap_or(&matching_rows[0])),
        };
        self.selected_row_index = Some(next_matching_row);
        let match_position = matching_rows
            .iter()
            .position(|row_index| *row_index == next_matching_row)
            .unwrap_or(0)
            + 1;
        Some((match_position, matching_rows.len()))
    }

    fn matching_rows(&self, needle: &str) -> Vec<usize> {
        if needle.is_empty() {
            return Vec::new();
        }
        let Some(page) = self.top() else {
            return Vec::new();
        };
        let lowercase_needle = needle.to_lowercase();
        self.rows()
            .iter()
            .enumerate()
            .filter(|(_, row)| ui::row_text(page, **row).contains(&lowercase_needle))
            .map(|(row_index, _)| row_index)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, title: &str) -> Item {
        Item::Track(redwax_ytm::Track {
            video_id: id.to_string(),
            title: title.to_string(),
            artists: Vec::new(),
            album: None,
            album_browse_id: None,
            duration_seconds: None,
            thumbnail_url: None,
            liked: false,
            playlist_entry: None,
        })
    }

    fn album(id: &str, title: &str) -> Item {
        Item::Album(redwax_ytm::Album {
            browse_id: id.to_string(),
            title: title.to_string(),
            artists: Vec::new(),
            year: None,
            thumbnail_url: None,
        })
    }

    fn playlist_page(items: Vec<Item>) -> (Page, Browsable) {
        let playlist = redwax_ytm::Playlist {
            playlist_id: "PL-test".to_string(),
            title: "Playlist".to_string(),
            subtitle: None,
            thumbnail_url: None,
        };
        (
            Page::new("Playlist", vec![redwax_ytm::Shelf::new("Tracks", items)]),
            Browsable::Playlist(playlist),
        )
    }

    #[test]
    fn back_navigation_restores_the_page_with_its_own_cache_identity() {
        let mut browser = Browser::default();
        browser.show(Page::new("Home", Vec::new()));
        browser.bind_cache_key(Some("home".to_owned()));
        browser.show_from(Page::new("Album", Vec::new()), None);
        browser.bind_cache_key(Some("album:synthetic".to_owned()));
        assert!(browser.back());
        assert_eq!(browser.top().expect("back retains the root").title, "Home");
        assert_eq!(browser.cache_key(), Some("home"));
        assert_eq!(browser.pages().len(), 1);
    }

    #[test]
    fn an_empty_viewport_does_not_change_navigation() {
        let mut browser = Browser::default();
        browser.show(Page::new("Empty", Vec::new()));
        assert_eq!(browser.viewport_scroll(0), 0);
        assert_eq!(browser.cursor(), None);
        browser.select_row(Some(90));
        assert_eq!(browser.cursor(), None);
    }

    #[test]
    fn optimistic_removal_restores_only_the_same_visit_version() {
        let first = track("1", "First");
        let second = track("2", "Second");
        let (page, opened) = playlist_page(vec![first.clone(), second.clone()]);
        let mut browser = Browser::default();
        browser.show_from(page, Some(opened));

        let removal = browser.remove_selected_playlist_item().unwrap();
        assert_eq!(browser.top().unwrap().shelves[0].items, vec![second.clone()]);
        assert!(browser.restore_removal(removal));
        assert_eq!(browser.top().unwrap().shelves[0].items, vec![first, second]);
    }

    #[test]
    fn refreshed_visit_rejects_an_obsolete_removal_rollback() {
        let (page, opened) = playlist_page(vec![track("1", "First"), track("2", "Second")]);
        let mut browser = Browser::default();
        browser.show_from(page, Some(opened));
        let removal = browser.remove_selected_playlist_item().unwrap();
        let refreshed = browser.top().unwrap().clone();

        browser.refresh_top(refreshed.clone());

        assert!(!browser.restore_removal(removal));
        assert_eq!(browser.top(), Some(&refreshed));
    }

    #[test]
    fn filtering_preserves_canonical_order_and_restores_exact_selection() {
        let page = Page::new(
            "Mixed",
            vec![redwax_ytm::Shelf::new(
                "Items",
                vec![
                    track("same", "Duplicate"),
                    track("same", "Duplicate"),
                    album("a1", "Album"),
                ],
            )],
        );
        let canonical = page.clone();
        let mut browser = Browser::default();
        browser.show(page);
        let second = browser
            .rows()
            .iter()
            .position(|row| *row == ui::Row::Item(0, 1))
            .unwrap();
        browser.select_row(Some(second));

        browser.set_filter(ItemKind::Albums);
        assert_eq!(browser.selected(), Some((0, 2)));
        browser.set_filter(ItemKind::All);

        assert_eq!(browser.selected(), Some((0, 1)));
        assert_eq!(browser.top(), Some(&canonical));
    }

    #[test]
    fn clearing_a_filter_follows_a_uniquely_relocated_source_item() {
        let selected = track("wanted", "Wanted");
        let shown_album = album("a1", "Album");
        let mut browser = Browser::default();
        browser.show(Page::new(
            "Mixed",
            vec![redwax_ytm::Shelf::new(
                "Items",
                vec![selected.clone(), shown_album.clone()],
            )],
        ));
        browser.set_filter(ItemKind::Albums);
        browser.refresh_top(Page::new(
            "Mixed",
            vec![redwax_ytm::Shelf::new(
                "Items",
                vec![track("other", "Other"), selected, shown_album],
            )],
        ));

        browser.set_filter(ItemKind::All);

        assert_eq!(browser.selected(), Some((0, 1)));
    }

    #[test]
    fn cycling_filters_skips_kinds_absent_from_the_page() {
        let mut browser = Browser::default();
        browser.show(Page::new(
            "Mixed",
            vec![redwax_ytm::Shelf::new(
                "Items",
                vec![track("t1", "Track"), album("a1", "Album")],
            )],
        ));

        assert_eq!(browser.cycle_filter(true), ItemKind::Tracks);
        assert_eq!(browser.cycle_filter(true), ItemKind::Albums);
        assert_eq!(browser.cycle_filter(true), ItemKind::All);
        assert_eq!(browser.cycle_filter(false), ItemKind::Albums);
    }

    #[test]
    fn a_single_result_kind_does_not_create_a_redundant_filter() {
        let mut browser = Browser::default();
        browser.show(Page::new(
            "Tracks",
            vec![redwax_ytm::Shelf::new("Tracks", vec![track("t1", "Track")])],
        ));

        assert_eq!(browser.cycle_filter(true), ItemKind::All);
        assert_eq!(browser.filter(), ItemKind::All);
    }

    #[test]
    fn row_targets_survive_cursor_moves_but_not_projection_changes() {
        let first = track("1", "First");
        let mut browser = Browser::default();
        browser.show(Page::new(
            "Tracks",
            vec![redwax_ytm::Shelf::new(
                "Tracks",
                vec![first.clone(), track("2", "Second")],
            )],
        ));
        let target = crate::ui::hit::RowTarget {
            browser_target: crate::app::Target::Home,
            projection_revision: browser.revision(),
            shelf_index: 0,
            item_index: 0,
            item: first,
        };

        browser.move_cursor(1);
        assert!(browser.accepts(&target));
        browser.set_filter(ItemKind::Tracks);
        assert!(!browser.accepts(&target));
    }

    #[test]
    fn find_next_reports_position_and_wraps_in_projected_order() {
        let mut browser = Browser::default();
        browser.show(Page::new(
            "Tracks",
            vec![redwax_ytm::Shelf::new(
                "Tracks",
                vec![track("1", "Blue"), track("2", "Red"), track("3", "Blue again")],
            )],
        ));

        assert_eq!(browser.find_next("blue", true), Some((2, 2)));
        assert_eq!(browser.selected(), Some((0, 2)));
        assert_eq!(browser.find_position("blue"), Some((2, 2)));
        assert_eq!(browser.find_next("blue", true), Some((1, 2)));
        assert_eq!(browser.selected(), Some((0, 0)));
        assert_eq!(browser.find_next("blue", false), Some((2, 2)));
    }

    #[test]
    fn duration_requests_prioritize_the_filtered_viewport_then_hidden_tracks() {
        let mut browser = Browser::default();
        browser.show(Page::new(
            "Mixed",
            vec![redwax_ytm::Shelf::new(
                "Items",
                vec![
                    album("a", "Album"),
                    track("one", "One"),
                    album("b", "Another album"),
                    track("two", "Two"),
                    track("one", "One again"),
                ],
            )],
        ));
        browser.set_filter(ItemKind::Tracks);
        let second_track_row = browser
            .rows()
            .iter()
            .position(|row| *row == ui::Row::Item(0, 3))
            .unwrap();
        browser.reconcile_viewport(second_track_row);
        assert_eq!(browser.missing_duration_ids(1), ["two", "one"]);

        browser.set_filter(ItemKind::Albums);
        assert_eq!(browser.missing_duration_ids(1), ["one", "two"]);
    }
}
