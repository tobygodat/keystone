//! The browser preview's engine: headless Chrome driven over the DevTools protocol, one
//! page per project folder. See `docs/design/browser-preview.md`.

pub mod cdp;
mod outline;
pub mod page;
pub mod servers;

pub use cdp::Chrome;
pub use page::{Act, Frame, History, Mouse, Page, Target, Viewport, modifiers, normalize_url};
pub use servers::DevServer;

use crate::Wake;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

pub const NO_BROWSER: &str =
    "No Chrome, Chromium, Edge or Brave found. Install one, or set KEYSTONE_BROWSER_PATH.";

/// The browser process and its pages, keyed by project folder. Starting the browser or a
/// page blocks, so [`Browsers::page`] runs off the UI thread; [`Browsers::existing`]
/// never waits on a start.
pub struct Browsers {
    profile: PathBuf,
    wake: Wake,
    /// Held while the browser or a page starts, so two callers never start two.
    starting: Mutex<Option<Arc<Chrome>>>,
    pages: Mutex<HashMap<String, Arc<Page>>>,
    /// Every page by its tab's target ID, for the browser's events, which name a tab.
    tabs: Tabs,
    closed: AtomicBool,
}

type Tabs = Arc<Mutex<HashMap<String, Weak<Page>>>>;

/// How long a pop-up may stay blank before it is closed without an address to load.
const BLANK_POPUP: Duration = Duration::from_secs(1);

impl Browsers {
    /// `profile` holds the browser's own profile. `wake` runs on each new frame or
    /// address of any page.
    pub fn new(profile: PathBuf, wake: Wake) -> Self {
        Self {
            profile,
            wake,
            starting: Mutex::new(None),
            pages: Mutex::new(HashMap::new()),
            tabs: Arc::default(),
            closed: AtomicBool::new(false),
        }
    }

    pub fn existing(&self, folder: &str) -> Option<Arc<Page>> {
        self.pages
            .lock()
            .unwrap()
            .get(folder)
            .filter(|page| !page.is_closed())
            .cloned()
    }

    /// `folder`'s page, starting the browser and the page if needed.
    pub fn page(&self, folder: &str) -> Result<Arc<Page>, String> {
        if let Some(page) = self.existing(folder) {
            return Ok(page);
        }
        let mut chrome = self.starting.lock().unwrap();
        if let Some(page) = self.existing(folder) {
            return Ok(page);
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(cdp::CLOSED.into());
        }
        if chrome.as_ref().is_none_or(|chrome| chrome.is_closed()) {
            self.pages.lock().unwrap().clear();
            let binary = Chrome::find().ok_or(NO_BROWSER)?;
            let started = Chrome::launch(&binary, &self.profile)
                .map_err(|error| format!("{} did not start: {error}", binary.display()))?;
            let started = Arc::new(started);
            contain(&started, self.tabs.clone())?;
            *chrome = Some(started);
        }
        let page = Arc::new(Page::open(chrome.clone().unwrap(), self.wake.clone())?);
        {
            let mut tabs = self.tabs.lock().unwrap();
            tabs.retain(|_, page| page.strong_count() > 0);
            tabs.insert(page.target().to_owned(), Arc::downgrade(&page));
        }
        if self.closed.load(Ordering::Acquire) {
            chrome.take();
            return Err(cdp::CLOSED.into());
        }
        self.pages
            .lock()
            .unwrap()
            .insert(folder.to_owned(), page.clone());
        (self.wake)();
        Ok(page)
    }

    /// Closes every page and the browser, and refuses new pages. Never waits on a start:
    /// a page still starting closes the browser itself when it finds this.
    pub fn shut_down(&self) {
        self.closed.store(true, Ordering::Release);
        self.pages.lock().unwrap().clear();
        if let Ok(mut chrome) = self.starting.try_lock() {
            chrome.take();
        }
    }
}

/// Keeps what pages do in their tab, where the user and agents can see it: downloads are
/// refused, since a page or an agent could otherwise save files into the user's
/// Downloads folder unseen, and a pop-up is closed, its address loaded in the tab that
/// opened it.
fn contain(chrome: &Arc<Chrome>, tabs: Tabs) -> Result<(), String> {
    const CALL: Duration = Duration::from_secs(10);
    let browser = Arc::downgrade(chrome);
    let blank = Arc::new(Mutex::new(HashSet::<String>::new()));
    chrome.listen(
        "",
        Arc::new(move |method: &str, params: &Value| {
            let opener_of = |id: &str| tabs.lock().unwrap().get(id).and_then(Weak::upgrade);
            match method {
                "Target.targetCreated" | "Target.targetInfoChanged" => {
                    let info = &params["targetInfo"];
                    let (Some(id), Some(opener)) = (
                        info["targetId"].as_str(),
                        info["openerId"].as_str().and_then(opener_of),
                    ) else {
                        return;
                    };
                    if info["type"] != "page" || id == opener.target() {
                        return;
                    }
                    let url = info["url"].as_str().unwrap_or("");
                    let close = |browser: &Weak<Chrome>, id: &str| {
                        if let Some(chrome) = browser.upgrade() {
                            chrome.send(None, "Target.closeTarget", json!({"targetId": id}));
                        }
                    };
                    if url.is_empty() || url == "about:blank" {
                        if !blank.lock().unwrap().insert(id.to_owned()) {
                            return;
                        }
                        let (blank, browser, id) = (blank.clone(), browser.clone(), id.to_owned());
                        let opener = Arc::downgrade(&opener);
                        let _ = std::thread::Builder::new()
                            .name("keystone-browser-popup".into())
                            .spawn(move || {
                                std::thread::sleep(BLANK_POPUP);
                                if blank.lock().unwrap().remove(&id) {
                                    close(&browser, &id);
                                    if let Some(opener) = opener.upgrade() {
                                        opener.popped_up(None);
                                    }
                                }
                            });
                        return;
                    }
                    let was_blank = blank.lock().unwrap().remove(id);
                    if method == "Target.targetCreated" || was_blank {
                        close(&browser, id);
                        opener.popped_up(Some(url));
                    }
                }
                "Browser.downloadWillBegin" => {
                    let name = params["suggestedFilename"].as_str().unwrap_or("a file");
                    if let Some(page) = params["frameId"].as_str().and_then(opener_of) {
                        page.report(format!("Refused a download of {name}"));
                    }
                }
                _ => {}
            }
        }),
    );
    chrome.call(
        None,
        "Browser.setDownloadBehavior",
        json!({"behavior": "deny", "eventsEnabled": true}),
        CALL,
    )?;
    chrome.call(
        None,
        "Target.setDiscoverTargets",
        json!({"discover": true}),
        CALL,
    )?;
    Ok(())
}
