# Plan: LSP result pickers with preview (references, definition, implementation, …)

## Goal

Present LSP navigation results (find-all-references and friends) in a **picker
modal with a preview pane** instead of a multibuffer tab. This lets you navigate
LSP results (explore → preview → jump) without cluttering the tab bar / buffer
switcher with multibuffer tabs.

Each LSP picker is **net-new** and exposes two actions:

- `open` — run a **fresh** LSP query and open the picker with the new results.
- `toggle` — show/hide the picker using the **prior** result set from the last
  search (does not necessarily re-run the query — see Open Decisions).

The same pattern applies to all the LSP location results:
references, definition, implementation, declaration, type-definition.

This builds entirely on the picker-preview machinery added in
zed-industries/zed#59604 ("Add Preview to Pickers and Make Them Resizable").
No new core picker infrastructure is required.

## Scope

In scope:

- A generic `LspLocationsPicker` modal + delegate, parameterized by kind.
- `open` / `toggle` action pairs per LSP picker kind.
- A workspace/global cache so `toggle` can reopen prior results.
- Routing find-all-references (and optionally the hover-link definition kinds)
  into the picker.

Explicitly out of scope (these were earlier "phases" we are NOT doing here):

- No `PickerPreviewSettings` settings schema / configurable preview defaults.
- No retrofitting of `tab_switcher` / `outline` / `project_symbols` previews.
- No `nonsearchable_*_with_preview` constructor.
- Keep the existing multibuffer presentation as the default / fallback; the
  picker is an alternative, not a replacement.

## Background: how things work today (anchors for the new thread)

All paths in `/home/david/code/dbkegley/zed`.

### Picker preview API (from #59604)

- `crates/picker/src/picker.rs`
  - `Picker<D>` has `preview: Option<Preview>`.
  - Construct with preview: `Picker::list_with_preview(delegate, project, window, cx)`
    (variable-height rows) or `Picker::uniform_list_with_preview(...)`.
  - Delegate hook (~line 282):
    ```rust
    fn try_get_preview_data_for_match(&self, _cx: &App) -> Option<PreviewUpdate> { None }
    fn preview_layout_changed(&mut self, _layout_is_horizontal: bool) {}
    ```
  - Initial layout is loaded from per-picker persistence and defaults to
    `Hidden` (picker.rs ~line 471). There is **no settings entry** for this.
    To open with preview visible by default, set the layout explicitly in the
    modal's `new()` (e.g. `set_preview_layout(Layout::Right, …)`).
- `crates/picker/src/preview.rs`
  - `PreviewUpdate { source: PreviewSource, match_location: Option<MatchLocation> }`
  - `PreviewSource::{ Path(PathBuf), Buffer(Entity<Buffer>), Message(HighlightedText) }`
  - `MatchLocation { anchor_range: Range<language::Anchor>, range: Range<usize> }`
  - Constructors: `PreviewUpdate::from_path`, `::from_buffer`, `::message`.
- Runtime layout actions (picker.rs ~line 57): `TogglePreview`,
  `SetPreviewRight`, `SetPreviewBelow`, `SetPreviewHidden`.

### Where LSP reference results are produced

- `crates/editor/src/navigation.rs`
  - `find_all_references` (~line 1239).
    - Raw results available at **~line 1291**: `Vec<Location>` from
      `project.references(...)`. **Tap here** — `Location` is buffer + anchor
      range, which is exactly what the picker preview wants.
    - Grouped into `HashMap<Entity<Buffer>, Vec<Range<Point>>>` at ~lines
      1294–1311 (anchors converted to points — too late for our needs).
    - Single-reference fast path (~lines 1326–1376) navigates directly without
      a multibuffer. **Reuse this as the picker's confirm/open-selected logic.**
    - Calls `open_locations_in_multibuffer(...)` at ~line 1399.
  - `open_locations_in_multibuffer` defined at ~line 2043.
- `Location` type: `crates/language/src/language.rs:229`
  ```rust
  pub struct Location { pub buffer: Entity<Buffer>, pub range: Range<Anchor> }
  ```
- Callers of `open_locations_in_multibuffer` (LSP ones to potentially intercept):
  - `navigation.rs:1399` — `find_all_references` (References).
  - `navigation.rs:1690` — `navigate_to_hover_links` (Definition / Implementation
    / Declaration / Type — branches on `GotoDefinitionKind`).
  - (Non-LSP callers `editor.rs:8617` and `bookmarks.rs:23` are out of scope.)
- Existing action field already plumbed: `FindAllReferences { always_open_multibuffer: bool }`
  at `crates/editor/src/actions.rs:948` (defaults `true`). Usable as the
  branch between multibuffer and picker, or supersede with new `open`/`toggle`
  actions.
- Single-location open mechanics to lift into a shared `open_location` helper
  (from navigation.rs ~1334–1375):
  - same buffer: `editor.go_to_singleton_buffer_range(range, window, cx)`
  - different buffer: `workspace.open_project_item(pane, buffer, …)` inside a
    `window.defer`.

### Cross-open state precedents

- **Text Finder** persists results by holding an `Entity<ProjectSearchView>`
  (`crates/search/src/text_finder/delegate.rs:61`) that survives modal dismissal;
  it reconnects via `hook_up_any_ongoing_search` (delegate.rs:247). This is a
  *live streaming* search (Connected/Disconnected states) — more complex than we
  need.
- **File Finder** caches no results; rebuilds from
  `workspace.recent_navigation_history()` (`file_finder.rs:120`) on each open.
- **Workspace/global state pattern**: project-search stores per-project options
  in a `Global` keyed by `WeakEntity<Project>`:
  `ActiveSettings(HashMap<WeakEntity<Project>, ProjectSearchSettings>)` at
  `crates/search/src/project_search.rs:107`, read via
  `cx.global::<ActiveSettings>()`. This is the idiomatic home for our results
  cache.
- **Modal open vs. reuse pattern**: `text_finder.rs:42` uses
  `workspace.active_modal::<Self>(cx)` to detect an already-open modal and cycle
  vs. open fresh. We extend this with a "cache populated?" branch.

### Modal boilerplate template (clone from Text Finder)

`crates/search/src/text_finder.rs` shows the minimal modal:

- struct wrapping `Entity<Picker<Delegate>>` + a `_subscription`.
- `new()` (lines 224–245): builds `Picker::list_with_preview`, subscribes to
  `DismissEvent`.
- impls `ModalView` (265), `EventEmitter<DismissEvent>` (275), `Focusable`
  (277), `Render` (render.rs).
- `open()` (206) spawns + `workspace.toggle_modal(...)`.

Note: our pickers are spawned **programmatically with results already in hand**,
so we do NOT need the `register`/`Toggle` action-keybinding wiring that Text
Finder uses; we need an `open(workspace, locations, title, …)` entry point plus
the open/toggle actions on the editor/workspace.

## Proposed design

### One generic picker for all LSP location kinds

References, definition, implementation, declaration, and type-definition all
produce `Vec<Location>` and present identically. Build **one**
`LspLocationsPicker` + delegate, parameterized by an `LspPickerKind`. Per-kind
differences are only: which `project.*` method to call, the modal title, and
which cache slot to use.

Delegate holds a flat `Vec<LocationMatch>`:

```rust
struct LocationMatch {
    buffer: Entity<Buffer>,
    anchor_range: Range<language::Anchor>,
    offset_range: Range<usize>, // for scroll; computed via to_offset on snapshot
    file_label: String,
    line_snippet: String,       // for the row, like text_finder rows
}
```

- Construct with `Picker::list_with_preview(delegate, project, window, cx)`
  (variable-height rows for grouped file headers + snippets).
- `try_get_preview_data_for_match` mirrors Text Finder (delegate.rs:787):
  ```rust
  Some(PreviewUpdate::from_buffer(
      m.buffer.clone(),
      MatchLocation { anchor_range: m.anchor_range.clone(), range: m.offset_range.clone() },
  ))
  ```
- `render_match` clones Text Finder's grouped-header + line-snippet rendering.
- `confirm` calls the shared `open_location` helper (lifted from the
  single-reference fast path), with split on secondary-confirm.
- In the modal `new()`, set preview layout visible by default (e.g. `Right`)
  since the whole point is preview, and there is no settings layer to do it.

### Actions: `open` and `toggle`

For each kind (using references as the example):

- `FindAllReferences::open`
  1. Run `project.references(...)` → `Vec<Location>`.
  2. Store a snapshot in the workspace cache slot for `References`.
  3. `toggle_modal(... LspLocationsPicker::new(locations, kind) ...)`.
- `FindAllReferences::toggle`
  1. If a `LspLocationsPicker` of this kind is already the active modal →
     **dismiss** it (true toggle).
  2. Else if the cache slot for `References` is populated → reopen the picker
     from the cached set (behavior re: re-query is an Open Decision below).
  3. Else → (choose) no-op, or fall back to `open`.

The other kinds (`GoToDefinition`, `GoToImplementation`, `GoToDeclaration`,
`GoToTypeDefinition`) get the same `open`/`toggle` pair, each reading/writing its
own cache slot and calling its own `project.*` query method.

### Results cache (so `toggle` can reopen prior results)

Add a `Global` modeled on `ActiveSettings`:

```rust
struct LspPickerResults(HashMap<(WeakEntity<Workspace>, LspPickerKind), CachedLspResults>);
impl Global for LspPickerResults {}

struct CachedLspResults {
    kind: LspPickerKind,
    title: String,
    locations: Vec<Location>, // Location holds Entity<Buffer> + Range<Anchor>
    // optionally: origin (buffer + anchor) of the query, for re-query mode
}
```

- Keyed per workspace and per kind (so references-toggle and
  implementation-toggle are independent — see Open Decisions).
- Holds **strong** `Entity<Buffer>` (inside `Location`) to keep referenced
  buffers alive; cache replacement / window close releases them. (Tradeoff:
  pins those buffers in memory until replaced.)

### Why anchors make this robust

`Location.range` is a `Range<Anchor>` (language.rs:229). Anchors track edits as
long as the buffer entity is alive, so a cached set reopened via `toggle` after
edits still points at the correct positions — **without** re-querying. Validate
or drop dead buffers on `toggle` (or rely on the strong refs above to keep them
alive).

### How this compares to existing pickers

| | File Finder | Text Finder | LSP pickers (this plan) |
|---|---|---|---|
| Result source | workspace nav history | streaming project search | one-shot LSP query |
| State across opens | none (rebuilt) | `ProjectSearchView` entity | cached `Vec<Location>` (Global) |
| `toggle` = prior results | no | effectively yes (stream) | yes, by design |
| Reconnect machinery | n/a | `hook_up_any_ongoing_search` | none (snapshot, not live) |
| Preview source | `from_path` | `from_buffer` + `MatchLocation` | `from_buffer` + `MatchLocation` |

Key point: these pickers are **simpler than Text Finder** on the state axis —
no live streaming search to pause/resume, just a cached snapshot to rebind.

## Open decisions (resolve before / early in implementation)

1. **Toggle behavior — cache vs. re-query (MUST DECIDE).**
   Two viable semantics for `toggle` when results exist:
   - **(a) Cache the results.** Reopen the picker bound to the cached
     `Vec<Location>`. Fast, no LSP round-trip. Anchors keep positions correct
     across edits, but newly-added/removed references since the last `open` are
     NOT reflected (stale set). Requires pinning buffers (strong refs).
   - **(b) Re-query using the previous search.** On `toggle`, re-run the LSP
     query from the cached origin (buffer + position/symbol) to get a fresh set.
     Always current, no stale results, no need to pin buffers — but costs an LSP
     round-trip and the origin position may have moved/become invalid.
   - Possible hybrid: cache by default, re-query if the cache is older than N
     seconds or if the origin buffer changed. Decide and document the chosen
     semantics; the cache struct already leaves room for an `origin` to support
     (b)/hybrid.

2. **Per-kind cache slots vs. a single shared "last LSP locations" slot.**
   Recommended: per-kind (independent toggles per kind, matches "each picker
   functions similarly"). Confirm.

3. **Cache scope key: per-workspace vs. per-project.** Per-workspace is likely
   more correct for multi-window. Confirm against how `ActiveSettings` keys by
   project.

4. **Which entry points route to the picker initially.** Minimal first cut:
   references only (one branch at navigation.rs:1399). Definition/implementation/
   etc. share `open_locations_in_multibuffer` at navigation.rs:1690 — same kind
   of branch, add after references lands.

5. **Action surface.** Reuse the existing `FindAllReferences.always_open_multibuffer`
   field to branch, or introduce explicit new `open`/`toggle` actions per kind?
   The two-action model in this plan implies new actions; decide how they
   coexist with `always_open_multibuffer` and the multibuffer default.

6. **Default preview layout for these pickers.** Set visible (`Right`) in
   `new()` since there is no settings layer. Confirm `Right` vs `Below`.

## Implementation steps

1. Lift `open_location(workspace, &Location, split, window, cx)` out of the
   single-reference fast path (navigation.rs ~1334–1375) into a reusable helper;
   make `find_all_references` use it.
2. Add the `LspLocationsPicker` modal + `Delegate` (clone Text Finder modal
   skeleton, drop the action-registration wiring). Implement `render_match`,
   `try_get_preview_data_for_match`, `confirm` (→ `open_location`), and the
   `ModalView`/`EventEmitter`/`Focusable`/`Render` impls. Set preview layout
   visible in `new()`.
3. Add row-building: from `Vec<Location>` compute `offset_range`
   (`range.to_offset(&snapshot)`), `file_label`, and `line_snippet` per location
   (one snapshot read per buffer).
4. Add the `LspPickerResults` global cache (+ `LspPickerKind`,
   `CachedLspResults`), with store on `open` and read on `toggle`.
5. Wire `FindAllReferences::{open,toggle}`: tap raw `Vec<Location>` at
   navigation.rs ~1291; branch to the picker; implement open/toggle per the flow
   above. Keep the multibuffer path as default/fallback.
6. Resolve Open Decision #1 (toggle cache-vs-requery) and implement accordingly.
7. Generalize to the other kinds (definition, implementation, declaration,
   type-definition), each: query method + title + cache slot, sharing everything
   else. Add the branch at navigation.rs:1690.
8. Tests + manual verification: open from a symbol, navigate with preview,
   confirm jumps correctly (same- and cross-buffer), toggle reopens prior set,
   toggle again dismisses, no multibuffer tab is created.

## Net footprint

One new modal module (smaller than `text_finder.rs` — no streaming/search
wiring), one extracted `open_location` helper, one `Global` results cache, and
small branches at navigation.rs:1291/1399 (and later :1690). No changes to core
picker infrastructure or settings.
