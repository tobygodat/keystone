# Sounds

Keystone can play a short sound for each event a notification announces: a turn finished, an approval request, or a failed turn. Sounds are off by default. A sound plays under the same rule as its notification: only for a session no active window shows, once until the user sees it again, whether or not notifications are on. A batch of events plays only its most urgent sound (approval, then failed, then finished). A pull request's failed check or new review notifies but plays no sound.

## Sub-features

- `sound-events` plays `turn-finished.wav`, `approval.wav` or `turn-failed.wav` from the data directory's `sounds` folder, written there from the copies bundled in the binary.
- `sound-player` runs paplay, else pw-play, on Linux, and afplay on macOS, with the volume on each player's own scale and in C's number format. paplay plays through PulseAudio and through PipeWire's pulse server alike. When it fails, or has not finished within 3 seconds, pw-play plays the sound. With no player, Settings → Sounds says which to install in place of the switch, hides the other rows, and nothing plays.
- `sound-settings` turns sounds, or each event, on and off and sets the volume in Settings → Sounds. Once a switch turned on, or the volume, rests for 250 ms, its sound plays once as a preview: an event's own, or the first event switched on.
- `sound-persist` keeps the choices in the workspace `preferences.sounds` across restarts.

## How to get to it (user POV)

- Press Ctrl+Comma or click the cog, then choose **Sounds**. Turn on **Play sounds**.
- Let a turn finish in a session while another session is selected, or while another app has focus.

## Driving it with keystone-verify

Never let a run reach the user's speakers. The driver points every run's `PULSE_SERVER` and `PIPEWIRE_REMOTE` at servers that do not exist, whatever the build, and sets `KEYSTONE_TEST_SOUND_COMMAND` to `true`. A debug build runs that command, split like a shell, in place of the player, with paplay's arguments: `--volume=<0 to 65536> <file>`. Pass `env` `KEYSTONE_TEST_SOUND_COMMAND` naming a script that appends its arguments to a log to record each request. Set it empty to stand for a computer with no player. Release builds ignore the variable.

- `click` `settings`, `settings-section-sounds`, then `settings-sounds`. The workspace's `preferences.sounds.enabled` becomes `true` and, a moment later, the log gains one preview request for `turn-finished.wav`. The event switches are `settings-sound-turn-finished`, `settings-sound-approval` and `settings-sound-turn-failed`; the volume slider is `settings-sound-volume`.
- Close Settings with Escape, since nothing counts as seen while it is open. Send a message containing "long" in a scripted Claude session, press Ctrl+N, and wait for the first session's turn to end. Expect exactly one new request, for `turn-finished.wav`, at `--volume=45875` for the default 70%.
- Turn `settings-sounds` off, repeat the turn. Expect no new request.
- Quit with Ctrl+Q, `launch`, and require the switches as they were saved.

## Not covered

- Whether the sound is audible and pleasant. A person checks that once on a real desktop.
- paplay and afplay invocations, and the fallback from a failing or waiting player, are proven by unit tests, not native runs. Real paplay and pw-play runs are not verified.
