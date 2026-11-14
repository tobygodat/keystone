//! The Chrome DevTools Protocol over `--remote-debugging-pipe`: Chrome reads commands
//! from file descriptor 3 and writes replies and events to 4, each a JSON message ended
//! by a NUL byte. No port is opened, so no other process can attach to the browser.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Called on the reader thread with an event's method and params.
pub type Listener = Arc<dyn Fn(&str, &Value) + Send + Sync>;

/// A message larger than this ends the connection rather than growing without bound.
const MAX_MESSAGE: usize = 256 * 1024 * 1024;

#[derive(Default)]
struct Shared {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<Result<Value, String>>>>,
    /// Event listeners by CDP session ID; the empty string is the browser itself.
    listeners: Mutex<HashMap<String, Listener>>,
    closed: AtomicBool,
}

impl Shared {
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        for (_, reply) in self.pending.lock().unwrap().drain() {
            let _ = reply.send(Err(CLOSED.into()));
        }
    }
}

pub const CLOSED: &str = "The browser closed";

/// A CDP connection over any byte stream pair, so tests can stand in for Chrome.
pub struct Transport {
    shared: Arc<Shared>,
    outgoing: Mutex<Option<Sender<Vec<u8>>>>,
}

impl Transport {
    pub fn new(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) -> Self {
        let shared = Arc::new(Shared::default());
        let (outgoing, queue) = mpsc::channel::<Vec<u8>>();
        let writing = shared.clone();
        let _ = thread::Builder::new()
            .name("keystone-cdp-write".into())
            .spawn(move || {
                let mut writer = writer;
                for message in queue {
                    if writer.write_all(&message).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
                writing.close();
            });
        let reading = shared.clone();
        let _ = thread::Builder::new()
            .name("keystone-cdp-read".into())
            .spawn(move || read_loop(BufReader::new(reader), &reading));
        Self {
            shared,
            outgoing: Mutex::new(Some(outgoing)),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// Sends `method` and waits up to `timeout` for its result.
    pub fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let (reply, answer) = mpsc::channel();
        let id = self.shared.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.shared.pending.lock().unwrap().insert(id, reply);
        if !self.write(id, session, method, params) {
            self.shared.pending.lock().unwrap().remove(&id);
            return Err(CLOSED.into());
        }
        match answer.recv_timeout(timeout) {
            Ok(Err(error)) if error == CLOSED => Err(error),
            Ok(result) => result.map_err(|error| format!("{method}: {error}")),
            Err(_) => {
                self.shared.pending.lock().unwrap().remove(&id);
                Err(if self.is_closed() {
                    CLOSED.into()
                } else {
                    format!("The browser did not answer {method} in time")
                })
            }
        }
    }

    /// Sends `method` without waiting; its reply is dropped.
    pub fn send(&self, session: Option<&str>, method: &str, params: Value) {
        let id = self.shared.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.write(id, session, method, params);
    }

    fn write(&self, id: u64, session: Option<&str>, method: &str, params: Value) -> bool {
        if self.is_closed() {
            return false;
        }
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(0);
        self.outgoing
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|outgoing| outgoing.send(bytes).is_ok())
    }

    /// Routes events of CDP `session` to `listener`, replacing any earlier one.
    pub fn listen(&self, session: &str, listener: Listener) {
        self.shared
            .listeners
            .lock()
            .unwrap()
            .insert(session.to_owned(), listener);
    }

    pub fn forget(&self, session: &str) {
        self.shared.listeners.lock().unwrap().remove(session);
    }

    /// Stops writing, which ends the connection.
    fn shut(&self) {
        self.outgoing.lock().unwrap().take();
    }
}

fn read_loop(mut reader: impl BufRead, shared: &Shared) {
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader
            .by_ref()
            .take(MAX_MESSAGE as u64)
            .read_until(0, &mut buffer)
        {
            Ok(0) | Err(_) => break,
            Ok(_) if buffer.last() != Some(&0) => break,
            Ok(_) => {}
        }
        buffer.pop();
        let Ok(message) = serde_json::from_slice::<Value>(&buffer) else {
            continue;
        };
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            let Some(reply) = shared.pending.lock().unwrap().remove(&id) else {
                continue;
            };
            let result = match message.get("error") {
                Some(error) => Err(error["message"]
                    .as_str()
                    .unwrap_or("unknown error")
                    .to_owned()),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = reply.send(result);
        } else if let Some(method) = message.get("method").and_then(Value::as_str) {
            let session = message
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let listener = shared.listeners.lock().unwrap().get(session).cloned();
            if let Some(listener) = listener {
                listener(method, message.get("params").unwrap_or(&Value::Null));
            }
        }
    }
    shared.close();
}

/// Headless Chrome, or another Chromium, run as Keystone's child.
pub struct Chrome {
    transport: Transport,
    child: Mutex<Child>,
}

impl std::ops::Deref for Chrome {
    type Target = Transport;

    fn deref(&self) -> &Transport {
        &self.transport
    }
}

impl Chrome {
    /// `KEYSTONE_BROWSER_PATH`, else the first installed Chrome, Chromium, Edge or Brave.
    pub fn find() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("KEYSTONE_BROWSER_PATH") {
            return Some(path.into());
        }
        let apps = [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Chromium.app/Contents/MacOS/Chromium",
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "Brave Browser.app/Contents/MacOS/Brave Browser",
        ];
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let roots = [
            Some(PathBuf::from("/Applications")),
            home.map(|h| h.join("Applications")),
        ];
        let bundled = roots
            .iter()
            .flatten()
            .flat_map(|root| apps.iter().map(move |app| root.join(app)))
            .find(|path| path.is_file());
        bundled.or_else(|| {
            [
                "google-chrome",
                "google-chrome-stable",
                "chromium",
                "chromium-browser",
                "microsoft-edge",
                "brave-browser",
            ]
            .into_iter()
            .find_map(crate::find::program)
        })
    }

    /// Starts `binary` headless with its profile in `profile`.
    #[cfg(unix)]
    pub fn launch(binary: &Path, profile: &Path) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        std::fs::create_dir_all(profile)?;
        let (child_reads, we_write) = io::pipe()?;
        let (we_read, child_writes) = io::pipe()?;
        let (read_fd, write_fd) = (child_reads.as_raw_fd(), child_writes.as_raw_fd());
        let mut command = Command::new(binary);
        command
            .arg("--headless")
            .arg("--remote-debugging-pipe")
            .arg(format!("--user-data-dir={}", profile.display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--disable-sync",
                "--mute-audio",
                // Headless Chrome has nowhere to show a permission prompt, so one waited
                // forever. Every prompt is refused at once instead.
                "--deny-permission-prompts",
                // Chrome's own background traffic: variation seeds, component updates,
                // pings and reports. Besides calling Google from the agents' browser, on
                // Linux it held a page's first load for more than 10 seconds.
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-default-apps",
                "--disable-client-side-phishing-detection",
                "--disable-domain-reliability",
                "--disable-breakpad",
                "--no-pings",
                "--metrics-recording-only",
                // Keep the profile's saved passwords out of the system keychain.
                "--password-store=basic",
                "--use-mock-keychain",
                "--disable-features=Translate,OptimizationHints,MediaRouter,NetworkTimeServiceQuerying",
                "--window-size=1280,800",
                "about:blank",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: fcntl, dup2 and close are async-signal-safe. The pipes are first copied
        // above 10 so moving one onto 3 or 4 never closes the other.
        unsafe {
            command.pre_exec(move || {
                let reads = libc::fcntl(read_fd, libc::F_DUPFD, 10);
                let writes = libc::fcntl(write_fd, libc::F_DUPFD, 10);
                if reads < 0 || writes < 0 || libc::dup2(reads, 3) < 0 || libc::dup2(writes, 4) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                libc::close(reads);
                libc::close(writes);
                Ok(())
            });
        }
        let child = command.spawn()?;
        drop((child_reads, child_writes));
        Ok(Self {
            transport: Transport::new(we_read, we_write),
            child: Mutex::new(child),
        })
    }

    #[cfg(not(unix))]
    pub fn launch(_: &Path, _: &Path) -> io::Result<Self> {
        Err(io::Error::other("The browser preview needs macOS or Linux"))
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        self.transport.shut();
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;

    /// A stand-in browser: the test reads what the transport wrote and answers.
    fn pair() -> (Transport, Receiver<Value>, io::PipeWriter) {
        let (from_browser, to_transport) = io::pipe().unwrap();
        let (from_transport, to_browser) = io::pipe().unwrap();
        let transport = Transport::new(from_browser, to_browser);
        let (sent, received) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(from_transport);
            let mut buffer = Vec::new();
            while reader.read_until(0, &mut buffer).unwrap_or(0) > 0 {
                buffer.pop();
                let _ = sent.send(serde_json::from_slice(&buffer).unwrap());
                buffer.clear();
            }
        });
        (transport, received, to_transport)
    }

    fn reply(writer: &mut io::PipeWriter, message: Value) {
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(0);
        writer.write_all(&bytes).unwrap();
    }

    #[test]
    fn a_call_gets_the_reply_with_its_id_and_events_reach_their_session() {
        let (transport, sent, mut browser) = pair();
        let transport = Arc::new(transport);
        let (event_tx, events) = mpsc::channel();
        transport.listen(
            "S1",
            Arc::new(move |method: &str, params: &Value| {
                let _ = event_tx.send((method.to_owned(), params.clone()));
            }),
        );
        let calling = transport.clone();
        let call = thread::spawn(move || {
            calling.call(
                Some("S1"),
                "Page.navigate",
                json!({"url": "http://localhost:1"}),
                Duration::from_secs(5),
            )
        });
        let request = sent.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(request["method"], "Page.navigate");
        assert_eq!(request["sessionId"], "S1");
        reply(
            &mut browser,
            json!({"method": "Page.loadEventFired", "sessionId": "S1", "params": {"timestamp": 1}}),
        );
        reply(
            &mut browser,
            json!({"method": "Page.loadEventFired", "sessionId": "S2", "params": {}}),
        );
        reply(
            &mut browser,
            json!({"id": request["id"].as_u64().unwrap() + 100, "result": {}}),
        );
        reply(
            &mut browser,
            json!({"id": request["id"], "result": {"frameId": "F"}}),
        );
        assert_eq!(call.join().unwrap().unwrap(), json!({"frameId": "F"}));
        let (method, params) = events.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            (method.as_str(), params),
            ("Page.loadEventFired", json!({"timestamp": 1}))
        );
        assert!(
            events.try_recv().is_err(),
            "another session's event is not delivered"
        );
    }

    #[test]
    fn a_protocol_error_names_the_method() {
        let (transport, sent, mut browser) = pair();
        let transport = Arc::new(transport);
        let calling = transport.clone();
        let call = thread::spawn(move || {
            calling.call(None, "DOM.focus", json!({}), Duration::from_secs(5))
        });
        let request = sent.recv_timeout(Duration::from_secs(5)).unwrap();
        reply(
            &mut browser,
            json!({"id": request["id"], "error": {"code": -32000, "message": "Element is not focusable"}}),
        );
        assert_eq!(
            call.join().unwrap().unwrap_err(),
            "DOM.focus: Element is not focusable"
        );
    }

    #[test]
    fn a_closed_browser_fails_waiting_and_later_calls() {
        let (transport, sent, browser) = pair();
        let transport = Arc::new(transport);
        let calling = transport.clone();
        let call = thread::spawn(move || {
            calling.call(
                None,
                "Browser.getVersion",
                json!({}),
                Duration::from_secs(5),
            )
        });
        sent.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(browser);
        assert_eq!(call.join().unwrap().unwrap_err(), CLOSED);
        assert_eq!(
            transport
                .call(
                    None,
                    "Browser.getVersion",
                    json!({}),
                    Duration::from_secs(1)
                )
                .unwrap_err(),
            CLOSED
        );
    }

    #[test]
    fn an_unanswered_call_times_out() {
        let (transport, _sent, _browser) = pair();
        let error = transport
            .call(None, "Page.reload", json!({}), Duration::from_millis(50))
            .unwrap_err();
        assert_eq!(error, "The browser did not answer Page.reload in time");
    }
}
