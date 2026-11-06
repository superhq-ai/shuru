// Portions adapted from AccessKit, copyright The AccessKit Authors.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! AT-SPI wire interfaces backed by the shared AccessKit mapping.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use accesskit_atspi_common::{simplified::Accessible, CoordType, PlatformNode, PlatformRoot};
use zbus::{fdo, interface, OwnedObjectPath};

use super::{address, node_path, ROOT};

type Address = (String, OwnedObjectPath);
type Bounds = (i32, i32, i32, i32);

fn error(e: accesskit_atspi_common::Error) -> fdo::Error {
    fdo::Error::Failed(format!("{e:?}"))
}

fn coord(value: u32) -> fdo::Result<CoordType> {
    match value {
        0 => Ok(CoordType::Screen),
        1 => Ok(CoordType::Window),
        2 => Ok(CoordType::Parent),
        _ => Err(fdo::Error::InvalidArgs("Invalid coordinate type".into())),
    }
}

fn bounds(b: accesskit_atspi_common::Rect) -> Bounds {
    (b.x, b.y, b.width, b.height)
}

#[derive(Clone)]
pub(super) struct Node {
    pub accessible: Accessible,
    pub bus: String,
    pub desktop: Arc<OnceLock<Address>>,
}

impl Node {
    fn reference(&self, accessible: Accessible) -> Address {
        let path = match accessible {
            Accessible::Root(_) => ROOT.to_string(),
            Accessible::Node(n) => node_path(n.adapter_id(), n.id()),
        };
        address(&self.bus, &path)
    }

    fn optional(&self, accessible: Option<Accessible>) -> Address {
        accessible.map_or_else(
            || address("", "/org/a11y/atspi/null"),
            |a| self.reference(a),
        )
    }

    pub(super) fn interfaces(&self) -> fdo::Result<Vec<String>> {
        let mut names = vec!["org.a11y.atspi.Accessible".into()];
        for (supported, name) in [
            (self.accessible.supports_action(), "Action"),
            (self.accessible.supports_component(), "Component"),
            (self.accessible.supports_selection(), "Selection"),
            (self.accessible.supports_text(), "Text"),
            (self.accessible.supports_editable_text(), "EditableText"),
            (self.accessible.supports_value(), "Value"),
        ] {
            if supported.map_err(error)? {
                names.push(format!("org.a11y.atspi.{name}"));
            }
        }
        if matches!(self.accessible, Accessible::Root(_)) {
            names.push("org.a11y.atspi.Application".into());
        }
        Ok(names)
    }
}

#[interface(name = "org.a11y.atspi.Accessible")]
impl Node {
    #[zbus(property)]
    pub fn name(&self) -> fdo::Result<String> {
        self.accessible.name().map_err(error)
    }
    #[zbus(property)]
    fn description(&self) -> fdo::Result<String> {
        self.accessible.description().map_err(error)
    }
    #[zbus(property)]
    fn parent(&self) -> fdo::Result<Address> {
        if matches!(self.accessible, Accessible::Root(_)) {
            return Ok(self
                .desktop
                .get()
                .cloned()
                .unwrap_or_else(|| self.optional(None)));
        }
        Ok(self.optional(self.accessible.parent().map_err(error)?))
    }
    #[zbus(property)]
    pub fn child_count(&self) -> fdo::Result<i32> {
        self.accessible.child_count().map_err(error)
    }
    #[zbus(property)]
    fn locale(&self) -> String {
        crate::locale::messages_locale()
    }
    #[zbus(property)]
    fn accessible_id(&self) -> fdo::Result<String> {
        self.accessible.accessible_id().map_err(error)
    }
    fn get_child_at_index(&self, index: i32) -> fdo::Result<(Address,)> {
        let index = usize::try_from(index)
            .map_err(|_| fdo::Error::InvalidArgs("Negative child index".into()))?;
        Ok((self.optional(self.accessible.child_at_index(index).map_err(error)?),))
    }
    fn get_children(&self) -> fdo::Result<Vec<Address>> {
        self.accessible
            .map_children(|a| self.reference(a))
            .map_err(error)
    }
    fn get_index_in_parent(&self) -> fdo::Result<i32> {
        self.accessible.index_in_parent().map_err(error)
    }
    pub fn get_role(&self) -> fdo::Result<u32> {
        self.accessible.role().map(|r| r as u32).map_err(error)
    }
    fn get_role_name(&self) -> fdo::Result<String> {
        self.accessible
            .role()
            .map(|r| format!("{r:?}"))
            .map_err(error)
    }
    fn get_localized_role_name(&self) -> fdo::Result<String> {
        self.accessible.localized_role_name().map_err(error)
    }
    fn get_state(&self) -> Vec<u32> {
        let bits = self.accessible.state().bits();
        vec![bits as u32, (bits >> 32) as u32]
    }
    fn get_attributes(&self) -> fdo::Result<HashMap<&str, String>> {
        self.accessible.attributes().map_err(error)
    }
    fn get_relation_set(&self) -> fdo::Result<Vec<(u32, Vec<Address>)>> {
        self.accessible
            .relation_set()
            .map(|relations| {
                relations
                    .into_iter()
                    .map(|(kind, nodes)| {
                        (
                            kind as u32,
                            nodes.into_iter().map(|n| self.reference(n)).collect(),
                        )
                    })
                    .collect()
            })
            .map_err(error)
    }
    fn get_application(&self) -> (Address,) {
        (address(&self.bus, ROOT),)
    }
    fn get_interfaces(&self) -> fdo::Result<Vec<String>> {
        self.interfaces()
    }
}

pub(super) struct Application(pub PlatformRoot);
#[interface(name = "org.a11y.atspi.Application")]
impl Application {
    #[zbus(property)]
    fn toolkit_name(&self) -> &str {
        "SuperHQ"
    }
    #[zbus(property)]
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    #[zbus(property)]
    fn atspi_version(&self) -> &str {
        "2.1"
    }
    #[zbus(property)]
    fn id(&self) -> fdo::Result<i32> {
        self.0.id().map_err(error)
    }
    #[zbus(property)]
    fn set_id(&mut self, id: i32) -> fdo::Result<()> {
        self.0.set_id(id).map_err(error)
    }
}

pub(super) struct Component(pub Node);
#[interface(name = "org.a11y.atspi.Component")]
impl Component {
    fn contains(&self, x: i32, y: i32, kind: u32) -> fdo::Result<bool> {
        self.0
            .accessible
            .contains(x, y, coord(kind)?)
            .map_err(error)
    }
    fn get_accessible_at_point(&self, x: i32, y: i32, kind: u32) -> fdo::Result<(Address,)> {
        Ok((self.0.optional(
            self.0
                .accessible
                .accessible_at_point(x, y, coord(kind)?)
                .map_err(error)?,
        ),))
    }
    pub fn get_extents(&self, kind: u32) -> fdo::Result<(Bounds,)> {
        self.0
            .accessible
            .extents(coord(kind)?)
            .map(|b| (bounds(b),))
            .map_err(error)
    }
    fn get_position(&self, kind: u32) -> fdo::Result<(i32, i32)> {
        let ((x, y, _, _),) = self.get_extents(kind)?;
        Ok((x, y))
    }
    fn get_size(&self) -> fdo::Result<(i32, i32)> {
        let ((_, _, w, h),) = self.get_extents(0)?;
        Ok((w, h))
    }
    fn get_layer(&self) -> fdo::Result<u32> {
        self.0.accessible.layer().map(|l| l as u32).map_err(error)
    }
    fn get_mdi_z_order(&self) -> i16 {
        0
    }
    fn get_alpha(&self) -> f64 {
        1.0
    }
    fn grab_focus(&self) -> fdo::Result<bool> {
        self.0.accessible.grab_focus().map_err(error)
    }
}

pub(super) struct Action(pub PlatformNode);
#[interface(name = "org.a11y.atspi.Action")]
impl Action {
    #[zbus(property)]
    pub fn n_actions(&self) -> fdo::Result<i32> {
        self.0.n_actions().map_err(error)
    }
    fn get_name(&self, index: i32) -> fdo::Result<String> {
        self.0.action_name(index).map_err(error)
    }
    fn get_localized_name(&self, index: i32) -> fdo::Result<String> {
        self.0.action_name(index).map_err(error)
    }
    fn get_description(&self, _index: i32) -> &str {
        ""
    }
    fn get_key_binding(&self, _index: i32) -> &str {
        ""
    }
    fn get_actions(&self) -> fdo::Result<Vec<(String, String, String)>> {
        self.0
            .actions()
            .map(|actions| {
                actions
                    .into_iter()
                    .map(|a| (a.localized_name, a.description, a.key_binding))
                    .collect()
            })
            .map_err(error)
    }
    pub fn do_action(&self, index: i32) -> fdo::Result<bool> {
        self.0.do_action(index).map_err(error)
    }
}

pub(super) struct TextInterface {
    node: PlatformNode,
}

impl TextInterface {
    pub fn new(node: PlatformNode) -> Self {
        Self { node }
    }

    fn map_error(&self) -> impl '_ + FnOnce(accesskit_atspi_common::Error) -> fdo::Error {
        |error| super_error(error)
    }
}

#[interface(name = "org.a11y.atspi.Text")]
impl TextInterface {
    #[zbus(property)]
    fn character_count(&self) -> fdo::Result<i32> {
        self.node.character_count().map_err(self.map_error())
    }

    #[zbus(property)]
    fn caret_offset(&self) -> fdo::Result<i32> {
        self.node.caret_offset().map_err(self.map_error())
    }

    fn get_string_at_offset(
        &self,
        offset: i32,
        granularity: u32,
    ) -> fdo::Result<(String, i32, i32)> {
        self.node
            .string_at_offset(offset, granularity_from(granularity)?)
            .map_err(self.map_error())
    }

    fn get_text(&self, start_offset: i32, end_offset: i32) -> fdo::Result<String> {
        self.node
            .text(start_offset, end_offset)
            .map_err(self.map_error())
    }

    fn set_caret_offset(&self, offset: i32) -> fdo::Result<bool> {
        self.node.set_caret_offset(offset).map_err(self.map_error())
    }

    fn get_attribute_value(&self, offset: i32, attribute_name: &str) -> fdo::Result<String> {
        self.node
            .text_attribute_value(offset, attribute_name)
            .map_err(self.map_error())
    }

    fn get_attributes(
        &self,
        offset: i32,
    ) -> fdo::Result<(HashMap<&'static str, String>, i32, i32)> {
        self.node.text_attributes(offset).map_err(self.map_error())
    }

    fn get_default_attributes(&self) -> fdo::Result<HashMap<&'static str, String>> {
        self.node
            .default_text_attributes()
            .map_err(self.map_error())
    }

    fn get_character_extents(&self, offset: i32, coord_type: u32) -> fdo::Result<Bounds> {
        self.node
            .character_extents(offset, coord(coord_type)?)
            .map(bounds)
            .map_err(self.map_error())
    }

    fn get_offset_at_point(&self, x: i32, y: i32, coord_type: u32) -> fdo::Result<i32> {
        self.node
            .offset_at_point(x, y, coord(coord_type)?)
            .map_err(self.map_error())
    }

    fn get_n_selections(&self) -> fdo::Result<i32> {
        self.node.n_selections().map_err(self.map_error())
    }

    fn get_selection(&self, selection_num: i32) -> fdo::Result<(i32, i32)> {
        self.node.selection(selection_num).map_err(self.map_error())
    }

    fn add_selection(&self, start_offset: i32, end_offset: i32) -> fdo::Result<bool> {
        self.node
            .add_selection(start_offset, end_offset)
            .map_err(self.map_error())
    }

    fn remove_selection(&self, selection_num: i32) -> fdo::Result<bool> {
        self.node
            .remove_selection(selection_num)
            .map_err(self.map_error())
    }

    fn set_selection(
        &self,
        selection_num: i32,
        start_offset: i32,
        end_offset: i32,
    ) -> fdo::Result<bool> {
        self.node
            .set_selection(selection_num, start_offset, end_offset)
            .map_err(self.map_error())
    }

    fn get_range_extents(
        &self,
        start_offset: i32,
        end_offset: i32,
        coord_type: u32,
    ) -> fdo::Result<Bounds> {
        self.node
            .range_extents(start_offset, end_offset, coord(coord_type)?)
            .map(bounds)
            .map_err(self.map_error())
    }

    fn get_attribute_run(
        &self,
        offset: i32,
        include_defaults: bool,
    ) -> fdo::Result<(HashMap<&'static str, String>, i32, i32)> {
        self.node
            .text_attribute_run(offset, include_defaults)
            .map_err(self.map_error())
    }

    fn scroll_substring_to(
        &self,
        start_offset: i32,
        end_offset: i32,
        scroll_type: u32,
    ) -> fdo::Result<bool> {
        self.node
            .scroll_substring_to(start_offset, end_offset, scroll_from(scroll_type)?)
            .map_err(self.map_error())
    }

    fn scroll_substring_to_point(
        &self,
        start_offset: i32,
        end_offset: i32,
        coord_type: u32,
        x: i32,
        y: i32,
    ) -> fdo::Result<bool> {
        self.node
            .scroll_substring_to_point(start_offset, end_offset, coord(coord_type)?, x, y)
            .map_err(self.map_error())
    }
}

pub(super) struct ValueInterface {
    node: PlatformNode,
}

impl ValueInterface {
    pub fn new(node: PlatformNode) -> Self {
        Self { node }
    }

    fn map_error(&self) -> impl '_ + FnOnce(accesskit_atspi_common::Error) -> fdo::Error {
        |error| super_error(error)
    }
}

#[interface(name = "org.a11y.atspi.Value")]
impl ValueInterface {
    #[zbus(property)]
    fn minimum_value(&self) -> fdo::Result<f64> {
        self.node.minimum_value().map_err(self.map_error())
    }

    #[zbus(property)]
    fn maximum_value(&self) -> fdo::Result<f64> {
        self.node.maximum_value().map_err(self.map_error())
    }

    #[zbus(property)]
    fn minimum_increment(&self) -> fdo::Result<f64> {
        self.node.minimum_increment().map_err(self.map_error())
    }

    #[zbus(property)]
    fn current_value(&self) -> fdo::Result<f64> {
        self.node.current_value().map_err(self.map_error())
    }

    #[zbus(property)]
    fn set_current_value(&mut self, value: f64) -> fdo::Result<()> {
        self.node.set_current_value(value).map_err(self.map_error())
    }
}

pub(super) struct SelectionInterface {
    bus_name: String,
    node: PlatformNode,
}

impl SelectionInterface {
    pub fn new(bus_name: String, node: PlatformNode) -> Self {
        Self { bus_name, node }
    }

    fn map_error(&self) -> impl '_ + FnOnce(accesskit_atspi_common::Error) -> fdo::Error {
        |error| super_error(error)
    }
}

#[interface(name = "org.a11y.atspi.Selection")]
impl SelectionInterface {
    #[zbus(property)]
    fn n_selected_children(&self) -> fdo::Result<i32> {
        self.node.n_selected_children().map_err(self.map_error())
    }

    fn get_selected_child(&self, selected_child_index: i32) -> fdo::Result<(Address,)> {
        let child = self
            .node
            .selected_child(map_child_index(selected_child_index)?)
            .map_err(self.map_error())?;
        Ok((child.map_or_else(
            || address("", "/org/a11y/atspi/null"),
            |child| address(&self.bus_name, &node_path(self.node.adapter_id(), child)),
        ),))
    }

    fn select_child(&self, child_index: i32) -> fdo::Result<bool> {
        self.node
            .select_child(map_child_index(child_index)?)
            .map_err(self.map_error())
    }

    fn deselect_selected_child(&self, selected_child_index: i32) -> fdo::Result<bool> {
        self.node
            .deselect_selected_child(map_child_index(selected_child_index)?)
            .map_err(self.map_error())
    }

    fn is_child_selected(&self, child_index: i32) -> fdo::Result<bool> {
        self.node
            .is_child_selected(map_child_index(child_index)?)
            .map_err(self.map_error())
    }

    fn select_all(&self) -> fdo::Result<bool> {
        self.node.select_all().map_err(self.map_error())
    }

    fn clear_selection(&self) -> fdo::Result<bool> {
        self.node.clear_selection().map_err(self.map_error())
    }

    fn deselect_child(&self, child_index: i32) -> fdo::Result<bool> {
        self.node
            .deselect_child(map_child_index(child_index)?)
            .map_err(self.map_error())
    }
}

fn map_child_index(index: i32) -> fdo::Result<usize> {
    index
        .try_into()
        .map_err(|_| fdo::Error::InvalidArgs("Negative child index".into()))
}

pub(super) struct EditableTextInterface(PlatformNode);

impl EditableTextInterface {
    pub fn new(node: PlatformNode) -> Self {
        Self(node)
    }

    fn map_error(&self) -> impl '_ + FnOnce(accesskit_atspi_common::Error) -> fdo::Error {
        |error| super_error(error)
    }
}

#[interface(name = "org.a11y.atspi.EditableText")]
impl EditableTextInterface {
    fn set_text_contents(&self, new_contents: &str) -> fdo::Result<bool> {
        self.0
            .set_text_contents(new_contents)
            .map_err(self.map_error())
    }

    fn insert_text(&self, _position: i32, _text: &str, _length: i32) -> fdo::Result<bool> {
        Err(unsupported())
    }

    fn copy_text(&self, _start_pos: i32, _end_pos: i32) -> fdo::Result<()> {
        Err(unsupported())
    }

    fn cut_text(&self, _start_pos: i32, _end_pos: i32) -> fdo::Result<bool> {
        Err(unsupported())
    }

    fn delete_text(&self, _start_pos: i32, _end_pos: i32) -> fdo::Result<bool> {
        Err(unsupported())
    }

    fn paste_text(&self, _position: i32) -> fdo::Result<bool> {
        Err(unsupported())
    }
}

fn super_error(e: accesskit_atspi_common::Error) -> fdo::Error {
    error(e)
}
fn granularity_from(value: u32) -> fdo::Result<accesskit_atspi_common::Granularity> {
    use accesskit_atspi_common::Granularity::*;
    match value {
        0 => Ok(Char),
        1 => Ok(Word),
        2 => Ok(Sentence),
        3 => Ok(Line),
        4 => Ok(Paragraph),
        _ => Err(fdo::Error::InvalidArgs("Invalid granularity".into())),
    }
}
fn scroll_from(value: u32) -> fdo::Result<accesskit_atspi_common::ScrollType> {
    use accesskit_atspi_common::ScrollType::*;
    match value {
        0 => Ok(TopLeft),
        1 => Ok(BottomRight),
        2 => Ok(TopEdge),
        3 => Ok(BottomEdge),
        4 => Ok(LeftEdge),
        5 => Ok(RightEdge),
        6 => Ok(Anywhere),
        _ => Err(fdo::Error::InvalidArgs("Invalid scroll type".into())),
    }
}

fn unsupported() -> fdo::Error {
    fdo::Error::NotSupported("Unsupported text operation".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atspi::{text_runs, Actions, Callback};
    use crate::draw::{Font, Rect};
    use crate::ui::{style, widgets::Content, Builder, Node as UiNode};
    use accesskit::{Action as UiAction, NodeId, Role, TreeId, TreeInfo, TreeUpdate};
    use accesskit_atspi_common::{Adapter, AppContext, WindowBounds};
    use tokio::sync::mpsc;

    fn fixture() -> (Adapter, mpsc::UnboundedReceiver<accesskit::ActionRequest>) {
        let mut font = Font::for_test();
        let mut b = Builder::new(
            UiNode::new(Content::Box, style().column().width(300.0)).semantic(
                1,
                Role::Window,
                "Test",
            ),
        );
        b.row(UiNode::new(Content::Box, style().height(24.0)).semantic(
            2,
            Role::Button,
            "Applications",
        ));
        b.row(
            UiNode::new(
                Content::Field {
                    placeholder: String::new(),
                },
                style().height(24.0),
            )
            .semantic(3, Role::SearchInput, "Search"),
        );
        let menu =
            b.row(UiNode::new(Content::Box, style().column()).semantic(4, Role::Menu, "Menu"));
        let mut item =
            UiNode::new(Content::Choice, style().height(24.0)).semantic(5, Role::MenuItem, "Open");
        item.semantic.as_mut().unwrap().set_selected(true);
        b.add(menu, item);
        let mut slider =
            UiNode::new(Content::Box, style().height(24.0)).semantic(6, Role::Slider, "Volume");
        let semantic = slider.semantic.as_mut().unwrap();
        semantic.set_numeric_value(50.0);
        semantic.set_min_numeric_value(0.0);
        semantic.set_max_numeric_value(100.0);
        b.row(slider);
        b.row(UiNode::new(Content::Box, style().height(24.0)).semantic(
            7,
            Role::Label,
            "Debian GNU/Linux",
        ));
        let mut tree = b.finish(&mut font);
        tree.set_value(1, "你好 world");
        let (mut nodes, _) = tree.accesskit(Rect::new(0.0, 0.0, 300.0, 48.0), (20, 30));
        text_runs(&mut nodes);
        let (tx, _changes) = mpsc::unbounded_channel();
        let (actions, rx) = mpsc::unbounded_channel();
        let adapter = Adapter::new(
            &AppContext::new(Some("Test".into())),
            Callback(tx),
            TreeUpdate {
                nodes,
                tree: Some(TreeInfo::new(NodeId(1))),
                tree_id: TreeId::ROOT,
                focus: NodeId(3),
            },
            true,
            WindowBounds::default(),
            Actions(actions),
        );
        (adapter, rx)
    }

    #[test]
    fn static_labels_have_names_on_the_accessibility_bus() {
        let (adapter, _) = fixture();
        let root = adapter.platform_node(adapter.root_id());
        let label = adapter.platform_node(root.child_at_index(4).unwrap().unwrap());
        assert_eq!(label.name().unwrap(), "Debian GNU/Linux");
    }

    #[test]
    fn paths_and_interface_answers_use_retained_nodes() {
        let (adapter, mut actions) = fixture();
        let button = adapter.platform_node(
            adapter
                .platform_node(adapter.root_id())
                .child_at_index(0)
                .unwrap()
                .unwrap(),
        );
        let path = node_path(adapter.id(), button.id());
        assert_eq!(
            path,
            format!(
                "/org/a11y/atspi/accessible/{}/{}",
                adapter.id(),
                u128::from(button.id())
            )
        );
        assert!(OwnedObjectPath::try_from(path).is_ok());
        let node = Node {
            accessible: Accessible::Node(button.clone()),
            bus: ":1.7".into(),
            desktop: Arc::new(OnceLock::new()),
        };
        assert_eq!(node.name().unwrap(), "Applications");
        assert!(node
            .interfaces()
            .unwrap()
            .contains(&"org.a11y.atspi.Action".into()));
        assert_eq!(
            Component(node).get_extents(0).unwrap(),
            ((20, 30, 300, 24),)
        );
        let action = Action(button);
        assert_eq!(action.n_actions().unwrap(), 1);
        assert!(action.do_action(0).unwrap());
        let request = actions.try_recv().unwrap();
        assert_eq!(request.target_node, NodeId(2));
        assert_eq!(request.action, UiAction::Click);
        assert!(!action.do_action(99).unwrap());
    }

    #[test]
    fn selection_and_value_answers_come_from_the_mapper() {
        let (adapter, _) = fixture();
        let root = adapter.platform_node(adapter.root_id());
        let menu = adapter.platform_node(root.child_at_index(2).unwrap().unwrap());
        let selection = SelectionInterface::new(":1.7".into(), menu);
        assert_eq!(selection.n_selected_children().unwrap(), 1);
        assert!(selection.is_child_selected(0).unwrap());
        assert!(selection.get_selected_child(-1).is_err());
        let slider = adapter.platform_node(root.child_at_index(3).unwrap().unwrap());
        let value = ValueInterface::new(slider);
        assert_eq!(value.minimum_value().unwrap(), 0.0);
        assert_eq!(value.maximum_value().unwrap(), 100.0);
        assert_eq!(value.current_value().unwrap(), 50.0);
    }

    #[test]
    fn text_and_editing_use_unicode_offsets_and_real_actions() {
        let (adapter, mut actions) = fixture();
        let field = adapter.platform_node(
            adapter
                .platform_node(adapter.root_id())
                .child_at_index(1)
                .unwrap()
                .unwrap(),
        );
        assert!(field.supports_text().unwrap());
        let text = TextInterface::new(field.clone());
        assert_eq!(text.character_count().unwrap(), 8);
        assert_eq!(text.get_text(0, 2).unwrap(), "你好");
        assert_eq!(text.caret_offset().unwrap(), 8);
        assert!(EditableTextInterface::new(field)
            .set_text_contents("dark")
            .unwrap());
        let request = actions.try_recv().unwrap();
        assert_eq!(request.action, UiAction::SetValue);
        assert!(
            matches!(request.data, Some(accesskit::ActionData::Value(v)) if v.as_ref() == "dark")
        );
        assert!(coord(99).is_err());
        assert!(granularity_from(99).is_err());
    }
}
