# A local workspace for Claude Code and Codex

This sample shows the conversation layout and how **Markdown replies** render: *emphasis*, `inline code`, ~~struck text~~, and [links](https://github.com/tobygodat/keystone) that open in your browser.

## The testing loop

1. Run the core checks after persistence changes.
2. Run the GPUI checks after UI behavior changes:
   - simulated clicks and typing
   - the component tests in `keystone-ui`
3. Check the real window before calling it done.

| Check | Command | When |
| :-- | :-- | --: |
| Core behavior | `bash scripts/check.sh core` | After persistence changes |
| GPUI interactions | `bash scripts/check.sh ui` | After UI behavior changes |
| Native build | `bash scripts/check.sh build` | Before desktop verification |
| Native capture | `python scripts/native-smoke.py` | Check the real window |

> Keep live provider tests separate from deterministic UI checks.

Run the preview with the command below.

```bash
cargo run -p keystone-desktop --locked
KEYSTONE_VISUAL_FIXTURE=1 cargo run -p keystone-desktop --locked  # this sample
```

---

This is a visual fixture, not an agent response. No model request is made.
