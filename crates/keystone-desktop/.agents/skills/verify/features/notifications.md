# Notifications

Keystone sends a desktop notification when a session's turn finishes, it asks for approval, or its turn fails, but only for a session the user cannot see: no active Keystone window shows it. Each session sends each kind once until the user sees it again. Activating a notification shows its session.

## Sub-features

- `notify-events` sends one notification per event for a session no active window shows, titled with the session's title. The body reads "Turn finished", "Approval requested" or "Turn failed". A turn the user stopped, or ended with a denial, sends none.
- `notify-coalesce` sends a session's event once until an active window shows that session. Two approval requests, or two finished turns, in one unseen stretch send one notification.
- `notify-open` selects the session from its notification, in the window showing it, else the window used last, else the first, leaves the settings page there and asks the compositor to focus the window.
- `notify-settings` turns all notifications, or each event, off in Settings → Notifications. Every switch is on by default, and the event switches are disabled while the global one is off.
- Attention, Needs action and settling are unchanged: notifications only read events.

## How to get to it (user POV)

- Let a turn finish, or ask for approval, in a session while another session is selected or while another app has focus.
- Click the notification in the desktop's notification area.
- Press Ctrl+Comma or click the cog, then choose **Notifications**.

## Driving it with keystone-verify

The driver starts each run's app on its own private session bus, a `dbus-daemon` with no activatable services, and returns its address as `bus` from `start`. Nothing the app sends reaches the user's own desktop. With no notification server on that bus, notifications fail quietly.

To observe them, own `org.freedesktop.Notifications` on `bus` with a fake server that records each `Notify` call and can emit `ActionInvoked`. Never point it at the user's session bus. `uv run --no-project --with jeepney python scripts/fake-notifications.py <bus> <log> <commands>` does this: it appends each `Notify` to the log as JSON and emits `ActionInvoked` for each `invoke <id> default` line appended to the commands file. Check `/proc/<pid>/environ` of the launched app for `DBUS_SESSION_BUS_ADDRESS` equal to `bus`.

- Send a message containing "long" in a scripted Claude session, press Ctrl+N, and wait for the first session's turn to end. Expect one `Notify` with the session's title as its summary and "Turn finished" as its body.
- Emit `ActionInvoked(<id>, "default")`. The workspace's `selected` becomes that session.
- `click` `settings`, `settings-section-notifications`, then `settings-notify-turn-finished`. The workspace's `preferences.notifications.turn_finished` becomes `false`. The global switch is `settings-notifications`; the others are `settings-notify-approval` and `settings-notify-turn-failed`.
- For an approval request out of sight, start the scripted Claude through a wrapper that sleeps a few seconds before `exec`, send a message containing "run", and select another session before the request arrives. Expect one `Notify` with "Approval requested".

## Not covered

- Whether the compositor honours the focus request. On Wayland Keystone asks through xdg-activation with its own last input serial, because the notification service's activation token is not passed on. A compositor may refuse and mark the window urgent instead. The selection changes either way.
- Sounds, covered in [sounds](sounds.md), and pull request checks and reviews, which notify as [pull request](pull-request.md) describes.
