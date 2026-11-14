//! What a page may do without asking: which navigations an agent's actions may cause,
//! and the permission prompts Keystone refuses for it.

use crate::mcp::browser::is_local;

/// Who last drove the page. A navigation the page makes is the doing of whoever acted
/// on it last, until the other acts.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Driver {
    /// The user, in the Browser surface. Where their clicks lead is their own choice.
    #[default]
    User,
    /// An agent, through the browser tools. `asked` is the last address it loaded by
    /// name, which the provider's approval already covered.
    Agent { asked: Option<String> },
}

/// One main-frame document request the page is about to make.
pub struct Request<'a> {
    pub url: &'a str,
    /// The address the page shows now.
    pub current: &'a str,
    /// Whether it follows a redirect from an address off this machine that was let
    /// through, so a site the user approved can redirect within itself and beyond.
    pub after_approved: bool,
}

/// Whether `request` goes ahead. An agent may lead the page anywhere on this machine,
/// to the address it asked for, or to the page it is on, as `browser_navigate` may
/// without asking. Anywhere else it must ask for by name with `browser_navigate`,
/// which goes through the provider's approval.
pub fn admits(driver: &Driver, request: &Request) -> bool {
    let Driver::Agent { asked } = driver else {
        return true;
    };
    !(request.url.starts_with("http://") || request.url.starts_with("https://"))
        || is_local(request.url)
        || request.after_approved
        || same_page(request.url, request.current)
        || asked.as_deref().is_some_and(|asked| {
            crate::browser::normalize_url(asked).is_ok_and(|asked| same_page(request.url, &asked))
        })
}

/// Two addresses of one page: equal apart from a fragment, a trailing slash, and the
/// case of the scheme and host.
fn same_page(a: &str, b: &str) -> bool {
    fn key(url: &str) -> String {
        let url = url.split('#').next().unwrap_or(url);
        let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
        let (host, path) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
        let path = path.strip_suffix('/').unwrap_or(path);
        format!(
            "{}://{}{path}",
            scheme.to_ascii_lowercase(),
            host.to_ascii_lowercase()
        )
    }
    key(a) == key(b)
}

/// The note and the agent's reply when a navigation an agent caused was stopped.
pub fn refused(url: &str) -> String {
    format!("Kept the page here: an agent's action led to {url}, which isn't on this machine")
}

/// What the agent is told about a stopped navigation.
pub fn refused_hint(url: &str) -> String {
    format!(
        "An action led the page to {url}, which isn't on this machine, so the page stayed where it was. To go there, call browser_navigate with that url, which goes through the user's approval."
    )
}

/// Marks a console message from [`PERMISSIONS_SCRIPT`]. The page can write it too, but
/// only a name from [`PERMISSIONS`] is read from it.
pub const PERMISSION_MARK: &str = "\u{2063}keystone-permission";

/// Each permission the script watches, and how the note names it.
pub const PERMISSIONS: [(&str, &str); 10] = [
    ("geolocation", "its location"),
    ("notifications", "notifications"),
    ("media", "the camera or microphone"),
    ("display", "screen capture"),
    ("midi", "MIDI devices"),
    ("clipboard", "the clipboard"),
    ("fonts", "local fonts"),
    ("screens", "window placement"),
    ("storage", "storage access"),
    ("idle", "idle detection"),
];

/// The note for a refused permission named `name`, if it is one the script reports.
pub fn permission_note(name: &str) -> Option<String> {
    PERMISSIONS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, label)| format!("The page asked for {label}, and Keystone refused"))
}

/// Run in every frame before the page's own script. Chrome refuses every permission
/// prompt (`--deny-permission-prompts`) without telling anyone, so this reports each
/// refusal of a prompting API to the surface through the console.
pub const PERMISSIONS_SCRIPT: &str = r#"(() => {
  const mark = "⁣keystone-permission";
  const report = (name) => console.debug(mark, name);
  const watch = (owner, key, name, refusedValue) => {
    const original = owner && owner[key];
    if (typeof original !== "function") return;
    owner[key] = function (...args) {
      const result = original.apply(this, args);
      Promise.resolve(result).then(
        (value) => { if (refusedValue !== undefined && value === refusedValue) report(name); },
        (error) => { if (error && (error.name === "NotAllowedError" || error.name === "SecurityError")) report(name); });
      return result;
    };
  };
  const geolocation = globalThis.Geolocation && Geolocation.prototype;
  for (const key of ["getCurrentPosition", "watchPosition"]) {
    const original = geolocation && geolocation[key];
    if (typeof original !== "function") continue;
    geolocation[key] = function (success, failure, options) {
      return original.call(this, success, function (error) {
        if (error && error.code === 1) report("geolocation");
        if (typeof failure === "function") return failure.apply(this, arguments);
      }, options);
    };
  }
  watch(globalThis.Notification, "requestPermission", "notifications", "denied");
  const media = globalThis.MediaDevices && MediaDevices.prototype;
  watch(media, "getUserMedia", "media");
  watch(media, "getDisplayMedia", "display");
  watch(globalThis.Navigator && Navigator.prototype, "requestMIDIAccess", "midi");
  const clipboard = globalThis.Clipboard && Clipboard.prototype;
  watch(clipboard, "read", "clipboard");
  watch(clipboard, "readText", "clipboard");
  watch(globalThis, "queryLocalFonts", "fonts");
  watch(globalThis, "getScreenDetails", "screens");
  watch(globalThis.Document && Document.prototype, "requestStorageAccess", "storage");
  watch(globalThis.IdleDetector, "requestPermission", "idle", "denied");
})();"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(asked: Option<&str>) -> Driver {
        Driver::Agent {
            asked: asked.map(str::to_owned),
        }
    }

    fn request<'a>(url: &'a str, current: &'a str) -> Request<'a> {
        Request {
            url,
            current,
            after_approved: false,
        }
    }

    #[test]
    fn an_agent_leaves_this_machine_only_where_it_asked_to_go() {
        let local = "http://localhost:3000/";
        assert!(admits(
            &Driver::User,
            &request("https://example.com/", local)
        ));
        assert!(admits(
            &agent(None),
            &request("http://localhost:4000/a", local)
        ));
        assert!(admits(&agent(None), &request("http://127.0.0.1:9/", local)));
        assert!(!admits(
            &agent(None),
            &request("https://example.com/", local)
        ));
        assert!(!admits(
            &agent(Some("localhost:3000")),
            &request("https://accounts.example.com/login", local)
        ));
        assert!(admits(
            &agent(Some("example.com")),
            &request("https://EXAMPLE.com/#top", local)
        ));
        assert!(
            admits(
                &agent(None),
                &request("https://example.com/a/", "https://example.com/a")
            ),
            "a reload of the page it is on"
        );
        assert!(admits(
            &agent(None),
            &Request {
                url: "https://example.org/",
                current: local,
                after_approved: true,
            }
        ));
        assert!(
            !admits(
                &agent(None),
                &request("https://evil.com\\@localhost/", local)
            ),
            "anything unusual counts as off this machine"
        );
    }

    #[test]
    fn only_known_permissions_make_a_note() {
        assert_eq!(
            permission_note("geolocation").as_deref(),
            Some("The page asked for its location, and Keystone refused")
        );
        assert_eq!(permission_note("Saved your files to evil.com"), None);
    }
}
