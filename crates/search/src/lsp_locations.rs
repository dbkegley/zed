//! A picker modal (with a preview pane) for presenting LSP location results
//! such as find-all-references. It is an alternative to the multibuffer tab:
//! you explore the results in a list, preview each one inline, and jump to the
//! one you want, without cluttering the tab bar.
//!
//! The picker is generic over an [`LspPickerKind`] so the same modal can later
//! back definition / implementation / declaration / type-definition. Only the
//! per-kind query, title and cache slot differ. References is the first kind
//! wired up and serves as the template for the rest.
//!
//! Each kind exposes two workspace actions:
//!
//! - `open` — run a fresh LSP query and open the picker with the new results.
//! - `toggle` — if the picker is already open dismiss it; otherwise reopen the
//!   picker from the previously cached result set (or run a fresh query if there
//!   is no cache yet).
//!
//! The cached results hold strong [`Location`]s (buffer + anchor range), so a
//! `toggle` after edits still points at the correct positions because anchors
//! track edits while the buffer is alive.

use std::ops::Range;

use collections::HashMap;
use editor::Editor;
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, AppContext, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, HighlightStyle, StyledText, Subscription, Task, TextStyle, WeakEntity, actions,
    prelude::*,
};
use language::{Buffer, LanguageAwareStyling};
use picker::{Picker, PickerDelegate};
use project::{Location, Project, ProjectPath};
use settings::Settings as _;
use text::{Anchor, Point};
use theme_settings::ThemeSettings;
use ui::{ListItem, ListItemSpacing, Rems, prelude::*};
use ui::{Divider, FluentBuilder};
use util::ResultExt as _;
use workspace::item::ItemSettings;
use workspace::{ModalView, Workspace};

actions!(
    editor,
    [
        /// Finds all references to the symbol under the cursor and shows them in
        /// a picker with a preview pane. Dismisses the picker if it is already
        /// open.
        OpenReferencesPicker,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(register).detach();
}

fn register(
    workspace: &mut Workspace,
    _window: Option<&mut Window>,
    _cx: &mut Context<Workspace>,
) {
    workspace.register_action(|workspace, _: &OpenReferencesPicker, window, cx| {
        LspLocationsPicker::open(LspPickerKind::References, workspace, window, cx);
    });
}

#[derive(Clone, Copy, Debug)]
pub enum LspPickerKind {
    References,
}

impl LspPickerKind {
    fn placeholder(self) -> &'static str {
        match self {
            LspPickerKind::References => "Filter references…",
        }
    }

    /// Runs the LSP query for this kind against the symbol under the editor's
    /// cursor, returning the raw locations.
    fn run_query(
        self,
        editor: &mut Editor,
        project: &Entity<Project>,
        cx: &mut Context<Editor>,
    ) -> Option<Task<anyhow::Result<Vec<Location>>>> {
        match self {
            LspPickerKind::References => editor.references_at_cursor(project, cx),
        }
    }
}

pub struct LspLocationsPicker {
    picker: Entity<Picker<LspLocationsDelegate>>,
    _subscription: Subscription,
}

impl LspLocationsPicker {
    /// Toggles the picker for `kind`: if it is already open, dismiss it;
    /// otherwise run a fresh LSP query and open it with the results. Results are
    /// always re-queried, so they never go stale and no buffers are pinned.
    fn open(
        kind: LspPickerKind,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if workspace.active_modal::<Self>(cx).is_some() {
            workspace.hide_modal(window, cx);
            return;
        }

        let Some(editor) = workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<Editor>())
        else {
            return;
        };
        let project = workspace.project().clone();
        let Some(query) = editor.update(cx, |editor, cx| kind.run_query(editor, &project, cx))
        else {
            return;
        };
        cx.spawn_in(window, async move |workspace, cx| {
            let locations = match query.await {
                Ok(locations) => locations,
                Err(error) => {
                    log::error!("LSP {kind:?} query failed: {error:?}");
                    return;
                }
            };
            if locations.is_empty() {
                return;
            }

            workspace
                .update_in(cx, |workspace, window, cx| {
                    let project = workspace.project().clone();
                    let workspace_handle = workspace.weak_handle();
                    workspace.toggle_modal(window, cx, |window, cx| {
                        Self::new(kind, locations, project, workspace_handle, window, cx)
                    });
                })
                .log_err();
        })
        .detach();
    }

    fn new(
        kind: LspPickerKind,
        locations: Vec<Location>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = LspLocationsDelegate::new(kind, locations, project.clone(), workspace, cx);
        let picker = cx.new(|cx| {
            Picker::list_with_preview(delegate, project, window, cx)
                .show_preview()
                .minimum_results_width(Rems(20.0))
        });
        let focus_handle = picker.focus_handle(cx);
        picker.update(cx, |picker, _| {
            picker.delegate.focus_handle = focus_handle;
        });
        let subscription = cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| {
            cx.emit(DismissEvent);
        });
        Self {
            picker,
            _subscription: subscription,
        }
    }
}

impl ModalView for LspLocationsPicker {}

impl EventEmitter<DismissEvent> for LspLocationsPicker {}

impl Focusable for LspLocationsPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for LspLocationsPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

/// A single location result, with the data the row and the preview need.
struct LocationMatch {
    path: ProjectPath,
    buffer: Entity<Buffer>,
    anchor_range: Range<Anchor>,
    /// Offset range of the match within the buffer (used to scroll the preview).
    range: Range<usize>,
    /// Offset range of the match within its line (used to highlight the row).
    relative_range: Range<usize>,
    line_text: String,
    line_number: u32,
}

/// A row in the grouped display list: a non-selectable file header, a match, or
/// a separator between file groups. `selected_index` indexes into this list.
enum Entry {
    Header(ProjectPath),
    Match(usize),
    Separator,
}

struct LspLocationsDelegate {
    kind: LspPickerKind,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    /// Every match for this query, in result order.
    all_matches: Vec<LocationMatch>,
    /// The subset of `all_matches` that passes the current filter query.
    matches: Vec<LocationMatch>,
    /// Grouped display rows derived from `matches`.
    entries: Vec<Entry>,
    selected_index: usize,
    max_line_number: u32,
    last_selection_change_time: Option<std::time::Instant>,
    last_click: Option<(usize, std::time::Instant)>,
}

const CLICK_THRESHOLD_MS: u128 = 50;
const DOUBLE_CLICK_THRESHOLD_MS: u128 = 300;

impl LspLocationsDelegate {
    fn new(
        kind: LspPickerKind,
        locations: Vec<Location>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        cx: &App,
    ) -> Self {
        let all_matches = build_location_matches(&locations, cx);
        let matches = all_matches.iter().map(LocationMatch::clone_shallow).collect();
        let mut this = Self {
            kind,
            project,
            workspace,
            focus_handle: cx.focus_handle(),
            all_matches,
            matches,
            entries: Vec::new(),
            selected_index: 0,
            max_line_number: 0,
            last_selection_change_time: None,
            last_click: None,
        };
        this.rebuild_entries();
        this
    }

    /// Rebuilds the grouped [`Self::entries`] from the filtered [`Self::matches`]:
    /// one header per file, its matches, and a separator before every group
    /// after the first. Selection snaps to the first selectable row.
    fn rebuild_entries(&mut self) {
        let mut entries = Vec::with_capacity(self.matches.len());
        let mut last_path: Option<&ProjectPath> = None;
        for (match_index, location_match) in self.matches.iter().enumerate() {
            if last_path != Some(&location_match.path) {
                if last_path.is_some() {
                    entries.push(Entry::Separator);
                }
                entries.push(Entry::Header(location_match.path.clone()));
                last_path = Some(&location_match.path);
            }
            entries.push(Entry::Match(match_index));
        }
        self.entries = entries;
        self.max_line_number = self
            .matches
            .iter()
            .map(|location_match| location_match.line_number)
            .max()
            .unwrap_or(0);
        self.selected_index = self.first_selectable_index().unwrap_or(0);
    }

    fn first_selectable_index(&self) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| matches!(entry, Entry::Match(_)))
    }

    fn selected_location_match(&self) -> Option<&LocationMatch> {
        match self.entries.get(self.selected_index)? {
            Entry::Match(match_index) => self.matches.get(*match_index),
            Entry::Header(_) | Entry::Separator => None,
        }
    }

    fn open_selected(&mut self, split: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(location_match) = self.selected_location_match() else {
            return;
        };
        let location = Location {
            buffer: location_match.buffer.clone(),
            range: location_match.anchor_range.clone(),
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            Editor::open_location(workspace, &location, split, window, cx);
        });
        cx.emit(DismissEvent);
    }
}

/// Converts the flat [`Location`] list into [`LocationMatch`]es, reading each
/// buffer's snapshot once.
fn build_location_matches(locations: &[Location], cx: &App) -> Vec<LocationMatch> {
    use gpui::EntityId;
    let mut snapshots: HashMap<EntityId, language::BufferSnapshot> = HashMap::default();
    let mut matches = Vec::with_capacity(locations.len());

    for location in locations {
        let snapshot = snapshots
            .entry(location.buffer.entity_id())
            .or_insert_with(|| location.buffer.read(cx).snapshot());

        let Some(file) = snapshot.file() else {
            continue;
        };
        let path = ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path().clone(),
        };

        let start_offset: usize = snapshot.summary_for_anchor(&location.range.start);
        let end_offset: usize = snapshot.summary_for_anchor(&location.range.end);
        let start_point = snapshot.offset_to_point(start_offset);
        let row = start_point.row;
        let line_start = snapshot.point_to_offset(Point::new(row, 0));
        let line_end = snapshot.point_to_offset(Point::new(row, snapshot.line_len(row)));
        let line_text: String = snapshot.text_for_range(line_start..line_end).collect();

        let relative_start = start_offset - line_start;
        let relative_end = (end_offset - line_start).min(line_text.len());

        matches.push(LocationMatch {
            path,
            buffer: location.buffer.clone(),
            anchor_range: location.range.clone(),
            range: start_offset..end_offset,
            relative_range: relative_start..relative_end,
            line_text,
            line_number: row + 1,
        });
    }

    // Group by file and order by position so the grouped display list is stable.
    matches.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.range.start.cmp(&b.range.start))
    });
    matches
}

impl PickerDelegate for LspLocationsDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "lsp locations picker"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> std::sync::Arc<str> {
        self.kind.placeholder().into()
    }

    fn match_count(&self) -> usize {
        self.entries.len()
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        matches!(self.entries.get(ix), Some(Entry::Match(_)))
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn select_on_hover(&self) -> bool {
        false
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
        self.last_selection_change_time = Some(std::time::Instant::now());
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();
        self.matches = self
            .all_matches
            .iter()
            .filter(|location_match| {
                if query.is_empty() {
                    return true;
                }
                location_match.line_text.to_lowercase().contains(&query)
                    || location_match
                        .path
                        .path
                        .as_unix_str()
                        .to_lowercase()
                        .contains(&query)
            })
            .map(LocationMatch::clone_shallow)
            .collect();
        self.rebuild_entries();
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        // Clicks (set_selected_index fires immediately before confirm) require a
        // double-click; the Enter key proceeds immediately.
        let now = std::time::Instant::now();
        let is_click = self
            .last_selection_change_time
            .map(|t| now.duration_since(t).as_millis() < CLICK_THRESHOLD_MS)
            .unwrap_or(false);
        if is_click {
            let is_double_click = self
                .last_click
                .map(|(ix, t)| {
                    ix == self.selected_index
                        && now.duration_since(t).as_millis() < DOUBLE_CLICK_THRESHOLD_MS
                })
                .unwrap_or(false);
            self.last_click = Some((self.selected_index, now));
            if !is_double_click {
                cx.focus_self(window);
                return;
            }
        }

        self.open_selected(secondary, window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn try_get_preview_data_for_match(&self, _cx: &App) -> Option<picker::PreviewUpdate> {
        let location_match = self.selected_location_match()?;
        Some(picker::PreviewUpdate::from_buffer(
            location_match.buffer.clone(),
            picker::MatchLocation {
                anchor_range: location_match.anchor_range.clone(),
                range: location_match.range.clone(),
            },
        ))
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match self.entries.get(ix)? {
            Entry::Separator => Some(
                div()
                    .py(DynamicSpacing::Base04.rems(cx))
                    .child(Divider::horizontal())
                    .into_any_element(),
            ),
            Entry::Header(path) => {
                let path_style = self.project.read(cx).path_style(cx);
                let file_name = path
                    .path
                    .file_name()
                    .map(|name| name.to_string())
                    .unwrap_or_default();
                let directory = path
                    .path
                    .parent()
                    .map(|parent| parent.display(path_style))
                    .map(SharedString::new)
                    .unwrap_or_default();
                let file_icon = ItemSettings::get_global(cx)
                    .file_icons
                    .then(|| FileIcons::get_icon(path.path.as_std_path(), cx))
                    .flatten()
                    .map(|icon| {
                        Icon::from_path(icon)
                            .color(Color::Muted)
                            .size(IconSize::Small)
                    });
                Some(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .px(DynamicSpacing::Base06.rems(cx))
                        .py_1()
                        .gap_1p5()
                        .children(file_icon)
                        .child(
                            h_flex()
                                .gap_1()
                                .child(Label::new(file_name).size(LabelSize::Small))
                                .when(!directory.is_empty(), |this| {
                                    this.child(
                                        Label::new(directory)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate_start(),
                                    )
                                }),
                        )
                        .into_any_element(),
                )
            }
            Entry::Match(match_index) => {
                let location_match = self.matches.get(*match_index)?;
                Some(
                    ListItem::new(ix)
                        .spacing(ListItemSpacing::Sparse)
                        .inset(true)
                        .toggle_state(selected)
                        .child(
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .gap_2p5()
                                .text_sm()
                                .child(
                                    h_flex()
                                        .w(rems(
                                            (self.max_line_number.max(1).ilog10() + 1) as f32 * 0.5,
                                        ))
                                        .justify_end()
                                        .child(
                                            Label::new(location_match.line_number.to_string())
                                                .color(Color::Custom(
                                                    cx.theme().colors().text_muted.opacity(0.5),
                                                )),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .child(render_matched_line(location_match, cx)),
                                ),
                        )
                        .into_any_element(),
                )
            }
        }
    }
}

impl LocationMatch {
    /// A clone that shares the underlying buffer handle. Used when rebuilding the
    /// filtered match list.
    fn clone_shallow(&self) -> Self {
        Self {
            path: self.path.clone(),
            buffer: self.buffer.clone(),
            anchor_range: self.anchor_range.clone(),
            range: self.range.clone(),
            relative_range: self.relative_range.clone(),
            line_text: self.line_text.clone(),
            line_number: self.line_number,
        }
    }
}

/// Renders the matched source line with syntax highlighting, overlaying the
/// match with a highlighted background and bold weight. Mirrors the text
/// finder's row rendering.
fn render_matched_line(location_match: &LocationMatch, cx: &App) -> StyledText {
    let settings = ThemeSettings::get_global(cx);
    let text_style = TextStyle {
        color: cx.theme().colors().text,
        font_family: settings.buffer_font.family.clone(),
        font_features: settings.buffer_font.features.clone(),
        font_fallbacks: settings.buffer_font.fallbacks.clone(),
        font_size: settings.buffer_font_size(cx).into(),
        font_weight: settings.buffer_font.weight,
        line_height: relative(1.),
        ..Default::default()
    };
    let original_line = &location_match.line_text;
    let line_text = original_line.trim_start();
    let trim_offset = original_line.len() - line_text.len();

    let match_style = HighlightStyle {
        background_color: Some(cx.theme().colors().search_match_background),
        font_weight: Some(gpui::FontWeight::BOLD),
        ..Default::default()
    };

    let line_start_abs = location_match.range.start - location_match.relative_range.start;
    let visible_start_abs = line_start_abs + trim_offset;
    let visible_end_abs = line_start_abs + original_line.len();

    let snapshot = location_match.buffer.read(cx).snapshot();
    let syntax_theme = cx.theme().syntax();
    let mut syntax_highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut current_offset = 0;
    for chunk in snapshot.chunks(
        visible_start_abs..visible_end_abs,
        LanguageAwareStyling {
            tree_sitter: true,
            diagnostics: false,
        },
    ) {
        let chunk_len = chunk.text.len();
        if let Some(style) = chunk
            .syntax_highlight_id
            .and_then(|id| syntax_theme.get(id).copied())
        {
            syntax_highlights.push((current_offset..current_offset + chunk_len, style));
        }
        current_offset += chunk_len;
    }

    let match_start = location_match
        .range
        .start
        .clamp(visible_start_abs, visible_end_abs);
    let match_end = location_match
        .range
        .end
        .clamp(visible_start_abs, visible_end_abs);
    let match_highlight = (
        match_start - visible_start_abs..match_end - visible_start_abs,
        match_style,
    );

    let highlights = gpui::combine_highlights(syntax_highlights, [match_highlight]);
    StyledText::new(line_text.to_string()).with_default_highlights(&text_style, highlights)
}
