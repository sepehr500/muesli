//! PipeWire watcher: tracks live capture streams (Stream/Input/Audio) and publishes a
//! snapshot on every change. Event-driven — no polling.

use muesli_core::detect::PwNode;
use pipewire as pw;
use pw::types::ObjectType;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tokio::sync::mpsc::UnboundedSender;

fn dict_to_node(id: u32, d: &pw::spa::utils::dict::DictRef, prev: Option<&PwNode>) -> PwNode {
    let s = |k: &str| d.get(k).map(str::to_owned);
    let base = prev.cloned().unwrap_or(PwNode { id, ..Default::default() });
    PwNode {
        id,
        media_class: s("media.class").unwrap_or(base.media_class),
        app_name: s("application.name").or(base.app_name),
        binary: s("application.process.binary").or(base.binary),
        node_name: s("node.name").or(base.node_name),
        pid: d.get("application.process.id").and_then(|p| p.parse().ok()).or(base.pid),
        corked: d.get("pulse.corked").map(|v| v == "true").unwrap_or(base.corked),
    }
}

fn is_ours(n: &PwNode) -> bool {
    n.app_name.as_deref() == Some("muesli")
        || n.node_name.as_deref().is_some_and(|s| s.starts_with("muesli-"))
}

pub fn spawn(tx: UnboundedSender<Vec<PwNode>>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("muesli-detect".into())
        .spawn(move || loop {
            if let Err(e) = run(&tx) {
                tracing::warn!("pipewire watcher stopped: {e}; restarting in 3 s");
            }
            if tx.is_closed() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
        })
        .expect("spawn detector")
}

fn run(tx: &UnboundedSender<Vec<PwNode>>) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;
    let registry_weak = registry.downgrade();

    // Quit (and restart) if the PipeWire connection drops.
    let ml = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |_, _, res, msg| {
            tracing::warn!("pipewire core error {res}: {msg}");
            if res == -32 {
                ml.quit(); // EPIPE: server went away
            }
        })
        .register();

    let nodes: Rc<RefCell<HashMap<u32, PwNode>>> = Rc::default();
    let bound: Rc<RefCell<HashMap<u32, (pw::node::Node, pw::node::NodeListener)>>> = Rc::default();

    let publish = {
        let (nodes, tx) = (nodes.clone(), tx.clone());
        Rc::new(move || {
            let snap: Vec<PwNode> = nodes
                .borrow()
                .values()
                .filter(|n| n.media_class == "Stream/Input/Audio" && !is_ours(n))
                .cloned()
                .collect();
            let _ = tx.send(snap);
        })
    };

    let _reg_listener = registry
        .add_listener_local()
        .global({
            let (nodes, bound, publish) = (nodes.clone(), bound.clone(), publish.clone());
            move |obj| {
                if obj.type_ != ObjectType::Node {
                    return;
                }
                let Some(props) = obj.props else { return };
                if props.get("media.class") != Some("Stream/Input/Audio") {
                    return;
                }
                let id = obj.id;
                let node = dict_to_node(id, props, None);
                nodes.borrow_mut().insert(id, node);
                publish();
                // Bind for full/updated props (application.*, pulse.corked).
                let Some(registry) = registry_weak.upgrade() else { return };
                let Ok(proxy) = registry.bind::<pw::node::Node, _>(obj) else { return };
                let listener = proxy
                    .add_listener_local()
                    .info({
                        let (nodes, publish) = (nodes.clone(), publish.clone());
                        move |info| {
                            if let Some(p) = info.props() {
                                let updated = {
                                    let mut map = nodes.borrow_mut();
                                    let next = dict_to_node(id, p, map.get(&id));
                                    let changed = map.get(&id) != Some(&next);
                                    map.insert(id, next);
                                    changed
                                };
                                if updated {
                                    publish();
                                }
                            }
                        }
                    })
                    .register();
                bound.borrow_mut().insert(id, (proxy, listener));
            }
        })
        .global_remove({
            let (nodes, bound, publish) = (nodes.clone(), bound.clone(), publish.clone());
            move |id| {
                bound.borrow_mut().remove(&id);
                if nodes.borrow_mut().remove(&id).is_some() {
                    publish();
                }
            }
        })
        .register();

    publish();
    mainloop.run();
    Ok(())
}

/// The live Hyprland instance signature. The inherited env var goes stale when Hyprland
/// restarts under a long-running service, so fall back to the newest instance socket.
fn hyprland_instance() -> Option<String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let hypr = std::path::Path::new(&runtime).join("hypr");
    let accepts = |p: std::path::PathBuf| std::os::unix::net::UnixStream::connect(p).is_ok();
    let live = |sig: &str| accepts(hypr.join(sig).join(".socket.sock"));
    if let Some(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().filter(|s| live(s)) {
        return Some(sig);
    }
    std::fs::read_dir(&hypr)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| accepts(e.path().join(".socket.sock")))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .and_then(|e| e.file_name().into_string().ok())
}

/// Current windows via `hyprctl clients -j` (empty if Hyprland isn't available).
pub async fn windows() -> Vec<muesli_core::detect::Window> {
    let mut cmd = tokio::process::Command::new("hyprctl");
    cmd.args(["clients", "-j"]);
    if let Some(sig) = hyprland_instance() {
        cmd.env("HYPRLAND_INSTANCE_SIGNATURE", sig);
    }
    let out = cmd.output().await;
    out.ok()
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok())
        .map(|v| muesli_core::detect::windows_from_hyprctl(&v))
        .unwrap_or_default()
}
