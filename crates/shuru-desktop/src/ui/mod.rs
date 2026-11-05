//! Retained shell nodes shared by layout, input, paint and semantics.

mod painter;
pub mod text;
pub mod widgets;

use crate::draw::Rect;
use accesskit::{Action, Node, NodeId, Role};
use serde_json::{json, Value};
use taffy::prelude::*;
use widgets::SHEET_PAD;

pub struct RowStyle {
    pub height: f32,
}

pub struct Element {
    pub id: NodeId,
    pub semantic: Node,
    pub style: RowStyle,
    pub bounds: Rect,
}

pub struct Tree {
    pub nodes: Vec<Element>,
    root: Node,
    pub selected: Option<usize>,
    pub focused: Option<usize>,
    pub scroll: f32,
    pub pointer: bool,
    pub height: f32,
}

impl Tree {
    pub fn new(rows: Vec<(u64, Role, String, f32)>, width: f32) -> Self {
        let nodes = rows
            .into_iter()
            .map(|(id, role, name, height)| {
                let mut semantic = Node::new(role);
                semantic.set_label(name);
                match role {
                    Role::MenuItem => {
                        semantic.add_action(Action::Click);
                        semantic.add_action(Action::Focus);
                    }
                    Role::TextInput => {
                        semantic.add_action(Action::Focus);
                        semantic.add_action(Action::SetValue);
                    }
                    _ => {}
                }
                Element {
                    id: NodeId(id),
                    semantic,
                    bounds: Rect::new(0.0, 0.0, 0.0, 0.0),
                    style: RowStyle { height },
                }
            })
            .collect();
        let mut tree = Self {
            nodes,
            root: Node::new(Role::Menu),
            selected: None,
            focused: None,
            scroll: 0.0,
            pointer: false,
            height: 0.0,
        };
        tree.root
            .set_children(tree.nodes.iter().map(|n| n.id).collect::<Vec<_>>());
        tree.layout(width);
        tree
    }

    fn layout(&mut self, width: f32) {
        let mut taffy: TaffyTree<()> = TaffyTree::new();
        let children: Vec<_> = self
            .nodes
            .iter()
            .map(|n| {
                taffy
                    .new_leaf(Style {
                        size: Size {
                            width: percent(1.0),
                            height: length(n.style.height),
                        },
                        flex_shrink: 0.0,
                        ..Default::default()
                    })
                    .unwrap()
            })
            .collect();
        let root = taffy
            .new_with_children(
                Style {
                    flex_direction: FlexDirection::Column,
                    size: Size {
                        width: length(width),
                        height: auto(),
                    },
                    padding: taffy::geometry::Rect::length(SHEET_PAD),
                    ..Default::default()
                },
                &children,
            )
            .unwrap();
        taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();
        self.height = taffy.layout(root).unwrap().size.height;
        for (node, child) in self.nodes.iter_mut().zip(children) {
            let b = taffy.layout(child).unwrap();
            node.bounds = Rect::new(b.location.x, b.location.y, b.size.width, b.size.height);
        }
    }

    pub fn bounds(&self, sheet: Rect) -> Vec<Rect> {
        self.nodes
            .iter()
            .map(|n| {
                Rect::new(
                    sheet.x + n.bounds.x,
                    sheet.y + n.bounds.y - self.scroll,
                    n.bounds.w,
                    n.bounds.h,
                )
            })
            .collect()
    }

    pub fn hit(&self, sheet: Rect, x: f32, y: f32) -> Option<usize> {
        if y < sheet.y || y >= sheet.y + sheet.h {
            return None;
        }
        self.bounds(sheet)
            .iter()
            .position(|b| x >= b.x && x < b.x + b.w && y >= b.y && y < b.y + b.h)
    }

    pub fn select(&mut self, selected: Option<usize>, height: f32) {
        self.selected = selected;
        for (i, node) in self.nodes.iter_mut().enumerate() {
            node.semantic.set_selected(selected == Some(i));
        }
        if self
            .focused
            .is_none_or(|i| self.nodes[i].semantic.role() != Role::TextInput)
        {
            self.focused = selected;
        }
        if let Some(i) = selected {
            let b = self.nodes[i].bounds;
            self.scroll = self
                .scroll
                .min(b.y)
                .max(b.y + b.h - height + SHEET_PAD)
                .max(0.0);
        }
    }

    pub fn step(&mut self, down: bool, height: f32) {
        self.pointer = false;
        let acts: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.semantic.supports_action(Action::Click) && !n.semantic.is_disabled())
            .map(|(i, _)| i)
            .collect();
        if acts.is_empty() {
            return;
        }
        let next = match self
            .selected
            .and_then(|s| acts.iter().position(|&i| i == s))
        {
            Some(i) if down => (i + 1) % acts.len(),
            Some(i) => (i + acts.len() - 1) % acts.len(),
            None if down => 0,
            None => acts.len() - 1,
        };
        self.select(Some(acts[next]), height);
    }

    pub fn json(&self, sheet: Rect, origin: (i16, i16)) -> Value {
        let screen = |b: Rect| {
            accesskit::Rect::new(
                (b.x + origin.0 as f32) as f64,
                (b.y + origin.1 as f32) as f64,
                (b.x + b.w + origin.0 as f32) as f64,
                (b.y + b.h + origin.1 as f32) as f64,
            )
        };
        let serialize = |id: NodeId, s: &Node, focused: bool| {
            let role = match s.role() {
                Role::Menu => "menu",
                Role::TextInput => "text_input",
                Role::MenuItem => "menu_item",
                Role::Heading => "heading",
                _ => "separator",
            };
            let actions: Vec<_> = [
                (Action::Click, "click"),
                (Action::Focus, "focus"),
                (Action::SetValue, "set_value"),
            ]
            .into_iter()
            .filter(|(a, _)| s.supports_action(*a))
            .map(|(_, a)| a)
            .collect();
            let b = s.bounds().unwrap();
            json!({"id": id.0, "role": role, "name": s.label().unwrap_or(""), "value": s.value(),
                "states": {"focused": focused, "selected": s.is_selected().unwrap_or(false), "disabled": s.is_disabled(), "expanded": s.is_expanded().unwrap_or(false)},
                "actions": actions, "bounds": {"x": b.x0,"y": b.y0,"width": b.width(),"height": b.height()}})
        };
        let mut nodes: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .zip(self.bounds(sheet))
            .map(|((i, n), b)| {
                let mut semantic = n.semantic.clone();
                semantic.set_bounds(screen(b));
                serialize(n.id, &semantic, self.focused == Some(i))
            })
            .collect();
        let mut root = self.root.clone();
        root.set_bounds(screen(sheet));
        let mut root = serialize(NodeId(u64::MAX), &root, false);
        root["children"] = json!(self
            .root
            .children()
            .iter()
            .map(|id| id.0)
            .collect::<Vec<_>>());
        nodes.push(root);
        Value::Array(nodes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(roles: &[Role], heights: &[f32]) -> Tree {
        Tree::new(
            roles
                .iter()
                .zip(heights)
                .enumerate()
                .map(|(i, (&role, &h))| (i as u64, role, format!("row {i}"), h))
                .collect(),
            320.0,
        )
    }

    #[test]
    fn one_item_fits_without_a_minimum() {
        let t = tree(&[Role::MenuItem], &[24.0]);
        assert_eq!(t.height, 24.0 + 2.0 * SHEET_PAD);
        assert_eq!(t.nodes[0].bounds.h, 24.0);
    }

    #[test]
    fn launcher_fits_all_rows() {
        let t = tree(
            &[
                Role::TextInput,
                Role::Heading,
                Role::MenuItem,
                Role::Splitter,
            ],
            &[30.0, 22.0, 26.0, 29.0],
        );
        assert_eq!(t.height, 117.0);
        let last = t.nodes.last().unwrap().bounds;
        assert_eq!(last.y + last.h + SHEET_PAD, t.height);
    }

    #[test]
    fn keyboard_skips_labels_and_wraps() {
        let mut t = tree(
            &[
                Role::TextInput,
                Role::Heading,
                Role::MenuItem,
                Role::Splitter,
                Role::MenuItem,
            ],
            &[30.0, 22.0, 26.0, 9.0, 26.0],
        );
        t.step(true, 70.0);
        assert_eq!(t.selected, Some(2));
        t.step(true, 70.0);
        assert_eq!(t.selected, Some(4));
        assert!(t.scroll > 0.0);
        t.step(true, 70.0);
        assert_eq!(t.selected, Some(2));
        t.step(false, 70.0);
        assert_eq!(t.selected, Some(4));
    }

    #[test]
    fn semantics_share_bounds_and_actions() {
        let mut t = tree(&[Role::TextInput, Role::MenuItem], &[30.0, 24.0]);
        t.focused = Some(0);
        t.nodes[0].semantic.set_value("query");
        let v = t.json(Rect::new(10.0, 20.0, 320.0, t.height), (100, 200));
        assert_eq!(v[0]["id"], 0);
        assert_eq!(v[0]["role"], "text_input");
        assert_eq!(v[0]["value"], "query");
        assert_eq!(v[0]["bounds"]["x"], 115.0);
        assert_eq!(v[0]["bounds"]["y"], 225.0);
        assert_eq!(v[0]["states"]["focused"], true);
        assert_eq!(v[0]["actions"], json!(["focus", "set_value"]));
        assert_eq!(v[1]["actions"], json!(["click", "focus"]));
    }
}
