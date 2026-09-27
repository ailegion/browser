//! Tab event loop. See plan/02-architecture.md, section "Tab event loop".
//!
//! One OS thread per tab. It owns the document, styles, layout and the
//! scene it paints, and talks to the shell and the network only through
//! messages. A panic anywhere in the tab is caught at the thread boundary
//! and reported as `TabToShell::Crashed`; the thread then shows a crash
//! page in a fresh state and keeps serving the tab, so the user can
//! navigate on or close it. Closing the tab ends the thread and drops
//! everything it owned.

#![forbid(unsafe_code)]

mod document;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use browser_ipc_types::{NetToTab, ShellToTab, TabId, TabToShell, Viewport};
use browser_net::NetService;
use url::Url;
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

    /// Ask the tab to exit and wait for its thread to finish. When this
    /// returns, the thread is gone and everything it owned is dropped.
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
        .spawn(move || tab_thread(id, net, viewport, receiver, net_sender, output))
        .expect("spawn tab thread");
    TabHandle {
        id,
        sender,
        thread: Some(thread),
    }
}

/// The tab thread: run the loop; if it panics, report it, replace the
/// state with one showing a crash page, and run again.
fn tab_thread(
    id: TabId,
    net: Arc<NetService>,
    viewport: Viewport,
    inbox: Receiver<TabEvent>,
    net_sender: Sender<TabEvent>,
    output: OutputSink,
) {
    // The last URL the tab reported, so the crash page can offer it again.
    let last_url: Arc<Mutex<Option<Url>>> = Arc::new(Mutex::new(None));
    let seen = last_url.clone();
    let output: Arc<OutputSink> = Arc::new(Box::new(move |id, out| {
        if let TabOutput::Message(TabToShell::StateChanged { url, .. }) = &out
            && let Ok(mut last) = seen.lock()
        {
            *last = Some(url.clone());
        }
        output(id, out);
    }));
    let net_sink: browser_net::Sink = Arc::new(move |ev| {
        let _ = net_sender.send(TabEvent::Net(ev));
    });

    let mut state = TabState::new(id, net.clone(), net_sink.clone(), viewport, output.clone());
    loop {
        let result = catch_unwind(AssertUnwindSafe(|| run(&mut state, &inbox)));
        match result {
            Ok(()) => break,
            Err(payload) => {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_owned());
                tracing::error!(tab = id.0, "tab crashed: {message}");
                output(id, TabOutput::Message(TabToShell::Crashed { message: message.clone() }));
                // The old state is unwound and may be inconsistent; drop it
                // whole and start over with the crash page.
                let url = last_url.lock().ok().and_then(|l| l.clone());
                state = TabState::new(id, net.clone(), net_sink.clone(), viewport, output.clone());
                state.handle_shell(ShellToTab::Navigate {
                    url: crash_page_url(&message, url.as_ref()),
                });
            }
        }
    }
    state.send(TabToShell::Closed);
}

/// The page shown after a panic: what happened, and a link back to where
/// the tab was.
fn crash_page_url(message: &str, url: Option<&Url>) -> Url {
    let again = url
        .map(|u| {
            format!(
                "<p><a href=\"{}\">Try again</a></p>",
                document::escape(u.as_str()).replace('"', "&quot;")
            )
        })
        .unwrap_or_default();
    let html = format!(
        "<!doctype html><html><head><title>Tab crashed</title></head>\
         <body style='font-family:sans-serif;margin:40px'><h1>This tab crashed</h1>\
         <p>The page was closed because of an internal error. The rest of the browser is unaffected.</p>\
         <p><code>{}</code></p>{again}</body></html>",
        document::escape(message)
    );
    let encoded: String = html
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    Url::parse(&format!("data:text/html,{encoded}")).expect("data url")
}

/// The event loop proper. Returns when the tab is closed.
fn run(state: &mut TabState, inbox: &Receiver<TabEvent>) {
    loop {
        // Block for the first event (or the tab's next timer), then drain
        // everything queued so that a burst of network chunks produces one
        // render, not many.
        let mut events = Vec::new();
        let first = match state.next_wake() {
            Some(deadline) => {
                let wait = deadline.saturating_duration_since(std::time::Instant::now());
                match inbox.recv_timeout(wait) {
                    Ok(ev) => Some(ev),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match inbox.recv() {
                Ok(ev) => Some(ev),
                Err(_) => return,
            },
        };
        events.extend(first);
        while let Ok(more) = inbox.try_recv() {
            events.push(more);
            if events.len() > 256 {
                break;
            }
        }
        for ev in events {
            match ev {
                TabEvent::Shell(ShellToTab::Close) => return,
                TabEvent::Shell(msg) => state.handle_shell(msg),
                TabEvent::Net(msg) => state.handle_net(msg),
            }
        }
        state.tick();
        state.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Weak;
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    /// A spawned tab whose messages arrive on a channel. The token is an
    /// `Arc` the sink holds; its `Weak` shows whether the tab still lives.
    struct Spawned {
        tab: TabHandle,
        messages: Receiver<TabToShell>,
        token: Weak<()>,
    }

    fn spawn(id: u64, net: &Arc<NetService>) -> Spawned {
        let (tx, messages) = channel();
        let token = Arc::new(());
        let weak = Arc::downgrade(&token);
        let sink: OutputSink = Box::new(move |_, out| {
            let _keep = &token;
            if let TabOutput::Message(m) = out {
                let _ = tx.send(m);
            }
        });
        let viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale_factor: 1.0,
        };
        Spawned {
            tab: spawn_tab(TabId(id), net.clone(), viewport, sink),
            messages,
            token: weak,
        }
    }

    fn data_url(html: &str) -> Url {
        let encoded: String = html
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        Url::parse(&format!("data:text/html,{encoded}")).expect("data url")
    }

    impl Spawned {
        /// Wait until the tab reports a finished load with this title.
        fn wait_for_title(&self, title: &str) {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let wait = deadline.saturating_duration_since(std::time::Instant::now());
                match self.messages.recv_timeout(wait) {
                    Ok(TabToShell::StateChanged {
                        title: Some(t),
                        loading: false,
                        ..
                    }) if t == title => return,
                    Ok(_) => {}
                    Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                        panic!("tab never reported a finished load titled {title:?}")
                    }
                }
            }
        }

        fn wait_for_crash(&self) -> String {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let wait = deadline.saturating_duration_since(std::time::Instant::now());
                match self.messages.recv_timeout(wait) {
                    Ok(TabToShell::Crashed { message }) => return message,
                    Ok(_) => {}
                    Err(_) => panic!("tab never reported a crash"),
                }
            }
        }
    }

    #[test]
    fn closing_a_tab_ends_its_thread_and_drops_what_it_owned() {
        let net = Arc::new(NetService::new().expect("net"));
        let s = spawn(1, &net);
        s.tab.send(ShellToTab::Navigate {
            url: data_url("<title>Alive</title><p>hello</p>"),
        });
        s.wait_for_title("Alive");
        assert!(s.token.upgrade().is_some(), "the sink lives while the tab does");
        assert!(Arc::strong_count(&net) > 1, "the tab holds the net service");

        let Spawned { tab, messages, token } = s;
        tab.close();
        // `close` joined the thread: everything it owned is gone.
        assert!(token.upgrade().is_none(), "the sink, and with it the tab state, was dropped");
        assert_eq!(Arc::strong_count(&net), 1, "the tab's reference to the net service was released");
        let last = std::iter::from_fn(|| messages.try_recv().ok()).last();
        assert!(matches!(last, Some(TabToShell::Closed)), "the tab said goodbye: {last:?}");
    }

    #[test]
    fn a_panicking_tab_shows_a_crash_page_and_keeps_serving_while_others_run() {
        let net = Arc::new(NetService::new().expect("net"));
        let a = spawn(1, &net);
        let b = spawn(2, &net);
        a.tab.send(ShellToTab::Navigate {
            url: data_url("<title>A</title>"),
        });
        b.tab.send(ShellToTab::Navigate {
            url: data_url("<title>B</title>"),
        });
        a.wait_for_title("A");
        b.wait_for_title("B");

        a.tab.send(ShellToTab::Navigate {
            url: Url::parse("about:crash").expect("url"),
        });
        let message = a.wait_for_crash();
        assert!(message.contains("about:crash"), "{message}");
        // The crash page follows on the same thread, then the tab goes on.
        a.wait_for_title("Tab crashed");
        a.tab.send(ShellToTab::Navigate {
            url: data_url("<title>A again</title>"),
        });
        a.wait_for_title("A again");

        // The other tab never noticed.
        b.tab.send(ShellToTab::Navigate {
            url: data_url("<title>B again</title>"),
        });
        b.wait_for_title("B again");

        a.tab.close();
        b.tab.close();
        assert!(a.token.upgrade().is_none() && b.token.upgrade().is_none());
        assert_eq!(Arc::strong_count(&net), 1);
    }
}
