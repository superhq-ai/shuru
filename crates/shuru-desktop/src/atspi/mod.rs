//! The accessibility bus on the desktop tokio runtime. AccessKit maps the
//! retained nodes; this module owns object paths, interfaces and signals.

mod interfaces;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use accesskit::{ActionHandler, ActionRequest, Node, NodeId, Role, TreeUpdate};
use accesskit_atspi_common::{
    simplified::{Accessible, EventData},
    Adapter, AdapterCallback, AppContext, FullNodeId, InterfaceSet, PlatformNode, WindowBounds,
};
use tokio::sync::mpsc;
use zbus::{Connection, OwnedObjectPath, Variant};

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub tree: TreeUpdate,
    pub focused: bool,
}

const ROOT: &str = "/org/a11y/atspi/accessible/root";

fn node_path(adapter: usize, node: FullNodeId) -> String {
    format!("/org/a11y/atspi/accessible/{adapter}/{}", u128::from(node))
}

fn address(bus: &str, path: &str) -> (String, OwnedObjectPath) {
    (
        bus.into(),
        OwnedObjectPath::try_from(path.to_string()).expect("valid accessibility path"),
    )
}

/// Adds text runs so the common mapper can answer AT-SPI text ranges.
pub fn text_runs(nodes: &mut Vec<(NodeId, Node)>) {
    let mut used: HashSet<_> = nodes.iter().map(|(id, _)| *id).collect();
    let mut runs = Vec::new();
    for (id, node) in nodes.iter_mut() {
        if !matches!(node.role(), Role::SearchInput | Role::TextInput) {
            continue;
        }
        let mut run_id = NodeId(id.0 ^ 0x7e57_0000_0000_0000);
        while !used.insert(run_id) {
            run_id.0 = run_id.0.wrapping_add(1);
        }
        let value = node.value().unwrap_or("").to_string();
        let mut run = Node::new(Role::TextRun);
        run.set_value(value.clone());
        run.set_character_lengths(
            value
                .chars()
                .map(|c| c.len_utf8() as u8)
                .collect::<Vec<_>>(),
        );
        if let Some(bounds) = node.bounds() {
            run.set_bounds(bounds);
        }
        node.set_children([run_id]);
        let end = accesskit::TextPosition {
            node: run_id,
            character_index: value.chars().count(),
        };
        node.set_text_selection(accesskit::TextSelection {
            anchor: end,
            focus: end,
        });
        node.add_action(accesskit::Action::SetValue);
        runs.push((run_id, run));
    }
    nodes.extend(runs);
}

enum Change {
    Register(PlatformNode),
    Event(accesskit_atspi_common::simplified::Event),
}

struct Callback(mpsc::UnboundedSender<Change>);
impl AdapterCallback for Callback {
    fn register_interfaces(&self, adapter: &Adapter, id: FullNodeId, _: InterfaceSet) {
        let _ = self.0.send(Change::Register(adapter.platform_node(id)));
    }
    fn unregister_interfaces(&self, adapter: &Adapter, id: FullNodeId, _: InterfaceSet) {
        let _ = self.0.send(Change::Register(adapter.platform_node(id)));
    }
    fn emit_event(&self, adapter: &Adapter, event: accesskit_atspi_common::Event) {
        let _ = self.0.send(Change::Event(
            accesskit_atspi_common::simplified::Event::new(adapter, event),
        ));
    }
}

struct Actions(mpsc::UnboundedSender<ActionRequest>);
impl ActionHandler for Actions {
    fn do_action(&mut self, request: ActionRequest) {
        let _ = self.0.send(request);
    }
}

async fn register(conn: &Connection, node: interfaces::Node) -> zbus::Result<()> {
    let path = match &node.accessible {
        Accessible::Root(_) => ROOT.into(),
        Accessible::Node(n) => node_path(n.adapter_id(), n.id()),
    };
    let server = conn.object_server();
    server.at(path.clone(), node.clone()).await?;
    if let Accessible::Node(n) = &node.accessible {
        if !n.supports_action().unwrap_or(false) {
            let _ = server.remove::<interfaces::Action, _>(path.as_str()).await;
        }
        if !n.supports_text().unwrap_or(false) {
            let _ = server
                .remove::<interfaces::TextInterface, _>(path.as_str())
                .await;
        }
        if !n.supports_editable_text().unwrap_or(false) {
            let _ = server
                .remove::<interfaces::EditableTextInterface, _>(path.as_str())
                .await;
        }
        if !n.supports_value().unwrap_or(false) {
            let _ = server
                .remove::<interfaces::ValueInterface, _>(path.as_str())
                .await;
        }
        if !n.supports_selection().unwrap_or(false) {
            let _ = server
                .remove::<interfaces::SelectionInterface, _>(path.as_str())
                .await;
        }
        if n.supports_action().unwrap_or(false) {
            server
                .at(path.clone(), interfaces::Action(n.clone()))
                .await?;
        }
        if n.supports_component().unwrap_or(false) {
            server
                .at(path.clone(), interfaces::Component(node.clone()))
                .await?;
        }
        if n.supports_text().unwrap_or(false) {
            server
                .at(path.clone(), interfaces::TextInterface::new(n.clone()))
                .await?;
        }
        if n.supports_editable_text().unwrap_or(false) {
            server
                .at(
                    path.clone(),
                    interfaces::EditableTextInterface::new(n.clone()),
                )
                .await?;
        }
        if n.supports_value().unwrap_or(false) {
            server
                .at(path.clone(), interfaces::ValueInterface::new(n.clone()))
                .await?;
        }
        if n.supports_selection().unwrap_or(false) {
            server
                .at(
                    path,
                    interfaces::SelectionInterface::new(node.bus.clone(), n.clone()),
                )
                .await?;
        }
    }
    Ok(())
}

async fn remove(conn: &Connection, path: &str) {
    let server = conn.object_server();
    let _ = server.remove::<interfaces::Action, _>(path).await;
    let _ = server.remove::<interfaces::Component, _>(path).await;
    let _ = server.remove::<interfaces::TextInterface, _>(path).await;
    let _ = server
        .remove::<interfaces::EditableTextInterface, _>(path)
        .await;
    let _ = server.remove::<interfaces::ValueInterface, _>(path).await;
    let _ = server
        .remove::<interfaces::SelectionInterface, _>(path)
        .await;
    let _ = server.remove::<interfaces::Node, _>(path).await;
}

fn reference(bus: &str, accessible: Accessible) -> (String, OwnedObjectPath) {
    match accessible {
        Accessible::Root(_) => address(bus, ROOT),
        Accessible::Node(n) => address(bus, &node_path(n.adapter_id(), n.id())),
    }
}

async fn emit(
    conn: &Connection,
    event: accesskit_atspi_common::simplified::Event,
) -> zbus::Result<()> {
    let mut parts = event.kind.splitn(3, ':');
    let family = parts.next().unwrap_or_default();
    if !matches!(family, "object" | "window" | "document") {
        return Ok(());
    }
    let member: String = parts
        .next()
        .unwrap_or_default()
        .split('-')
        .flat_map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .into_iter()
                .flat_map(char::to_uppercase)
                .chain(chars)
                .collect::<Vec<_>>()
        })
        .collect();
    let kind = parts.next().unwrap_or_default();
    let bus = conn.unique_name().expect("bus connection").as_str();
    let (_, path) = reference(bus, event.source);
    let data = match event.data {
        Some(EventData::U32(v)) => Variant::from(v),
        Some(EventData::F64(v)) => Variant::from(v),
        Some(EventData::String(v)) => Variant::from(v),
        Some(EventData::Rect(v)) => Variant::from((v.x, v.y, v.width, v.height)),
        Some(EventData::Accessible(v)) => Variant::from(reference(bus, v)),
        None => Variant::from(0i32),
    };
    let interface = format!(
        "org.a11y.atspi.Event.{}",
        match family {
            "object" => "Object",
            "window" => "Window",
            _ => "Document",
        }
    );
    conn.emit_signal(
        None::<&str>,
        path,
        interface,
        member,
        &(
            kind,
            event.detail1,
            event.detail2,
            data,
            HashMap::<String, Variant<'_>>::new(),
        ),
    )
    .await
}

async fn drain(
    conn: &Connection,
    changes: &mut mpsc::UnboundedReceiver<Change>,
    desktop: &Arc<OnceLock<(String, OwnedObjectPath)>>,
) -> zbus::Result<()> {
    while let Ok(change) = changes.try_recv() {
        match change {
            Change::Register(n) => {
                if n.interfaces().is_err() {
                    remove(conn, &node_path(n.adapter_id(), n.id())).await;
                } else {
                    register(
                        conn,
                        interfaces::Node {
                            accessible: Accessible::Node(n),
                            bus: conn.unique_name().unwrap().to_string(),
                            desktop: desktop.clone(),
                        },
                    )
                    .await?;
                }
            }
            Change::Event(event) => emit(conn, event).await?,
        }
    }
    Ok(())
}

pub async fn run(
    name: String,
    mut trees: mpsc::UnboundedReceiver<Snapshot>,
    actions: mpsc::UnboundedSender<ActionRequest>,
) {
    if let Err(e) = publish(name, &mut trees, actions).await {
        eprintln!("SuperHQ: accessibility bus unavailable: {e}");
    }
}

async fn publish(
    name: String,
    trees: &mut mpsc::UnboundedReceiver<Snapshot>,
    actions: mpsc::UnboundedSender<ActionRequest>,
) -> zbus::Result<()> {
    let session = Connection::session().await?;
    let proxy = zbus::Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus").await?;
    let bus_address: String = proxy.call("GetAddress", &()).await?;
    let conn = zbus::connection::Builder::address(bus_address.as_str())
        .build()
        .await?;
    let Some(initial) = trees.recv().await else {
        return Ok(());
    };
    let (tx, mut changes) = mpsc::unbounded_channel();
    let context = AppContext::new(Some(name));
    let mut adapter = Adapter::new(
        &context,
        Callback(tx),
        initial.tree,
        initial.focused,
        WindowBounds::default(),
        Actions(actions),
    );
    let desktop = Arc::new(OnceLock::new());
    let root = adapter.platform_root();
    register(
        &conn,
        interfaces::Node {
            accessible: Accessible::Root(root.clone()),
            bus: conn.unique_name().unwrap().to_string(),
            desktop: desktop.clone(),
        },
    )
    .await?;
    conn.object_server()
        .at(ROOT, interfaces::Application(root))
        .await?;
    drain(&conn, &mut changes, &desktop).await?;
    let registry = zbus::Proxy::new(
        &conn,
        "org.a11y.atspi.Registry",
        "/org/a11y/atspi/accessible/root",
        "org.a11y.atspi.Socket",
    )
    .await?;
    let (parent,): ((String, OwnedObjectPath),) = registry
        .call(
            "Embed",
            &(address(conn.unique_name().unwrap().as_str(), ROOT),),
        )
        .await?;
    let _ = desktop.set(parent);
    while let Some(mut update) = trees.recv().await {
        adapter.update_window_focus_state(update.focused);
        update.tree.tree = None;
        adapter.update(update.tree);
        drain(&conn, &mut changes, &desktop).await?;
    }
    let _: zbus::Result<()> = registry
        .call(
            "Unembed",
            &(address(conn.unique_name().unwrap().as_str(), ROOT),),
        )
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_emit_bounds_state_and_children_events_without_a_bus() {
        let root_id = NodeId(1);
        let mut root = Node::new(Role::Window);
        root.set_bounds(accesskit::Rect::new(0.0, 0.0, 300.0, 200.0));
        let initial = TreeUpdate {
            nodes: vec![(root_id, root.clone())],
            tree: Some(accesskit::TreeInfo::new(root_id)),
            tree_id: accesskit::TreeId::ROOT,
            focus: root_id,
        };
        let (tx, mut changes) = mpsc::unbounded_channel();
        let (actions, _) = mpsc::unbounded_channel();
        let mut adapter = Adapter::new(
            &AppContext::new(Some("Test".into())),
            Callback(tx),
            initial,
            true,
            WindowBounds::default(),
            Actions(actions),
        );
        while changes.try_recv().is_ok() {}
        root.set_children([NodeId(2)]);
        root.set_bounds(accesskit::Rect::new(10.0, 20.0, 310.0, 220.0));
        let mut child = Node::new(Role::Button);
        child.set_label("Open");
        child.add_action(accesskit::Action::Focus);
        adapter.update(TreeUpdate {
            nodes: vec![(root_id, root), (NodeId(2), child)],
            tree: None,
            tree_id: accesskit::TreeId::ROOT,
            focus: NodeId(2),
        });
        let mut kinds = Vec::new();
        while let Ok(change) = changes.try_recv() {
            if let Change::Event(event) = change {
                kinds.push(event.kind);
            }
        }
        assert!(kinds.iter().any(|k| k == "object:children-changed:add"));
        assert!(kinds.iter().any(|k| k == "object:bounds-changed"));
        assert!(kinds.iter().any(|k| k == "object:state-changed:focused"));
    }
}

#[cfg(test)]
mod text_event_tests {
    use super::*;

    fn state(value: &str) -> TreeUpdate {
        let mut root = Node::new(Role::Window);
        root.set_children([NodeId(2)]);
        let mut field = Node::new(Role::SearchInput);
        field.set_value(value);
        field.add_action(accesskit::Action::SetValue);
        let mut nodes = vec![(NodeId(1), root), (NodeId(2), field)];
        text_runs(&mut nodes);
        TreeUpdate {
            nodes,
            tree: Some(accesskit::TreeInfo::new(NodeId(1))),
            tree_id: accesskit::TreeId::ROOT,
            focus: NodeId(2),
        }
    }

    #[test]
    fn changing_search_emits_text_and_caret_events() {
        let (tx, mut changes) = mpsc::unbounded_channel();
        let (actions, _) = mpsc::unbounded_channel();
        let mut adapter = Adapter::new(
            &AppContext::new(Some("Test".into())),
            Callback(tx),
            state("hello"),
            true,
            WindowBounds::default(),
            Actions(actions),
        );
        while changes.try_recv().is_ok() {}
        let mut update = state("你好");
        update.tree = None;
        adapter.update(update);
        let mut kinds = Vec::new();
        while let Ok(change) = changes.try_recv() {
            if let Change::Event(event) = change {
                kinds.push(event.kind);
            }
        }
        assert!(kinds.iter().any(|k| k == "object:text-changed:insert"));
        assert!(kinds.iter().any(|k| k == "object:text-changed:delete"));
        assert!(kinds.iter().any(|k| k == "object:text-caret-moved"));
    }
}
