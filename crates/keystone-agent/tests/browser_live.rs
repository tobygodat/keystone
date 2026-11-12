//! Drives a real headless Chrome against a page this test serves. Needs Chrome,
//! Chromium, Edge or Brave, so it is opt-in:
//! `cargo test -p keystone-agent --test browser_live -- --ignored`.

use keystone_agent::browser::{Act, Browsers, History, Target};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const PAGE: &str = r#"<!doctype html><title>Fixture</title>
<h1>Orders</h1>
<label>Customer <input id="name"></label>
<select id="size" aria-label="Size"><option value="s">Small</option><option value="l">Large</option></select>
<form onsubmit="event.preventDefault(); document.getElementById('out').textContent = 'Saved ' + document.getElementById('name').value + ' ' + document.getElementById('size').value"><input id="note" aria-label="Note"></form>
<button onclick="document.getElementById('out').textContent = 'Clicked'">Save</button>
<button onclick="document.getElementById('out').textContent = confirm('Sure?') ? 'Confirmed' : 'Declined'">Ask</button>
<p id="out">Idle</p>"#;

fn serve() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        // A connection each: Chrome on Linux opens a spare connection it may never
        // send on, which would hold up a server that answers one at a time.
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut stream = stream;
                let mut request = [0; 2048];
                let _ = stream.read(&mut request);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PAGE}",
                    PAGE.len()
                );
            });
        }
    });
    port
}

fn reference(outline: &str, line_start: &str) -> String {
    let line = outline
        .lines()
        .find(|line| line.trim_start().starts_with(line_start))
        .unwrap_or_else(|| panic!("no {line_start} in\n{outline}"));
    let start = line.find("[ref=").unwrap() + 5;
    line[start..line[start..].find(']').unwrap() + start].to_owned()
}

#[test]
#[ignore = "needs an installed Chromium-family browser"]
fn an_agent_drives_a_local_page() {
    let profile = tempfile::tempdir().unwrap();
    let frames = Arc::new(AtomicUsize::new(0));
    let counted = frames.clone();
    let browsers = Browsers::new(
        profile.path().join("browser"),
        Arc::new(move || {
            counted.fetch_add(1, Ordering::Relaxed);
        }),
    );
    let port = serve();
    let page = browsers.page("/project").unwrap();
    page.navigate(&format!("localhost:{port}")).unwrap();
    assert_eq!(page.url(), format!("http://localhost:{port}/"));

    let snapshot = page.snapshot().unwrap();
    let outline = snapshot["outline"].as_str().unwrap();
    assert_eq!(snapshot["title"], "Fixture");
    assert!(
        outline.contains("- heading \"Orders\" [level=1]"),
        "{outline}"
    );

    let save = reference(outline, "- button \"Save\"");
    let again = page.snapshot().unwrap();
    assert_eq!(
        reference(again["outline"].as_str().unwrap(), "- button \"Save\""),
        save,
        "an element keeps its ref across snapshots"
    );
    page.act(&Act::Click {
        target: Target::Ref(save),
        count: 1,
    })
    .unwrap();
    assert_eq!(page.evaluate("out.textContent").unwrap(), json!("Clicked"));

    let name = reference(outline, "- textbox \"Customer\"");
    page.act(&Act::Type {
        reference: Some(name.clone()),
        text: "Ada".into(),
        clear: false,
    })
    .unwrap();
    page.act(&Act::Type {
        reference: Some(name),
        text: "Grace".into(),
        clear: true,
    })
    .unwrap();
    let size = reference(outline, "- combobox \"Size\"");
    assert_eq!(
        page.act(&Act::Select {
            reference: size,
            value: "Large".into()
        })
        .unwrap(),
        json!({"selected": "l"})
    );
    let note = reference(outline, "- textbox \"Note\"");
    page.act(&Act::Press {
        reference: Some(note),
        key: "Enter".into(),
    })
    .unwrap();
    assert_eq!(
        page.evaluate("out.textContent").unwrap(),
        json!("Saved Grace l")
    );

    let ask = reference(outline, "- button \"Ask\"");
    page.act(&Act::Click {
        target: Target::Ref(ask),
        count: 1,
    })
    .unwrap();
    assert_eq!(
        page.evaluate("out.textContent").unwrap(),
        json!("Declined"),
        "a dialog is dismissed, not left blocking the page"
    );
    assert_eq!(
        page.note().as_deref(),
        Some("Dismissed a confirm dialog: “Sure?”")
    );

    let stale = page
        .act(&Act::Click {
            target: Target::Ref("e999".into()),
            count: 1,
        })
        .unwrap_err();
    assert!(stale.contains("take a new browser_snapshot"), "{stale}");
    page.go(History::Reload).unwrap();
    let after_reload = page
        .act(&Act::Hover {
            target: Target::Ref("e1".into()),
        })
        .unwrap_err();
    assert!(
        after_reload.contains("stale"),
        "a navigation drops refs: {after_reload}"
    );

    let shot = page.screenshot().unwrap();
    assert!(shot.starts_with(&[0xff, 0xd8, 0xff]), "a JPEG");
    let thrown = page
        .evaluate("(() => { throw new Error('boom') })()")
        .unwrap_err();
    assert!(thrown.contains("boom"), "{thrown}");
    assert_eq!(page.evaluate("Promise.resolve(41 + 1)").unwrap(), json!(42));
    assert!(page.navigate("file:///etc/hosts").is_err());

    let deadline = Instant::now() + Duration::from_secs(5);
    while page.frame().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    let frame = page.frame().expect("the screencast sends frames");
    assert!(frame.jpeg.starts_with(&[0xFF, 0xD8]), "a JPEG frame");
    assert!(frames.load(Ordering::Relaxed) > 0);

    let status: Value = page.status().unwrap();
    assert_eq!(status["viewport"], json!({"width": 1280, "height": 800}));
    browsers.shut_down();
}

#[test]
#[ignore = "runs lsof against this test's own listener"]
fn this_tests_own_server_is_a_dev_server_of_its_folder() {
    let port = serve();
    let folder = std::env::current_dir().unwrap();
    let found = keystone_agent::browser::servers::find(&folder);
    assert!(found.iter().any(|server| server.port == port), "{found:?}");
}

const CONTAINED: &str = r#"<!doctype html><title>Contained</title>
<a href="file:///etc/hostname">Local file</a>
<button onclick="window.open('/popup')">Pop up</button>
<a href="/download" download="fixture.bin">Download</a>"#;

/// Serves [`CONTAINED`], a pop-up page that calls home, and a download, and records
/// each request's path.
fn serve_contained(seen: Arc<std::sync::Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let seen = seen.clone();
            thread::spawn(move || {
                let mut stream = stream;
                let mut request = [0; 2048];
                let read = stream.read(&mut request).unwrap_or(0);
                let path = String::from_utf8_lossy(&request[..read])
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_owned();
                seen.lock().unwrap().push(path.clone());
                let (kind, body) = match path.as_str() {
                    "/download" => ("application/octet-stream", "data"),
                    "/popup" => ("text/html", "<title>Popup</title><p>Popped"),
                    _ => ("text/html", CONTAINED),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    port
}

#[test]
#[ignore = "needs an installed Chromium-family browser"]
fn a_page_keeps_to_its_tab() {
    let home = tempfile::tempdir().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let browsers = Browsers::new(home.path().join("browser"), Arc::new(|| {}));
    let port = serve_contained(seen.clone());
    let page = browsers.page("/project").unwrap();
    let home_page = format!("http://localhost:{port}/");
    page.navigate(&home_page).unwrap();
    let outline = page.snapshot().unwrap()["outline"]
        .as_str()
        .unwrap()
        .to_owned();
    let click = |line: &str| {
        page.act(&Act::Click {
            target: Target::Ref(reference(&outline, line)),
            count: 1,
        })
        .unwrap();
    };
    let settle = |what: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !what() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
    };

    page.evaluate("location.href = 'file:///etc/hostname'; 0")
        .unwrap();
    click("- link \"Local file\"");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(page.url(), home_page, "a page can't open local files");

    click("- link \"Download\"");
    settle(&|| page.note().is_some());
    assert_eq!(
        page.note().as_deref(),
        Some("Refused a download of fixture.bin")
    );
    let saved: Vec<_> = walk(home.path())
        .into_iter()
        .filter(|path| path.ends_with("fixture.bin"))
        .collect();
    assert!(saved.is_empty(), "nothing was saved: {saved:?}");

    click("- button \"Pop up\"");
    let popup = format!("http://localhost:{port}/popup");
    settle(&|| page.url() == popup);
    assert_eq!(page.url(), popup, "the pop-up's page loads in this tab");
    assert_eq!(
        page.note().as_deref(),
        Some(format!("Opened a pop-up's page here: {popup}").as_str())
    );
    browsers.shut_down();
}

fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                folders.push(path.clone());
            }
            found.push(path);
        }
    }
    found
}
