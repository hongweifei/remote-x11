//! Form pages: one declarative engine behind the four settings screens.
//!
//! The server, client, SSH and runtime screens differ only in which fields
//! they show, so they share this engine rather than four near-identical
//! files. A page is a `FormSpec` — sections of fields, each with a TOML key,
//! a label, a hint and an input kind — plus a lifecycle strip for the role
//! that can actually be started.
//!
//! Two rules shape the layout:
//!
//! * the field label and its input sit on one row, label in a fixed left
//!   column, so a long form can be scanned down the left edge;
//! * every value is edited in place and nothing is written until 保存 is
//!   pressed, with validation issues listed next to the button that would
//!   commit them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use fltk::prelude::*;

use crate::theme::{self as th, C, L, T};
use crate::ui::action::{emit_action, set_handler, ActionSlot};
use crate::ui::chrome::{card, card_title, page_header, PAGE_HEADER_H};
use crate::ui::controls::{BtnKind, Button, Field, Select, Switch};
use crate::ui::layout::{Axis, Insets, Layout, NodeId, Rect, Size, Solved};
use crate::ui::scrollbar::{Hit, ScrollBar};
use crate::ui::{Page, PageInput};

/// One field's live widgets, in creation order.
type WidgetList = Rc<RefCell<Vec<(&'static str, FieldWidget)>>>;
mod scroll;

use scroll::{apply_scroll, place_fields, row_fits, scroll_band, scroll_bar};

// ---------------------------------------------------------------------------
// Spec
// ---------------------------------------------------------------------------

/// How a field is edited. The kind also decides how the value is rendered
/// back into a string for the config file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldKind {
    /// Free text (addresses, tokens, paths).
    Text,
    /// Integer.
    Number,
    /// Decimal number.
    Float,
    /// On/off switch.
    Toggle,
    /// One of a fixed set; the value is the selected item's text.
    Select(&'static [&'static str]),
    /// A command line, edited as one line and split on whitespace.
    Argv,
}

#[derive(Clone, Copy, Debug)]
pub struct FieldSpec {
    /// TOML path, e.g. `server.listen`. The engine never parses it; the app
    /// maps keys to the config struct, which keeps this layer free of the
    /// config schema.
    pub key: &'static str,
    pub label: &'static str,
    /// One line under the input: what the value does and its unit.
    pub hint: &'static str,
    pub kind: FieldKind,
}

#[derive(Clone, Copy, Debug)]
pub struct SectionSpec {
    pub title: &'static str,
    /// Right-aligned note in the section header (usually the default value).
    pub note: &'static str,
    pub fields: &'static [FieldSpec],
}

#[derive(Clone, Copy, Debug)]
pub struct FormSpec {
    pub title: &'static str,
    pub subtitle: &'static str,
    pub sections: &'static [SectionSpec],
    /// Role this page can start/stop; `None` for runtime-only pages.
    pub role: Option<FormRole>,
    /// Label of the start button, when `role` is set.
    pub start_label: &'static str,
}

/// Which lifecycle control the page carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FormRole {
    Server,
    Client,
    Ssh,
}

/// Everything a form page can ask the app to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormAction {
    Save,
    Reload,
    ResetDefaults,
    Start,
    Stop,
    /// A toggle or drop-down changed; the app re-reads `values()`.
    Changed(String),
    /// Open a file picker for a path-like field (`ssh.identity`).
    PickFile(&'static str),
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

const ROW_H: i32 = 52;
const SECTION_HEAD: i32 = 46;
const SECTION_GAP: i32 = 16;
const LABEL_COL: i32 = 190;
const SIDE_W: i32 = 300;

/// Where one field row lives, relative to the page.
#[derive(Clone, Copy, Debug)]
struct RowGeom {
    key: &'static str,
    /// Top of the input box.
    input_y: i32,
    input_h: i32,
    /// Caption baseline.
    label_y: i32,
    hint_y: i32,
    /// Row top, for hover and separators.
    row_y: i32,
}

/// Where one section card lives.
#[derive(Clone, Copy, Debug)]
struct SectionGeom {
    index: usize,
    y: i32,
    h: i32,
}

/// Compute every rectangle for a form at a given scroll offset.
///
/// Drawing and widget placement both consume this, so they cannot drift apart:
/// the tree is solved once and reshaped here for the callers. Scrolling is a
/// translation of the solved rectangles, not a second layout.
fn geometry(
    spec: &FormSpec,
    x: i32,
    y: i32,
    w: i32,
    _h: i32,
    scroll: i32,
) -> (Vec<SectionGeom>, Vec<RowGeom>) {
    let solved = form_layout(spec, x, y, w, scroll);
    let sections = solved
        .cards
        .iter()
        .enumerate()
        .map(|(index, &id)| {
            let r = solved.solved.get(id);
            SectionGeom {
                index,
                y: r.y,
                h: r.h,
            }
        })
        .collect();
    let rows = solved
        .rows
        .iter()
        .map(|&(key, kind, id)| {
            let ry = solved.solved.get(id).y;
            RowGeom {
                key,
                input_y: ry,
                input_h: if kind == FieldKind::Toggle { 20 } else { 32 },
                label_y: ry + 6,
                // The hint hugs the input it belongs to. It used to sit at
                // `fy + 36`, which put 18 device px between the input and the
                // hint but only 11 between the hint and the *next* row — so the
                // hint read as belonging to the row below it. `+31` flips that:
                // a small gap above, a larger one below.
                hint_y: ry + 31,
                row_y: ry,
            }
        })
        .collect();
    (sections, rows)
}

/// A solved form: the rectangles plus which leaf each section and row came from.
struct SolvedForm {
    solved: Solved,
    cards: Vec<NodeId>,
    rows: Vec<(&'static str, FieldKind, NodeId)>,
    /// Bottom of the scrollable content: the last card's bottom plus the gap
    /// that would follow another section. The hand-written version summed the
    /// same trailing gap, and the scroll range must not shrink by it.
    content_bottom: i32,
}

/// The form's layout tree: heading, the scrolling field column, the side column.
///
/// The only place a form's coordinates are decided. `geometry` reshapes this for
/// its callers; `place_fields` and the pointer handler read the same rectangles.
///
/// The field column is solved against a tall virtual page (a form is taller than
/// the window), then translated by `scroll`. Sections are separated by
/// [`SECTION_GAP`] *between* cards only — the first card starts flush under the
/// heading, which is what the hand-written version did.
fn form_layout(spec: &FormSpec, x: i32, y: i32, w: i32, scroll: i32) -> SolvedForm {
    let page = Rect::new(x, y, w, PAGE_HEADER_H + FORM_VIRTUAL_H);
    let mut layout = Layout::column().pad(0, Insets::all(L::PAD));
    let _header = layout.leaf(0, Size::Fixed(PAGE_HEADER_H));
    let body = layout.stack(0, Axis::Row, Size::Fill(1));
    layout = layout.gap(body, L::GAP);
    let column = layout.stack(body, Axis::Column, Size::Fill(1));

    let mut cards = Vec::new();
    let mut rows = Vec::new();
    for (index, s) in spec.sections.iter().enumerate() {
        let card_h = SECTION_HEAD + s.fields.len() as i32 * ROW_H + 12;
        let card = layout.leaf(column, Size::Fixed(card_h));
        if index > 0 {
            layout = layout.lead(card, SECTION_GAP);
        }
        let inner = layout.stack(card, Axis::Column, Size::Fill(1));
        layout = layout.pad(inner, Insets::xy(18, 0));
        let _head = layout.leaf(inner, Size::Fixed(SECTION_HEAD));
        for f in s.fields {
            let id = layout.leaf(inner, Size::Fixed(ROW_H));
            rows.push((f.key, f.kind, id));
        }
        cards.push(card);
    }
    // The side column is a sibling of the field column, so it does not scroll.
    let _side = layout.stack(body, Axis::Column, Size::Fixed(SIDE_W));

    let solved = layout.solve(page);
    let content_bottom = cards
        .last()
        .map(|&id| solved.get(id).bottom() + SECTION_GAP)
        .unwrap_or_else(|| solved.get(_header).bottom());
    let mut solved = solved;
    solved.shift(-scroll);
    SolvedForm {
        solved,
        cards,
        rows,
        content_bottom: content_bottom + scroll,
    }
}

/// Height the field column is solved against. A form is taller than any window,
/// so the tree is laid out at its natural height and the visible window is taken
/// by clipping; a value larger than any real form is enough.
const FORM_VIRTUAL_H: i32 = 4096;

fn page_y_start(page_y: i32, pad: i32) -> i32 {
    // Heading block (64) + rule + 20px of air, matching `page_header`.
    page_y + pad + 64 + 20
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

/// One field's live widgets, whichever kind it is.
enum FieldWidget {
    Text(Field),
    Toggle(Switch),
    Select(Select),
}

struct FormState {
    values: HashMap<String, String>,
    /// Keys the user has edited since the last save/reload.
    dirty: Vec<String>,
    issues: Vec<(String, String)>,
    scroll: i32,
    /// Total content height, for clamping the scroll offset.
    content_h: i32,
    /// Viewport height.
    view_h: i32,
    hover_row: Option<usize>,
    /// While dragging the scrollbar: how far below the thumb's top edge the
    /// pointer grabbed it. `None` when not dragging.
    thumb_grab: Option<i32>,
}

/// A settings page: a scrolling field form on the left, an action column on
/// the right (save/reload/reset, validation issues, lifecycle controls).
pub struct FormPage {
    page: Page,
    save: Button,
    reload: Button,
    reset: Button,
    /// Lifecycle pair, present only on pages that own a role (the runtime
    /// page has none — a start button there would have to guess which end it
    /// meant).
    start: Option<Button>,
    stop: Option<Button>,
    pick_file: Option<Button>,
    spec: &'static FormSpec,
    st: Rc<RefCell<FormState>>,
    widgets: WidgetList,
    on_action: ActionSlot<FormAction>,
    rect: Rc<RefCell<(i32, i32, i32, i32)>>,
}

impl FormPage {
    pub fn new(spec: &'static FormSpec, x: i32, y: i32, w: i32, h: i32) -> Self {
        // Group first, children inside begin()/end(): the group paints the
        // form background, FLTK draws the inputs and buttons on top of it.
        let st = Rc::new(RefCell::new(FormState {
            values: HashMap::new(),
            dirty: Vec::new(),
            issues: Vec::new(),
            scroll: 0,
            content_h: 0,
            view_h: h,
            hover_row: None,
            thumb_grab: None,
        }));
        let on_action: ActionSlot<FormAction> = Rc::new(RefCell::new(None));
        let rect = Rc::new(RefCell::new((x, y, w, h)));
        let widgets: Rc<RefCell<Vec<(&'static str, FieldWidget)>>> =
            Rc::new(RefCell::new(Vec::new()));

        // Background first: FLTK draws a group's children in creation order.
        let page = {
            let st_draw = st.clone();
            let rect_draw = rect.clone();
            Page::new(x, y, w, h, C::BG, move |px, py, pw, ph| {
                let s = st_draw.borrow();
                let (x0, _, _, _) = *rect_draw.borrow();
                paint(
                    spec,
                    Rect {
                        x: px,
                        y: py,
                        w: pw,
                        h: ph,
                    },
                    &s,
                    x0,
                );
            })
        };

        for section in spec.sections {
            for f in section.fields {
                let kind = f.kind;
                let key = f.key;
                let widget = match kind {
                    FieldKind::Toggle => {
                        let sw = Switch::new(0, 0, false);
                        {
                            let st_c = st.clone();
                            let act = on_action.clone();
                            sw.on_change(move |on| {
                                st_c.borrow_mut().values.insert(
                                    key.to_string(),
                                    if on { "true" } else { "false" }.to_string(),
                                );
                                mark_dirty(&st_c, key);
                                emit_action(&act, FormAction::Changed(key.to_string()));
                            });
                        }
                        FieldWidget::Toggle(sw)
                    }
                    FieldKind::Select(items) => {
                        let sel = Select::new(0, 0, 10, 10, items, 0);
                        {
                            let st_c = st.clone();
                            let act = on_action.clone();
                            let items_owned: Vec<String> =
                                items.iter().map(|s| (*s).to_string()).collect();
                            sel.on_change(move || {
                                let idx = {
                                    // The callback fires without telling us
                                    // the index; read it back via the value
                                    // cached by `Select::value` on demand.
                                    st_c.borrow()
                                        .values
                                        .get(key)
                                        .and_then(|v| items_owned.iter().position(|i| i == v))
                                };
                                let _ = idx;
                                mark_dirty(&st_c, key);
                                emit_action(&act, FormAction::Changed(key.to_string()));
                            });
                        }
                        FieldWidget::Select(sel)
                    }
                    _ => {
                        let fld = Field::new(0, 0, 10, 10, "");
                        {
                            let st_c = st.clone();
                            let mut input = fld.input();
                            input.set_callback(move |i| {
                                st_c.borrow_mut().values.insert(key.to_string(), i.value());
                                mark_dirty(&st_c, key);
                            });
                        }
                        FieldWidget::Text(fld)
                    }
                };
                widgets.borrow_mut().push((key, widget));
            }
        }

        let save = Button::new(0, 0, 10, 10, "保存", BtnKind::Primary);
        let reload = Button::new(0, 0, 10, 10, "重新载入", BtnKind::Secondary);
        let reset = Button::new(0, 0, 10, 10, "恢复默认", BtnKind::Ghost);
        let start = spec
            .role
            .map(|_| Button::new(0, 0, 10, 10, spec.start_label, BtnKind::Primary));
        let stop = spec
            .role
            .map(|_| Button::new(0, 0, 10, 10, "停止", BtnKind::Danger));
        let pick_file = if spec
            .sections
            .iter()
            .any(|s| s.fields.iter().any(|f| f.key.ends_with("identity")))
        {
            Some(Button::new(0, 0, 10, 10, "选择文件…", BtnKind::Ghost))
        } else {
            None
        };

        {
            let act = on_action.clone();
            save.on_click(move || emit_action(&act, FormAction::Save));
        }
        {
            let act = on_action.clone();
            reload.on_click(move || emit_action(&act, FormAction::Reload));
        }
        {
            let act = on_action.clone();
            reset.on_click(move || emit_action(&act, FormAction::ResetDefaults));
        }
        if let Some(start) = &start {
            let act = on_action.clone();
            start.on_click(move || emit_action(&act, FormAction::Start));
        }
        if let Some(stop) = &stop {
            let act = on_action.clone();
            stop.on_click(move || emit_action(&act, FormAction::Stop));
        }
        if let Some(p) = &pick_file {
            let act = on_action.clone();
            p.on_click(move || emit_action(&act, FormAction::PickFile("ssh.identity")));
        }

        page.finish();

        let page = Self {
            page,
            save,
            reload,
            reset,
            start,
            stop,
            pick_file,
            spec,
            st,
            widgets,
            on_action,
            rect,
        };
        page.layout(x, y, w, h);
        page.install_input();
        page
    }

    pub fn on_action<F: Fn(FormAction) + 'static>(&self, f: F) {
        set_handler(&self.on_action, f);
    }

    /// Load values into the form. Unknown keys in `values` are ignored; keys
    /// the form does not know about simply stay untouched.
    pub fn set_values(&self, values: &[(String, String)]) {
        {
            let mut s = self.st.borrow_mut();
            for (k, v) in values {
                s.values.insert(k.clone(), v.clone());
            }
            s.dirty.clear();
        }
        for (key, w) in self.widgets.borrow().iter() {
            let v = self
                .st
                .borrow()
                .values
                .get(*key)
                .cloned()
                .unwrap_or_default();
            match w {
                FieldWidget::Text(f) => f.set_value(&v),
                FieldWidget::Toggle(sw) => sw.set_value(v == "true"),
                FieldWidget::Select(sel) => {
                    if let Some(idx) = sel.items().iter().position(|i| *i == v) {
                        sel.set_value(idx);
                    }
                }
            }
        }
        self.page.redraw();
    }

    /// All field values as strings, in spec order.
    pub fn values(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for section in self.spec.sections {
            for f in section.fields {
                // Selects report their live index; text and toggles report
                // what the widgets hold, so a value typed but not yet synced
                // is still saved.
                let v = match self
                    .widgets
                    .borrow()
                    .iter()
                    .find(|(k, _)| *k == f.key)
                    .map(|(_, w)| w)
                {
                    Some(FieldWidget::Text(fld)) => fld.value(),
                    Some(FieldWidget::Toggle(sw)) => {
                        if sw.value() {
                            "true".into()
                        } else {
                            "false".into()
                        }
                    }
                    Some(FieldWidget::Select(sel)) => {
                        sel.items().get(sel.value()).cloned().unwrap_or_default()
                    }
                    None => String::new(),
                };
                out.push((f.key.to_string(), v));
            }
        }
        out
    }

    /// Validation issues from the last save attempt, shown beside the button.
    pub fn set_issues(&self, issues: Vec<(String, String)>) {
        self.st.borrow_mut().issues = issues;
        self.page.redraw();
    }

    /// Enable/disable the lifecycle controls according to whether this role
    /// is currently running.
    pub fn set_running(&self, running: bool) {
        if let (Some(start), Some(stop)) = (&self.start, &self.stop) {
            start.set_enabled(!running);
            stop.set_enabled(running);
        }
    }

    /// Hiding the group hides every field and button inside it, so page
    /// switching cannot leave a control stranded on another page.
    pub fn show(&self) {
        {
            let mut g = self.page.widget();
            g.show();
        }
        // Field visibility also depends on the scroll offset, which the
        // placement pass re-applies; the full repaint at the end is what makes
        // the inputs actually paint their values.
        self.place_fields();
        self.page.redraw();
    }

    /// Repaint the page surface and everything on it.
    pub fn redraw(&self) {
        self.page.redraw();
    }

    pub fn hide(&self) {
        let mut g = self.page.widget();
        g.hide();
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        *self.rect.borrow_mut() = (x, y, w, h);
        self.layout(x, y, w, h);
    }

    fn layout(&self, x: i32, y: i32, w: i32, h: i32) {
        self.page.resize(x, y, w, h);
        {
            let mut s = self.st.borrow_mut();
            s.view_h = h - (page_y_start(y, L::PAD) - y);
            // Content height comes from the layout tree, not from a second sum
            // over the spec: the tree already knows how tall the column is, and
            // two formulas for the same thing is how the scroll range drifts.
            let content = form_layout(self.spec, x, y, w, 0);
            s.content_h = content.content_bottom - page_y_start(y, L::PAD);
            let max = (s.content_h - s.view_h).max(0);
            s.scroll = s.scroll.clamp(0, max);
        }

        let pad = L::PAD;
        let content_x = x + pad;
        let content_w = w - pad * 2;
        let side_x = content_x + content_w - SIDE_W;
        let sy = page_y_start(y, pad) + 4;
        let half = (SIDE_W - 8) / 2;
        self.save.resize(side_x, sy, half, 32);
        self.reload.resize(side_x + half + 8, sy, half, 32);
        self.reset.resize(side_x, sy + 40, SIDE_W, 26);
        if let Some(p) = &self.pick_file {
            // Sits on the path field it fills, right-aligned inside it.
            let scroll = self.st.borrow().scroll;
            let (_, rows) = geometry(self.spec, x, y, w, h, scroll);
            if let Some(g) = rows.iter().find(|r| r.key.ends_with("identity")) {
                let input_w = (content_w - SIDE_W - L::GAP) - 36 - LABEL_COL;
                let input_x = content_x + 18 + LABEL_COL;
                p.resize(input_x + input_w - 104, g.input_y + 4, 104, 26);
            }
        }

        // Lifecycle pair sits at the bottom of the side column: the start
        // button is the page's primary action when nothing is running.
        let by = y + h - L::PAD - 32;
        if let (Some(start), Some(stop)) = (&self.start, &self.stop) {
            start.resize(side_x, by, half, 32);
            stop.resize(side_x + half + 8, by, half, 32);
        }

        self.place_fields();
    }

    fn place_fields(&self) {
        place_fields(self.spec, &self.widgets, &self.st, &self.rect);
    }

    /// Wheel, track-click and thumb-drag input for the field column.
    ///
    /// The form is taller than the window on the runtime page (20 fields), and
    /// `scroll` was already threaded through `geometry`/`place_fields` — only
    /// the input was missing, so everything below the fold used to be
    /// unreachable. A wheel is invisible, so the bar is both the affordance and
    /// the second way to use it: drag the thumb, or click the track to page.
    fn install_input(&self) {
        let spec = self.spec;
        let widgets = self.widgets.clone();
        let st = self.st.clone();
        let rect = self.rect.clone();
        let page = self.page.clone();

        self.page.on_input(move |input| {
            let page_rect = {
                let r = *rect.borrow();
                Rect {
                    x: r.0,
                    y: r.1,
                    w: r.2,
                    h: r.3,
                }
            };
            let bar = scroll_bar(page_rect, &st.borrow());
            let scrollable = bar.is_some();
            match input {
                PageInput::Wheel(dy) => {
                    if dy == 0 || !scrollable {
                        return false;
                    }
                    let cur = st.borrow().scroll;
                    // A notch is FLTK's `dy`; move about one and a half rows.
                    apply_scroll(spec, &widgets, &st, &rect, &page, cur + dy * ROW_H * 3 / 2);
                    true
                }
                PageInput::Push(px, py) => {
                    let Some(bar) = bar else { return false };
                    // `PageInput` is page-relative; the bar lives in the same
                    // absolute space as the painters.
                    match bar.hit(px + page_rect.x, py + page_rect.y) {
                        Some(Hit::Thumb { grab }) => {
                            st.borrow_mut().thumb_grab = Some(grab);
                            page.redraw();
                            true
                        }
                        Some(Hit::Track { dir }) => {
                            // Click above/below the thumb pages by one viewport,
                            // the usual scrollbar convention.
                            let (cur, view) = {
                                let s = st.borrow();
                                (s.scroll, s.view_h)
                            };
                            apply_scroll(spec, &widgets, &st, &rect, &page, cur + dir * view);
                            true
                        }
                        None => false,
                    }
                }
                PageInput::Drag(_px, py) => {
                    let Some(grab) = st.borrow().thumb_grab else {
                        return false;
                    };
                    let Some(bar) = bar else { return false };
                    apply_scroll(spec, &widgets, &st, &rect, &page, bar.scroll_at(py - grab));
                    true
                }
                PageInput::Release => {
                    let was = st.borrow_mut().thumb_grab.take();
                    if was.is_some() {
                        page.redraw();
                    }
                    was.is_some()
                }
            }
        });
    }
}

/// Reposition the field widgets after a scroll without touching the rest of
/// the layout.
fn mark_dirty(st: &Rc<RefCell<FormState>>, key: &str) {
    let mut s = st.borrow_mut();
    if !s.dirty.iter().any(|k| k == key) {
        s.dirty.push(key.to_string());
    }
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

/// One page rectangle, so the paint helpers take geometry as a unit.
///
/// This *is* [`crate::ui::layout::Rect`] — the type the layout tree hands out —
/// so a rectangle never has to be converted between two shapes.
type PageRect = Rect;

fn paint(spec: &FormSpec, rect: PageRect, s: &FormState, origin_x: i32) {
    let (x, y, w, h) = (rect.x, rect.y, rect.w, rect.h);
    th::fill(x, y, w, h, C::BG);
    let pad = L::PAD;
    let content_x = x + pad;
    let content_w = w - pad * 2;
    page_header(content_x, y + pad, content_w, spec.title, spec.subtitle);

    let (sections, rows) = geometry(spec, origin_x, y, w, h, s.scroll);

    // The scrolling region must not bleed over the page header or the side
    // column; clip once and draw sections inside it. Rows that would be cut
    // by the viewport edge are skipped rather than drawn half-visible — a
    // clipped line of Chinese text reads as a bug, not as "scroll for more".
    let (band_x, clip_top, band_w, band_h) = scroll_band(x, y, w, h);
    let clip_bottom = clip_top + band_h;
    th::push_clip(band_x, clip_top, band_w, band_h);

    for sec in &sections {
        let section = &spec.sections[sec.index];
        th::fill_round(
            content_x,
            sec.y,
            content_w - SIDE_W - L::GAP,
            sec.h,
            L::RADIUS,
            C::PANEL,
        );
        th::stroke_round(
            content_x,
            sec.y,
            content_w - SIDE_W - L::GAP,
            sec.h,
            L::RADIUS,
            C::LINE,
        );
        let (ix, iy, iw) = card(content_x, sec.y, content_w - SIDE_W - L::GAP, sec.h);
        card_title(ix, iy, iw, section.title, Some(section.note));

        for f in section.fields {
            let Some(g) = rows.iter().find(|r| r.key == f.key) else {
                continue;
            };
            // A row is painted only when the whole row fits: a half-visible
            // line of Chinese reads as a bug, and its input widget cannot be
            // clipped by the canvas clip anyway.
            if !row_fits(g, clip_top, clip_bottom) {
                continue;
            }
            let hovered = s
                .hover_row
                .is_some_and(|i| rows[i].key == f.key && rows[i].row_y == g.row_y);
            if hovered {
                th::fill_round(
                    ix - 8,
                    g.row_y - 6,
                    iw + 16,
                    ROW_H - 6,
                    L::RADIUS,
                    th::mix(C::RAISED, C::PANEL, 0.55),
                );
            }
            // Label column, then the hint under the input. The label and its
            // dirty marker are both laid out from one line box.
            let dirty = s.dirty.iter().any(|k| k == f.key);
            // The label's baseline must not move: it used to be drawn into a
            // 20-unit row, so the line box is centred in that same row.
            let label = th::LineBox::ui(ix, g.label_y, LABEL_COL - 16, 20, T::SMALL);
            th::line_text(
                f.label,
                &label,
                th::ui_font(),
                if dirty { C::TEXT } else { C::TEXT_2 },
            );
            if dirty {
                th::dot(ix + LABEL_COL - 8, label.center_y(), 2, C::AMBER);
            }
            th::micro(
                f.hint,
                ix + LABEL_COL,
                g.hint_y + 2,
                iw - LABEL_COL,
                14,
                C::TEXT_3,
            );
            // Text inputs are widgets; outline them here so the field reads
            // as a control even before it takes focus.
            if matches!(
                f.kind,
                FieldKind::Text | FieldKind::Number | FieldKind::Float | FieldKind::Argv
            ) {
                let fx = ix + LABEL_COL;
                let fw = iw - LABEL_COL;
                th::stroke_round(fx, g.input_y, fw, 32, 4, C::LINE_HI);
            }
        }
    }
    th::pop_clip();

    // `content_x`/`content_w` are only needed for the side column now; the bar
    // derives its own geometry from the page rect.
    let _ = (content_x, content_w);
    if let Some(bar) = scroll_bar(rect, s) {
        bar.paint(s.thumb_grab.is_some());
    }
    paint_side(spec, rect, s, content_x, content_w);
}

fn paint_side(spec: &FormSpec, rect: Rect, s: &FormState, content_x: i32, content_w: i32) {
    let side_x = content_x + content_w - SIDE_W;
    let top = page_y_start(rect.y, L::PAD) + 4;

    // Validation / unsaved state.
    let card_y = top + 78;
    let card_h = 118;
    th::fill_round(side_x, card_y, SIDE_W, card_h, L::RADIUS, C::PANEL);
    th::stroke_round(side_x, card_y, SIDE_W, card_h, L::RADIUS, C::LINE);
    let (ix, iy, iw) = card(side_x, card_y, SIDE_W, card_h);

    if s.issues.is_empty() {
        card_title(ix, iy, iw, "校验", Some("通过"));
        if s.dirty.is_empty() {
            th::small("没有未保存的修改", ix, iy + 30, iw, 18, C::TEXT_3);
        } else {
            th::small(
                &format!("{} 项已修改，尚未写入配置文件", s.dirty.len()),
                ix,
                iy + 30,
                iw,
                18,
                C::AMBER,
            );
        }
        th::micro(
            "保存会保留文件里的注释与未知键",
            ix,
            iy + 56,
            iw,
            14,
            C::TEXT_3,
        );
    } else {
        card_title(
            ix,
            iy,
            iw,
            "需要修正",
            Some(&format!("{} 项", s.issues.len())),
        );
        let mut ly = iy + 28;
        for (field, msg) in s.issues.iter().take(3) {
            th::micro(field, ix, ly, iw, 14, C::BAD);
            let shown = th::ellipsize(msg, iw, th::ui_font(), T::SMALL);
            th::small(&shown, ix, ly + 14, iw, 18, C::TEXT_2);
            ly += 34;
        }
    }

    // What this page edits, in plain words: the config path and the fact that
    // both ends must agree on runtime knobs.
    let note_y = card_y + card_h + L::GAP;
    th::micro("作用范围", side_x, note_y, SIDE_W, 14, C::TEXT_3);
    let note = match spec.role {
        Some(FormRole::Server) => "只在远端主机上生效；客户端需要用同一令牌",
        Some(FormRole::Client) => "只在本机生效；中继地址要与远端监听一致",
        Some(FormRole::Ssh) => "只在本机生效；隧道建好后端口由本机转发",
        None => "两端都要自行设置；不一致时按各自默认值协商",
    };
    th::small(note, side_x, note_y + 16, SIDE_W, 36, C::TEXT_2);
}

#[cfg(test)]
mod tests {
    use super::*;

    static FIELDS: &[FieldSpec] = &[
        FieldSpec {
            key: "server.listen",
            label: "监听",
            hint: "IP:PORT",
            kind: FieldKind::Text,
        },
        FieldSpec {
            key: "server.x11_port",
            label: "X11 端口",
            hint: "默认 6000",
            kind: FieldKind::Number,
        },
    ];
    static SECTIONS: &[SectionSpec] = &[SectionSpec {
        title: "服务端",
        note: "默认",
        fields: FIELDS,
    }];
    static SPEC: FormSpec = FormSpec {
        title: "服务端",
        subtitle: "测试用",
        sections: SECTIONS,
        role: Some(FormRole::Server),
        start_label: "启动中继",
    };

    #[test]
    fn geometry_stacks_sections_without_gaps() {
        let (sections, rows) = geometry(&SPEC, 0, 0, 1000, 700, 0);
        assert_eq!(sections.len(), 1);
        assert_eq!(rows.len(), 2);
        // Second row is exactly one row height below the first.
        assert_eq!(rows[1].row_y - rows[0].row_y, ROW_H);
        // Section is tall enough for its head plus every row.
        assert_eq!(sections[0].h, SECTION_HEAD + 2 * ROW_H + 12);
    }

    #[test]
    fn geometry_offsets_follow_scroll() {
        let (_, a) = geometry(&SPEC, 0, 0, 1000, 700, 0);
        let (_, b) = geometry(&SPEC, 0, 0, 1000, 700, 40);
        assert_eq!(a[0].row_y - b[0].row_y, 40);
    }

    /// The scrollable content height must equal the shipped sum: every card plus
    /// the gap that follows it. The first version of the tree omitted the
    /// trailing gap, which silently shrank the scroll range by `SECTION_GAP`.
    #[test]
    fn content_height_matches_the_shipped_formula() {
        let shipped: i32 = SPEC
            .sections
            .iter()
            .map(|sec| SECTION_HEAD + sec.fields.len() as i32 * ROW_H + 12 + SECTION_GAP)
            .sum();
        let solved = form_layout(&SPEC, 0, 0, 1000, 0);
        assert_eq!(
            solved.content_bottom - page_y_start(0, L::PAD),
            shipped,
            "content height is the sum of the cards plus their gaps"
        );
    }

    /// Every row and card must sit inside the column the form scrolls, and the
    /// rows must be exactly `ROW_H` apart.
    #[test]
    fn rows_tile_the_section_without_drift() {
        let solved = form_layout(&SPEC, 0, 0, 1000, 0);
        let card = solved.solved.get(solved.cards[0]);
        let tops: Vec<i32> = solved
            .rows
            .iter()
            .map(|&(_, _, id)| solved.solved.get(id).y)
            .collect();
        assert_eq!(tops[1] - tops[0], ROW_H);
        assert_eq!(tops[0], card.y + SECTION_HEAD, "rows start under the head");
        assert_eq!(
            solved.solved.get(solved.rows.last().unwrap().2).bottom(),
            card.bottom() - 12,
            "the last row leaves the card's bottom air"
        );
    }
}
