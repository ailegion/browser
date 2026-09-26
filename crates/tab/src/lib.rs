//! Tab event loop. See plan/02-architecture.md, section "Tab event loop".
//!
//! One OS thread per tab. It owns the document, styles, layout and the
//! scene it paints, and talks to the shell and the network only through
//! messages. A panic anywhere in the tab is caught at the thread boundary
//! and reported as `TabToShell::Crashed`.

#![forbid(unsafe_code)]

mod document;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use browser_ipc_types::{NetToTab, ShellToTab, TabId, TabToShell, Viewport};
use browser_net::NetService;
use vello::Scene;

use document::TabState;

/// Everything a tab sends to the shell.
pub enum TabOutput {
    Message(TabToShell),
    /// A freshly painted frame in physical pixels.
    Frame(Scene),
}

impl std::fmt::Debug for TabOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TabOutput::Message(m) => f.debug_tuple("Message").field(m).finish(),
            TabOutput::Frame(_) => f.write_str("Frame(..)"),
        }
    }
}

/// Receives tab output on the tab thread; the shell wakes its event loop.
pub type OutputSink = Box<dyn Fn(TabId, TabOutput) + Send>;

/// Events the tab thread waits on.
enum TabEvent {
    Shell(ShellToTab),
    Net(NetToTab),
}

/// Shell-side handle to a tab thread.
#[derive(Debug)]
pub struct TabHandle {
    id: TabId,
    sender: Sender<TabEvent>,
    thread: Option<JoinHandle<()>>,
}

impl TabHandle {
    pub fn id(&self) -> TabId {
        self.id
    }

    /// Send a message; a dead tab drops it silently.
    pub fn send(&self, msg: ShellToTab) {
        let _ = self.sender.send(TabEvent::Shell(msg));
    }

    /// Ask the tab to exit and wait for its thread to finish.
    pub fn close(mut self) {
        let _ = self.sender.send(TabEvent::Shell(ShellToTab::Close));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TabHandle {
    fn drop(&mut self) {
        let _ = self.sender.send(TabEvent::Shell(ShellToTab::Close));
    }
}

/// Start a tab thread.
pub fn spawn_tab(id: TabId, net: Arc<NetService>, viewport: Viewport, output: OutputSink) -> TabHandle {
    let (sender, receiver) = channel::<TabEvent>();
    let net_sender = sender.clone();
    let thread = std::thread::Builder::new()
        .name(format!("tab-{}", id.0))
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let output = Arc::new(output);
            let result = catch_unwind(AssertUnwindSafe(|| {
                run(id, net, viewport, receiver, net_sender, output.clone());
            }));
            if let Err(payload) = result {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_owned());
                tracing::error!(tab = id.0, "tab crashed: {message}");
                output(id, TabOutput::Message(TabToShell::Crashed { message }));
            }
        })
        .expect("spawn tab thread");
    TabHandle {
        id,
        sender,
        thread: Some(thread),
    }
}

fn run(
    id: TabId,
    net: Arc<NetService>,
    viewport: Viewport,
    inbox: Receiver<TabEvent>,
    net_sender: Sender<TabEvent>,
    output: Arc<OutputSink>,
) {
    let net_sink: browser_net::Sink = Arc::new(move |ev| {
        let _ = net_sender.send(TabEvent::Net(ev));
    });
    let mut state = TabState::new(id, net, net_sink, viewport, output);

    loop {
        // Block for the first event, then drain everything queued so that a
        // burst of network chunks produces one render, not many.
        let Ok(first) = inbox.recv() else { break };
        let mut events = vec![first];
        while let Ok(more) = inbox.try_recv() {
            events.push(more);
            if events.len() > 256 {
                break;
            }
        }
        let mut closing = false;
        for ev in events {
            match ev {
                TabEvent::Shell(ShellToTab::Close) => {
                    closing = true;
                    break;
                }
                TabEvent::Shell(msg) => state.handle_shell(msg),
                TabEvent::Net(msg) => state.handle_net(msg),
            }
        }
        if closing {
            break;
        }
        state.flush();
    }
    state.send(TabToShell::Closed);
}
