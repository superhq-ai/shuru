//! The retained shell UI: nested nodes laid out by taffy, painted by their
//! content, and described to agents through accesskit. One tree drives
//! layout, hit testing, keyboard selection, paint and `GET /tree`.

pub mod kit;
pub(crate) mod painter;
pub mod text;
pub mod widgets;

use accesskit::{Action, NodeId, Role};
use serde_json::{json, Value};
use taffy::prelude::{
    AlignItems, Dimension, FlexDirection, LengthPercentage, LengthPercentageAuto, Size,
    TaffyMaxContent, TaffyTree,
};

use crate::draw::{Canvas, Font, Rect};
use widgets::{Content, Mark, Paint};

pub struct Node {
    pub id: NodeId,
    /// What agents see; presentational nodes have none.
    pub semantic: Option<accesskit::Node>,
    pub style: Style,
    pub content: Content,
    children: Vec<usize>,
}

impl Node {
    pub fn new(content: Content, style: Style) -> Self {
        Self {
            id: NodeId(0),
            semantic: None,
            style,
            content,
            children: Vec::new(),
        }
    }

    /// A node agents see, with the actions its role supports.
    pub fn semantic(mut self, id: u64, role: Role, name: &str) -> Self {
        let mut s = accesskit::Node::new(role);
        // Static text is named by its value on the accessibility bus.
        if role == Role::Label {
            s.set_value(name);
        } else {
            s.set_label(name);
        }
        match role {
            Role::Button
            | Role::RadioButton
            | Role::Switch
            | Role::ComboBox
            | Role::ListBoxOption => s.add_action(Action::Click),
            Role::MenuItem | Role::MenuItemCheckBox => {
                s.add_action(Action::Click);
                s.add_action(Action::Focus);
            }
            Role::TextInput | Role::SearchInput => {
                s.add_action(Action::Focus);
                s.add_action(Action::SetValue);
            }
            _ => {}
        }
        self.id = NodeId(id);
        self.semantic = Some(s);
        self
    }
}

/// How a node lays out: the part of CSS flexbox the shell uses. Kept apart
/// from taffy styles, which are not `Send`, and turned into one for layout.
#[derive(Clone, Debug, Default)]
pub struct Style {
    position: Option<(f32, f32)>,
    column: bool,
    center: bool,
    grow: bool,
    width: Option<f32>,
    height: Option<f32>,
    min_width: Option<f32>,
    max_width: Option<f32>,
    pad: [f32; 4],
    margin: [f32; 4],
}

pub fn style() -> Style {
    Style::default()
}

impl Style {
    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.position = Some((x, y));
        self
    }

    pub fn row(mut self) -> Self {
        self.column = false;
        self
    }
    pub fn column(mut self) -> Self {
        self.column = true;
        self
    }
    /// Centers the children across the main axis.
    pub fn center(mut self) -> Self {
        self.center = true;
        self
    }
    pub fn width(mut self, w: f32) -> Self {
        self.width = Some(w);
        self
    }
    pub fn height(mut self, h: f32) -> Self {
        self.height = Some(h);
        self
    }
    pub fn min_width(mut self, w: f32) -> Self {
        self.min_width = Some(w);
        self
    }
    pub fn max_width(mut self, w: f32) -> Self {
        self.max_width = Some(w);
        self
    }
    /// Takes the room left in its row, and gives it up first.
    pub fn grow(mut self) -> Self {
        self.grow = true;
        self
    }
    pub fn pad(mut self, top: f32, right: f32, bottom: f32, left: f32) -> Self {
        self.pad = [top, right, bottom, left];
        self
    }
    pub fn margin(mut self, top: f32, right: f32, bottom: f32, left: f32) -> Self {
        self.margin = [top, right, bottom, left];
        self
    }

    fn taffy(&self) -> taffy::Style {
        let auto = |v: Option<f32>| v.map_or(Dimension::auto(), Dimension::length);
        let edge =
            |v: Option<f32>| v.map_or(LengthPercentageAuto::auto(), LengthPercentageAuto::length);
        let [t, r, b, l] = self.pad;
        let [mt, mr, mb, ml] = self.margin;
        taffy::Style {
            position: if self.position.is_some() {
                taffy::Position::Absolute
            } else {
                taffy::Position::Relative
            },
            inset: taffy::Rect {
                left: edge(self.position.map(|p| p.0)),
                top: edge(self.position.map(|p| p.1)),
                right: LengthPercentageAuto::auto(),
                bottom: LengthPercentageAuto::auto(),
            },
            flex_direction: if self.column {
                FlexDirection::Column
            } else {
                FlexDirection::Row
            },
            align_items: if self.center {
                AlignItems::CENTER
            } else {
                AlignItems::NORMAL
            },
            size: Size {
                width: auto(self.width),
                height: auto(self.height),
            },
            min_size: Size {
                width: edge(self.min_width.or(self.grow.then_some(0.0))),
                height: LengthPercentageAuto::auto(),
            },
            max_size: Size {
                width: edge(self.max_width),
                height: LengthPercentageAuto::auto(),
            },
            flex_grow: if self.grow { 1.0 } else { 0.0 },
            flex_shrink: if self.grow { 1.0 } else { 0.0 },
            padding: taffy::Rect {
                top: LengthPercentage::length(t),
                right: LengthPercentage::length(r),
                bottom: LengthPercentage::length(b),
                left: LengthPercentage::length(l),
            },
            margin: taffy::Rect {
                top: LengthPercentageAuto::length(mt),
                right: LengthPercentageAuto::length(mr),
                bottom: LengthPercentageAuto::length(mb),
                left: LengthPercentageAuto::length(ml),
            },
            ..Default::default()
        }
    }
}

/// Builds a tree under a root node.
pub struct Builder {
    nodes: Vec<Node>,
    rows: Vec<usize>,
    form: bool,
}

impl Builder {
    pub fn new(root: Node) -> Self {
        Self {
            nodes: vec![root],
            rows: Vec::new(),
            form: false,
        }
    }

    pub const ROOT: usize = 0;

    pub fn add(&mut self, parent: usize, node: Node) -> usize {
        self.nodes.push(node);
        let i = self.nodes.len() - 1;
        self.nodes[parent].children.push(i);
        i
    }

    /// A child of the root that the caller addresses by its row number.
    pub fn row(&mut self, node: Node) -> usize {
        self.row_in(Self::ROOT, node)
    }

    /// A row anywhere under the root, such as in a card: rows are what the
    /// keyboard walks, in the order they are added.
    pub fn row_in(&mut self, parent: usize, node: Node) -> usize {
        let i = self.add(parent, node);
        self.rows.push(i);
        i
    }

    /// Draws a ring round the row the keyboard is on, and a scrollbar
    /// when the content is taller than its view.
    pub fn form(&mut self) {
        self.form = true;
    }

    pub fn finish(self, font: &mut Font) -> Tree {
        let mut tree = Tree {
            bounds: vec![Rect::new(0.0, 0.0, 0.0, 0.0); self.nodes.len()],
            nodes: self.nodes,
            rows: self.rows,
            selected: None,
            focused: None,
            pointer: false,
            scroll: 0.0,
            form: self.form,
            hovered: None,
            thumb: Thumb::Rest,
        };
        tree.layout(font);
        tree
    }
}

pub struct Tree {
    nodes: Vec<Node>,
    /// Of each node, in the coordinates of the root, before scrolling.
    bounds: Vec<Rect>,
    rows: Vec<usize>,
    /// The selected row, by the pointer or the keyboard.
    pub selected: Option<usize>,
    /// The row with the keyboard focus.
    pub focused: Option<usize>,
    /// Whether the pointer made the selection.
    pub pointer: bool,
    pub scroll: f32,
    /// Set by [`Builder::form`].
    form: bool,
    /// The actionable node under the pointer, for controls with parts.
    pub hovered: Option<u64>,
    pub thumb: Thumb,
}

/// The state of the scrollbar thumb.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Thumb {
    Rest,
    Hover,
    /// Held, at this distance from its top.
    Held(f32),
}

/// What a press on the scrollbar hit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BarHit {
    /// The thumb, at this distance from its top.
    Thumb(f32),
    /// The track below the thumb when true, above it when false.
    Track(bool),
}

const BAR_W: f32 = 6.0;
const BAR_INSET: f32 = 3.0;
const MIN_THUMB: f32 = 24.0;
/// Of a form, the room kept above and below a row scrolled into view.
const FORM_PAD: f32 = 24.0;

impl Tree {
    fn layout(&mut self, font: &mut Font) {
        let mut taffy: TaffyTree<usize> = TaffyTree::new();
        taffy.disable_rounding();
        let mut ids = Vec::with_capacity(self.nodes.len());
        for (i, n) in self.nodes.iter().enumerate() {
            ids.push(taffy.new_leaf_with_context(n.style.taffy(), i).unwrap());
        }
        for (i, n) in self.nodes.iter().enumerate() {
            let kids: Vec<_> = n.children.iter().map(|&c| ids[c]).collect();
            taffy.set_children(ids[i], &kids).unwrap();
        }
        let nodes = &self.nodes;
        taffy
            .compute_layout_with_measure(ids[0], Size::MAX_CONTENT, |inputs, _, ctx, style| {
                let i = *ctx.unwrap();
                taffy::compute_leaf_layout(
                    inputs,
                    style,
                    |_, _| 0.0,
                    |known, available| {
                        let max = known.width.or(match available.width {
                            taffy::AvailableSpace::Definite(w) => Some(w),
                            _ => None,
                        });
                        let (w, h) = nodes[i].content.measure(font, max);
                        Size {
                            width: known.width.unwrap_or(w),
                            height: known.height.unwrap_or(h),
                        }
                    },
                )
            })
            .unwrap();
        self.place(&taffy, &ids, 0, 0.0, 0.0);
    }

    fn place(&mut self, taffy: &TaffyTree<usize>, ids: &[taffy::NodeId], i: usize, x: f32, y: f32) {
        let l = taffy.layout(ids[i]).unwrap();
        let (x, y) = (x + l.location.x, y + l.location.y);
        self.bounds[i] = Rect::new(x, y, l.size.width, l.size.height);
        for c in self.nodes[i].children.clone() {
            self.place(taffy, ids, c, x, y);
        }
    }

    /// The size of the root: its content, or its set size.
    pub fn size(&self) -> (f32, f32) {
        (self.bounds[0].w, self.bounds[0].h)
    }

    pub fn root_id(&self) -> u64 {
        self.nodes[0].id.0
    }

    pub fn selected_id(&self) -> Option<u64> {
        self.selected.map(|row| self.nodes[self.rows[row]].id.0)
    }

    fn at(&self, i: usize, origin: (f32, f32)) -> Rect {
        let b = self.bounds[i];
        let scrolls = i != 0;
        let dy = if scrolls { self.scroll } else { 0.0 };
        Rect::new(origin.0 + b.x, origin.1 + b.y - dy, b.w, b.h)
    }

    /// The bounds of a row, with the root at `origin`.
    pub fn row_bounds(&self, row: usize, origin: (f32, f32)) -> Rect {
        self.at(self.rows[row], origin)
    }

    /// The bounds of the first node showing a text field.
    pub fn field_bounds(&self, origin: (f32, f32)) -> Option<Rect> {
        (0..self.nodes.len())
            .find(|&i| matches!(self.nodes[i].content, Content::Field { .. }))
            .map(|i| self.at(i, origin))
    }

    /// The deepest actionable node under a point inside the view.
    pub fn hit_node(&self, view: Rect, x: f32, y: f32) -> Option<u64> {
        if x < view.x || x >= view.x + view.w || y < view.y || y >= view.y + view.h {
            return None;
        }
        (0..self.nodes.len()).rev().find_map(|i| {
            let s = self.nodes[i].semantic.as_ref()?;
            let b = self.at(i, (view.x, view.y));
            (s.supports_action(Action::Click)
                && !s.is_disabled()
                && x >= b.x
                && x < b.x + b.w
                && y >= b.y
                && y < b.y + b.h)
                .then_some(self.nodes[i].id.0)
        })
    }

    pub fn set_expanded(&mut self, id: u64, expanded: bool) {
        if let Some(s) = self
            .nodes
            .iter_mut()
            .find(|n| n.id.0 == id)
            .and_then(|n| n.semantic.as_mut())
        {
            s.set_expanded(expanded);
        }
    }

    /// The row under a point, within the `view` the root shows through.
    pub fn hit(&self, view: Rect, x: f32, y: f32) -> Option<usize> {
        if y < view.y || y >= view.y + view.h {
            return None;
        }
        (0..self.rows.len()).find(|&r| {
            let b = self.row_bounds(r, (view.x, view.y));
            x >= b.x && x < b.x + b.w && y >= b.y && y < b.y + b.h
        })
    }

    /// Whether the row, or a control in it such as a radio, acts.
    pub fn actionable(&self, row: usize) -> bool {
        self.under(self.rows[row])
            .any(|i| self.acts(i, Action::Click))
    }

    /// Selects a row and scrolls it into a view `height` tall.
    pub fn select(&mut self, row: Option<usize>, height: f32) {
        self.selected = row;
        // In a form the keyboard row is the focus; selected means chosen.
        for r in (0..self.rows.len()).filter(|_| !self.form) {
            if let Some(s) = self.nodes[self.rows[r]].semantic.as_mut() {
                s.set_selected(row == Some(r));
            }
        }
        if self.focused.is_none_or(|f| !self.is_text(f)) {
            self.focused = row;
        }
        if let Some(r) = row {
            let b = self.bounds[self.rows[r]];
            let pad = if self.form {
                FORM_PAD
            } else {
                self.bounds[0].h - self.content_bottom()
            };
            let most = (self.bounds[0].h - height).max(0.0);
            self.scroll = self
                .scroll
                .min(b.y - pad)
                .max(b.y + b.h + pad - height)
                .clamp(0.0, most);
        }
    }

    fn content_bottom(&self) -> f32 {
        self.rows
            .last()
            .map_or(self.bounds[0].h, |&i| self.bounds[i].y + self.bounds[i].h)
    }

    fn is_text(&self, row: usize) -> bool {
        self.text_node(row).is_some()
    }

    /// The text input of a row: the row itself or a node in it.
    fn text_node(&self, row: usize) -> Option<usize> {
        let mut stack = vec![self.rows[row]];
        while let Some(i) = stack.pop() {
            if self.nodes[i]
                .semantic
                .as_ref()
                .is_some_and(|s| matches!(s.role(), Role::TextInput | Role::SearchInput))
            {
                return Some(i);
            }
            stack.extend(&self.nodes[i].children);
        }
        None
    }

    /// Moves the selection to the next or previous row that acts.
    pub fn step(&mut self, down: bool, height: f32) {
        self.pointer = false;
        let acts: Vec<usize> = (0..self.rows.len())
            .filter(|&r| self.actionable(r))
            .collect();
        if acts.is_empty() {
            return;
        }
        let at = self
            .selected
            .and_then(|s| acts.iter().position(|&r| r == s));
        let n = acts.len();
        let next = match (at, down) {
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
            (None, true) => 0,
            (None, false) => n - 1,
        };
        self.select(Some(acts[next]), height);
    }

    /// Sets the value agents read from the text input of a row.
    pub fn set_value(&mut self, row: usize, value: &str) {
        if let Some(i) = self.text_node(row) {
            if let Some(s) = self.nodes[i].semantic.as_mut() {
                s.set_value(value);
            }
        }
    }

    /// Paints the tree with its root at `view`, clipped to it.
    pub fn paint(&self, c: &mut Canvas, p: &mut Paint, view: Rect) {
        let clip = c.clip;
        c.clip = Some((
            view.x.max(0.0) as usize,
            view.y.max(0.0) as usize,
            ((view.x + view.w).max(0.0) as usize).min(c.w),
            ((view.y + view.h).max(0.0) as usize).min(c.h),
        ));
        let marks: Vec<(usize, Mark)> = self
            .selected
            .map(|r| {
                let mark = if self.pointer {
                    Mark::Hover
                } else {
                    Mark::Selected
                };
                (self.rows[r], mark)
            })
            .into_iter()
            .collect();
        let hovered = self.hovered.and_then(|id| self.index_of(id));
        let mut stack = vec![(0, Mark::Rest)];
        while let Some((i, inherited)) = stack.pop() {
            let mark = if hovered == Some(i) {
                Mark::Hover
            } else {
                marks
                    .iter()
                    .find(|(n, _)| *n == i)
                    .map_or(inherited, |m| m.1)
            };
            let b = self.at(i, (view.x, view.y));
            if i == 0 || (b.y < view.y + view.h && b.y + b.h > view.y) {
                self.nodes[i].content.paint(c, p, b, mark);
                let pass = if self.form && i != 0 && self.rows.contains(&i) {
                    Mark::Rest
                } else {
                    mark
                };
                stack.extend(self.nodes[i].children.iter().rev().map(|&k| (k, pass)));
            }
        }
        if self.form {
            self.paint_focus(c, view);
            self.paint_bar(c, view);
        }
        c.clip = clip;
    }

    /// A 2 px ring outside the node the keyboard focus is on.
    fn paint_focus(&self, c: &mut Canvas, view: Rect) {
        let Some(row) = self.focused.filter(|_| !self.pointer) else {
            return;
        };
        let b = self.at(self.focus_node(row), (view.x, view.y));
        let k = &crate::draw::theme().controls;
        painter::ring(c, b.inset(-3.0), 9.0, k.focus);
        painter::ring(c, b.inset(-2.0), 8.0, k.focus);
    }

    fn paint_bar(&self, c: &mut Canvas, view: Rect) {
        let Some((_, thumb)) = self.bar(view) else {
            return;
        };
        let k = &crate::draw::theme().controls;
        let color = if self.thumb == Thumb::Rest {
            k.thumb
        } else {
            k.thumb_active
        };
        painter::rounded(c, thumb, BAR_W / 2.0, color);
    }

    /// The semantic nodes in screen coordinates for a platform publisher.
    pub fn accesskit(
        &self,
        view: Rect,
        origin: (i16, i16),
    ) -> (Vec<(NodeId, accesskit::Node)>, NodeId) {
        let mut nodes = Vec::new();
        self.accesskit_node(0, view, origin, &mut nodes);
        let focus = self
            .focused
            .map_or(self.nodes[0].id, |r| self.nodes[self.focus_node(r)].id);
        if let Some((track, _)) = self.bar(view) {
            let root = self.nodes[0].id;
            let id = NodeId(self.scrollbar_id());
            let max = self.scroll_max(view.h) as f64;
            let mut bar = accesskit::Node::new(Role::ScrollBar);
            bar.set_orientation(accesskit::Orientation::Vertical);
            bar.set_numeric_value(self.scroll as f64);
            bar.set_min_numeric_value(0.0);
            bar.set_max_numeric_value(max);
            bar.set_numeric_value_step(48.0);
            bar.set_numeric_value_jump(view.h as f64);
            bar.add_action(Action::SetValue);
            bar.set_controls(vec![root]);
            let (x, y) = (
                track.x as f64 + origin.0 as f64,
                track.y as f64 + origin.1 as f64,
            );
            bar.set_bounds(accesskit::Rect::new(
                x,
                y,
                x + track.w as f64,
                y + track.h as f64,
            ));
            if let Some((_, r)) = nodes.iter_mut().find(|(n, _)| *n == root) {
                r.set_scroll_y(self.scroll as f64);
                r.set_scroll_y_min(0.0);
                r.set_scroll_y_max(max);
                r.add_action(Action::ScrollDown);
                r.add_action(Action::ScrollUp);
                r.push_child(id);
            }
            nodes.push((id, bar));
        }
        (nodes, focus)
    }

    /// The id of the scrollbar a form adds to its semantics.
    pub fn scrollbar_id(&self) -> u64 {
        self.nodes[0].id.0 | 1 << 62
    }

    fn accesskit_node(
        &self,
        i: usize,
        view: Rect,
        origin: (i16, i16),
        out: &mut Vec<(NodeId, accesskit::Node)>,
    ) -> Vec<NodeId> {
        let mut children = Vec::new();
        for &child in &self.nodes[i].children {
            children.extend(self.accesskit_node(child, view, origin, out));
        }
        let Some(mut node) = self.nodes[i].semantic.clone() else {
            return children;
        };
        let b = self.at(i, (view.x, view.y));
        let x = b.x as f64 + origin.0 as f64;
        let y = b.y as f64 + origin.1 as f64;
        node.set_bounds(accesskit::Rect::new(x, y, x + b.w as f64, y + b.h as f64));
        node.set_children(children);
        out.push((self.nodes[i].id, node));
        vec![self.nodes[i].id]
    }

    /// The nodes agents see, as `GET /tree` answers them, with the root
    /// shown through `view` and the window at `origin` on the screen.
    pub fn json(&self, view: Rect, origin: (i16, i16)) -> Value {
        let mut out = Vec::new();
        self.describe(0, view, origin, &mut out);
        Value::Array(out)
    }

    /// Adds the node and the seen nodes under it; answers their ids.
    fn describe(&self, i: usize, view: Rect, origin: (i16, i16), out: &mut Vec<Value>) -> Vec<u64> {
        let mut kids = Vec::new();
        for &k in &self.nodes[i].children {
            kids.extend(self.describe(k, view, origin, out));
        }
        let Some(s) = &self.nodes[i].semantic else {
            return kids;
        };
        let b = self.at(i, (view.x, view.y));
        let (ox, oy) = (origin.0 as f32, origin.1 as f32);
        let focused = self
            .focused
            .is_some_and(|r| self.rows[r] == i || self.focus_node(r) == i);
        let actions: Vec<&str> = [
            (Action::Click, "click"),
            (Action::Focus, "focus"),
            (Action::SetValue, "set_value"),
        ]
        .into_iter()
        .filter(|(a, _)| s.supports_action(*a))
        .map(|(_, name)| name)
        .collect();
        let mut node = json!({
            "id": self.nodes[i].id.0,
            "role": role_name(s.role()),
            "name": s.label().or(s.value().filter(|_| s.role() == Role::Label)).unwrap_or(""),
            "value": s.value(),
            "states": {
                "focused": focused,
                "selected": s.is_selected().unwrap_or(false),
                "disabled": s.is_disabled(),
                "expanded": s.is_expanded().unwrap_or(false),
                "checked": s.toggled() == Some(accesskit::Toggled::True),
            },
            "actions": actions,
            "bounds": {"x": ox + b.x, "y": oy + b.y, "width": b.w, "height": b.h},
        });
        if !kids.is_empty() {
            node["children"] = json!(kids);
        }
        out.push(node);
        vec![self.nodes[i].id.0]
    }

    /// The row of a node id agents named: the row itself or a node in it.
    pub fn row_of(&self, id: u64) -> Option<usize> {
        (0..self.rows.len()).find(|&r| self.under(self.rows[r]).any(|i| self.nodes[i].id.0 == id))
    }

    /// A node and the nodes under it.
    fn under(&self, top: usize) -> impl Iterator<Item = usize> + '_ {
        let mut stack = vec![top];
        std::iter::from_fn(move || {
            let i = stack.pop()?;
            stack.extend(self.nodes[i].children.iter().rev());
            Some(i)
        })
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.nodes.iter().position(|n| n.id.0 == id)
    }

    #[cfg(test)]
    pub fn semantic(&self, id: u64) -> Option<&accesskit::Node> {
        self.nodes[self.index_of(id)?].semantic.as_ref()
    }

    /// The node of a row the keyboard focus is on: its text input, else
    /// its checked radio button, else the row.
    fn focus_node(&self, row: usize) -> usize {
        self.text_node(row)
            .or_else(|| {
                self.under(self.rows[row]).find(|&i| {
                    self.nodes[i].semantic.as_ref().is_some_and(|s| {
                        s.role() == Role::RadioButton
                            && s.toggled() == Some(accesskit::Toggled::True)
                    })
                })
            })
            .unwrap_or(self.rows[row])
    }

    /// Of the radio group a row holds, the radio after or before the
    /// checked one, wrapping; for arrow keys, which move and check.
    pub fn radio_step(&self, row: usize, forward: bool) -> Option<u64> {
        let radios: Vec<usize> = self
            .under(self.rows[row])
            .filter(|&i| {
                self.nodes[i]
                    .semantic
                    .as_ref()
                    .is_some_and(|s| s.role() == Role::RadioButton && !s.is_disabled())
            })
            .collect();
        if radios.is_empty() {
            return None;
        }
        let at = radios.iter().position(|&i| {
            self.nodes[i].semantic.as_ref().and_then(|s| s.toggled())
                == Some(accesskit::Toggled::True)
        });
        let n = radios.len();
        let next = match (at, forward) {
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
            (None, _) => 0,
        };
        Some(self.nodes[radios[next]].id.0)
    }

    /// How far the content can scroll in a view `height` tall.
    pub fn scroll_max(&self, height: f32) -> f32 {
        (self.bounds[0].h - height).max(0.0)
    }

    pub fn scroll_to(&mut self, y: f32, height: f32) {
        self.scroll = y.clamp(0.0, self.scroll_max(height));
    }

    /// The track and the thumb of the scrollbar of a form in `view`.
    fn bar(&self, view: Rect) -> Option<(Rect, Rect)> {
        let max = self.scroll_max(view.h);
        if !self.form || max <= 0.0 {
            return None;
        }
        let track = Rect::new(
            view.x + view.w - BAR_W - BAR_INSET,
            view.y + BAR_INSET,
            BAR_W,
            view.h - 2.0 * BAR_INSET,
        );
        let h = (track.h * view.h / self.bounds[0].h).clamp(MIN_THUMB.min(track.h), track.h);
        let y = track.y + (track.h - h) * self.scroll / max;
        Some((track, Rect::new(track.x, y, BAR_W, h)))
    }

    /// What a point hits on the scrollbar, if anything. The target is a
    /// little wider than the bar.
    pub fn bar_hit(&self, view: Rect, x: f32, y: f32) -> Option<BarHit> {
        let (track, thumb) = self.bar(view)?;
        if x < track.x - 6.0 || x > view.x + view.w || y < view.y || y >= view.y + view.h {
            return None;
        }
        Some(if y >= thumb.y && y < thumb.y + thumb.h {
            BarHit::Thumb(y - thumb.y)
        } else {
            BarHit::Track(y > thumb.y)
        })
    }

    /// Scrolls so the thumb, held `grab` below its top, follows `y`.
    pub fn drag_thumb(&mut self, view: Rect, y: f32, grab: f32) {
        let Some((track, thumb)) = self.bar(view) else {
            return;
        };
        let room = (track.h - thumb.h).max(1.0);
        let at = (y - grab - track.y).clamp(0.0, room);
        self.scroll_to(at / room * self.scroll_max(view.h), view.h);
    }

    /// Whether the node of a row, or the input in it, supports an action.
    pub fn supports(&self, row: usize, action: Action) -> bool {
        [Some(self.rows[row]), self.text_node(row)]
            .into_iter()
            .flatten()
            .any(|i| self.acts(i, action))
    }

    /// Whether the node with this id is enabled and supports an action.
    pub fn node_supports(&self, id: u64, action: Action) -> bool {
        self.index_of(id).is_some_and(|i| self.acts(i, action))
    }

    fn acts(&self, i: usize, action: Action) -> bool {
        self.nodes[i]
            .semantic
            .as_ref()
            .is_some_and(|s| !s.is_disabled() && s.supports_action(action))
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::RadioButton => "radio_button",
        Role::RadioGroup => "radio_group",
        Role::ComboBox => "combo_box",
        Role::ListBox => "list_box",
        Role::ListBoxOption => "list_box_option",
        Role::Window => "window",
        Role::Status => "status",
        Role::ScrollBar => "scroll_bar",
        Role::Switch => "switch",
        Role::TitleBar => "title_bar",
        Role::Button => "button",
        Role::Toolbar => "toolbar",
        Role::Label => "label",
        Role::Menu => "menu",
        Role::MenuItem => "menu_item",
        Role::MenuItemCheckBox => "menu_item_check_box",
        Role::TextInput => "text_input",
        Role::SearchInput => "search_input",
        Role::Heading => "heading",
        _ => "group",
    }
}

#[cfg(test)]
mod tests {
    use super::widgets::{Ink, SMALL_SIZE, TEXT_SIZE};
    use super::*;

    const PAD: f32 = 5.0;

    fn item(id: u64, name: &str) -> Node {
        Node::new(
            Content::Choice,
            style().row().center().height(24.0).pad(0.0, 8.0, 0.0, 8.0),
        )
        .semantic(id, Role::MenuItem, name)
    }

    fn text(t: &str, size: f32) -> Node {
        Node::new(
            Content::Text {
                text: t.into(),
                size,
                ink: Ink::Text,
            },
            style().grow(),
        )
    }

    fn menu(width: Option<f32>) -> Builder {
        let mut s = style().column().pad(PAD, PAD, PAD, PAD);
        if let Some(w) = width {
            s = s.width(w);
        }
        Builder::new(Node::new(Content::Box, s).semantic(1000, Role::Menu, ""))
    }

    #[test]
    fn one_item_fits_without_a_minimum() {
        let mut font = Font::for_test();
        let mut b = menu(Some(280.0));
        b.row(item(1, "Quit"));
        let t = b.finish(&mut font);
        assert_eq!(t.size(), (280.0, 24.0 + 2.0 * PAD));
        assert_eq!(
            t.row_bounds(0, (0.0, 0.0)),
            Rect::new(PAD, PAD, 270.0, 24.0)
        );
    }

    #[test]
    fn a_menu_grows_to_its_longest_row_within_bounds() {
        let mut font = Font::for_test();
        let mut b = menu(None);
        let r = b.row(item(1, "a"));
        let name = "Open a new window with a much longer label than usual";
        b.add(r, text(name, TEXT_SIZE));
        b.nodes[0].style = b.nodes[0].style.clone().min_width(280.0).max_width(420.0);
        let t = b.finish(&mut font);
        let want = (font.width(name, TEXT_SIZE) + 16.0 + 2.0 * PAD).clamp(280.0, 420.0);
        assert!((t.size().0 - want).abs() < 0.01, "{} vs {want}", t.size().0);
    }

    #[test]
    fn a_label_gives_way_to_a_detail_in_its_row() {
        let mut font = Font::for_test();
        let mut b = menu(Some(200.0));
        let r = b.row(item(1, "x"));
        let name = b.add(r, text(&"long name ".repeat(10), TEXT_SIZE));
        let detail = b.add(
            r,
            Node::new(
                Content::Text {
                    text: "Minimized".into(),
                    size: SMALL_SIZE,
                    ink: Ink::Dim,
                },
                style().margin(0.0, 0.0, 0.0, 8.0),
            ),
        );
        let t = b.finish(&mut font);
        let (n, d) = (t.bounds[name], t.bounds[detail]);
        assert!((d.x + d.w - (200.0 - PAD - 8.0)).abs() < 0.01);
        assert!((n.x + n.w + 8.0 - d.x).abs() < 0.01);
        assert!((d.w - font.width("Minimized", SMALL_SIZE)).abs() < 0.01);
    }

    #[test]
    fn keyboard_skips_rows_that_do_not_act_and_scrolls() {
        let mut font = Font::for_test();
        let mut b = menu(Some(200.0));
        b.row(Node::new(Content::Box, style().height(22.0)).semantic(1, Role::Heading, "Apps"));
        b.row(item(2, "One"));
        b.row(Node::new(Content::Rule, style().height(9.0)));
        b.row(item(3, "Two"));
        let mut t = b.finish(&mut font);
        t.step(true, 50.0);
        assert_eq!(t.selected, Some(1));
        t.step(true, 50.0);
        assert_eq!(t.selected, Some(3));
        let row = t.row_bounds(3, (0.0, 0.0));
        assert!(row.y + row.h + PAD <= 50.0 + 0.01, "{row:?}");
        t.step(true, 50.0);
        assert_eq!(t.selected, Some(1));
        assert!(t.row_bounds(1, (0.0, 0.0)).y - PAD >= -0.01);
        t.step(false, 50.0);
        assert_eq!(t.selected, Some(3));
    }

    #[test]
    fn hits_follow_the_scroll_and_stay_in_the_view() {
        let mut font = Font::for_test();
        let mut b = menu(Some(200.0));
        for i in 0..10 {
            b.row(item(i, "row"));
        }
        let mut t = b.finish(&mut font);
        let view = Rect::new(10.0, 20.0, 200.0, 100.0);
        assert_eq!(t.hit(view, 50.0, 20.0 + PAD + 1.0), Some(0));
        t.select(Some(9), view.h);
        assert_eq!(t.hit(view, 50.0, 20.0 + 100.0 - PAD - 1.0), Some(9));
        assert_eq!(t.hit(view, 50.0, 20.0 + 101.0), None);
    }

    #[test]
    fn semantics_nest_and_share_bounds() {
        let mut font = Font::for_test();
        let mut b = menu(Some(200.0));
        let search = b.row(Node::new(Content::Box, style().height(30.0)));
        b.add(
            search,
            Node::new(
                Content::Field {
                    placeholder: "Search".into(),
                },
                style().grow(),
            )
            .semantic(7, Role::SearchInput, "Search"),
        );
        let r = b.row(item(8, "Files"));
        b.add(r, text("Files", TEXT_SIZE));
        let mut t = b.finish(&mut font);
        t.focused = Some(0);
        t.set_value(0, "fi");
        let v = t.json(Rect::new(10.0, 20.0, 200.0, 64.0), (100, 200));
        let field = &v[0];
        assert_eq!(field["id"], 7);
        assert_eq!(field["role"], "search_input");
        assert_eq!(field["value"], "fi");
        assert_eq!(field["states"]["focused"], true);
        assert_eq!(field["actions"], json!(["focus", "set_value"]));
        assert_eq!(field["bounds"]["x"], 115.0);
        assert_eq!(field["bounds"]["y"], 225.0);
        assert_eq!(v[1]["actions"], json!(["click", "focus"]));
        assert!(v[1].get("children").is_none());
        let root = v.as_array().unwrap().last().unwrap();
        assert_eq!(root["role"], "menu");
        assert_eq!(root["children"], json!([7, 8]));
        assert_eq!(t.row_of(7), Some(0));
        assert_eq!(t.row_of(8), Some(1));
    }

    #[test]
    fn a_selected_row_paints_only_inside_the_view() {
        let mut font = Font::for_test();
        let mut b = menu(Some(100.0));
        for i in 0..6 {
            b.row(item(i, "row"));
        }
        let mut t = b.finish(&mut font);
        t.select(Some(5), 40.0);
        let mut c = Canvas::new(100, 200, 0);
        let icons = |_: usize| None;
        let mut p = Paint {
            font: &mut font,
            icons: &icons,
            field: None,
        };
        t.paint(&mut c, &mut p, Rect::new(0.0, 0.0, 100.0, 40.0));
        assert!(c.px[..100 * 40].contains(&crate::draw::theme().selected));
        assert!(c.px[100 * 40..].iter().all(|&px| px == 0));
    }
}
