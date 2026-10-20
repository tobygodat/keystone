# Save and Quit

UC-11 keeps close-as-Quit. Ctrl+Q, the title-bar Close button, and compositor close all use the application save transaction. Closing the last window does not run conversations in the background; that is deferred to TOB-24.

## Native acceptance

Use isolated scripted providers and record the exact application SHA and doctor binary hash. Exercise Ctrl+Q with the composer, root, Settings, palette, and a transcript Copy action focused. Exercise title-bar Close and compositor close separately. Type an exact multiline draft immediately before closing, relaunch, and assert both the saved bytes and restored usable editor. Every successful Quit must exit 0, not merely remove its window.

During a scripted turn, record the provider and descendant PIDs, then Quit. They must exit before driver cleanup. Relaunch must show an interrupted turn, with completed log items preserved; partial streamed deltas are not promised recoverable. Repeat while both providers are starting, including version/catalog probes. Never wait for a turn to finish or automatically approve it as part of Quit.

For failure/retry, make the private state directory unwritable or put a file at the private `sessions` directory after history has loaded. Keep a scripted provider active, type a new draft, then Quit. Require the original native window, live app/provider PIDs, exact composer text, and the visible `shutdown-error` element. Open Settings and confirm the same error remains visible there. Repair storage and allow ordinary draft autosave to succeed: the shutdown error must remain sticky. Explicitly retry Quit without another Send, require exit 0 and all owned processes gone, then relaunch and check exact saved draft/log contents without duplicates.

GPUI regressions additionally inject directory-sync failure, exercise a nonselected session's failed log/metadata save, count dirty/clean/reentrant saves, cover delayed or stalled history, and verify two independent presentations receive runtime notifications. No-view runtime tests establish app ownership without changing the product's close policy.

For terminal budget loss, block the private log directory and generate enough scripted completed items to exceed the shared 16 MiB retry budget. Require the loss warning before Quit, even for a nonselected session and in Settings. Retained writes and failed loss-marker writes still veto Quit. After storage repair, an explicit retry must exit 0 without pretending the rejected entries were recovered. Relaunch and inspect the retained prefix followed by the existing-format “Keystone log loss” item reporting how many entries were lost. Repeated barriers must not duplicate a marker; additional rejections produce an additional count marker. Ordinary log writes for that session remain disabled until restart.

## Limits

A blocked storage syscall can still block shutdown. Forced termination and power loss are best effort; log recovery remains completed-items-only. Cloud software-native receipts do not establish hardware-GPU or real-provider inference behavior.

Rejected budget entries are already discarded, not retryable writes. Quit does not veto that terminal loss alone; only remaining save work can veto. The synced marker makes the gap visible after restart but cannot recover missing content. Terminal log-budget rejection recovery/backpressure is a separate bounded follow-up. The two-second history-load fallback remains: after a vetoed Quit, a history that missed the deadline stays hidden until restart, while new run-tagged entries continue to be saved.
