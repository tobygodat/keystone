# Mermaid replies (TOB-34)

Closed, lowercase `mermaid` fences render through the vendored Pele 1.6.0 bundle.
Open fences remain source, including an unfinished final reply. A closed fence can
render while the rest of the reply is still streaming. Other languages retain the
existing code-block behavior. Nested fences remove Markdown container prefixes,
but retain the logical source's whitespace, original CRLF and trailing newline.
Copy always uses that source, never reconstructed SVG or a shortened preview.

The Source/Diagram and Copy buttons are focusable tab stops, with a visible ring
and Enter/Space activation. They use the host's existing Tab/Shift+Tab traversal.
This does not implement the separate transcript-row keyboard journey (TOB-30).
Source previews stop at a UTF-8 boundary after 16 KiB and say when shortened;
Copy still preserves everything. Failures keep the source and show a diagnostic
with line and column. Resource-budget errors identify the limit instead of
pretending to be Mermaid syntax errors.

## Rendering and safety

`diagram/runtime.rs` owns one lazy worker thread and one QuickJS runtime. Source
is passed as a string argument to a fixed host wrapper, never evaluated as code.
The sandbox has no filesystem, network, timers or process host APIs. No Node or
JavaScript package installation is needed at runtime. The bundle and its licenses
are pinned and checked in; see its vendor README for hashes.

Host options disable images and links. Source frontmatter and initialization
directives are rejected so they cannot override those options. The renderer's
SVG passes through an XML element/attribute/style allowlist: no scripts, event
handlers, foreign objects, nested SVG, images, hyperlinks, external URLs, XML
entities/DTDs, CSS imports or escaped CSS resource names. Local marker, clipping
and gradient references are allowed. CSS variables are resolved to theme tokens
or Pele's built-in fallbacks and the selected UI font is hoisted onto the root.
Only those sanitized bytes reach GPUI's SVG `img()` decoder.

The diagram resolves the theme's UI and monospace families against installed
fonts before measuring or rendering. Missing families use explicit concrete
Latin-capable fallbacks; symbol-only faces are rejected. The same concrete names
reach GPUI measurement, Pele's UI/monospace options, sanitized SVG and cache keys.
If no suitable text fonts exist, the source remains available with a diagnostic.
Source-defined font-family overrides are constrained to these host families;
descendant font shorthand is dropped so it cannot restore a missing/symbol face.

GPUI's text system measures labels off the UI thread with the resolved UI/mono
fonts and bold/italic style. Pele layout is cached by source, measured content
width and font-metrics identity. Theme substitution has its own palette-keyed
cache, so a theme-only change does not rerun the JavaScript layout. Width is
measured after layout, excluding the component's padding. Stale requests are
cancelled on source/width/theme/font changes and component destruction. Old
decoded GPUI image assets and their window atlas tiles are explicitly evicted on
replacement and release. Submission returns an async ticket: waiting for the
dedicated JavaScript worker does not occupy a shared GPUI background-pool thread.

The budgets are deliberately smaller than upstream defaults:

| Resource | Limit and failure |
| --- | --- |
| Source | 32,768 UTF-8 bytes; source remains copyable above the limit. |
| Work queue | Eight waiting jobs; full queue fails immediately. |
| Runtime heap / stack | 64 MiB / 512 KiB. |
| JavaScript work | 750 ms per initialization or layout, checked by QuickJS's interrupt handler. |
| Caller/queue deadline | Four seconds; expired jobs are skipped and cancelled work is interrupted. |
| Edges / output | 512 flowchart edges / 1 MiB SVG. |
| Measurement | 4,096 host calls and 256 KiB cumulative label text per layout; invalid metrics fail. |
| Image dimensions | 16,384 pixels per axis and 16,777,216 base pixels; GPUI's 2× SVG smoothing can allocate up to 256 MiB of pixel storage for one maximum-size image. |
| Worker caches | Two LRUs, each at most 32 entries and 8 MiB including source keys. |

Host shaping and native rasterization aren't preempted by the QuickJS interrupt.
They are bounded by the text/SVG/geometry limits and execute off the UI thread.
This is an in-process memory/CPU containment layer, not an OS process sandbox.
JavaScript exceptions, interruption and memory exhaustion reset the QuickJS
engine; ordinary Mermaid syntax errors preserve the healthy engine. Completed
safe results can populate the bounded cache even if their caller was cancelled.
Queue, timeout,
parser and raster failures all preserve a source/copy fallback; toggling back to
Diagram retries cancelled work. No provider or session state is changed.

## Reproduce

Run `bash scripts/check.sh core`, `bash scripts/check.sh ui` and
`bash scripts/check.sh build`. The runtime tests render flowchart, sequence and
pie fixtures at two widths in two themes with resvg 0.46, check interruption,
memory/queue/input bounds, malformed diagnostics, cache invalidation and unsafe
SVG rejection. They write diagnostic PNGs under `target/diagram-runtime-visuals/`.
The Markdown tests compare every streamed prefix with a whole parse, including
CRLF and fence-boundary cases. The `diagrams` UI test covers keyboard source/copy
and independent controls, including explicit retry after failure. A component
regression proves both decoded-asset and atlas eviction on theme/width changes
and destruction, and fails without explicit tile eviction. Symbol-first generated
font fixtures cover missing theme UI/mono names without proprietary font assets.
These are simulated GPUI tests, not native-render evidence.

Launch `cargo run -p keystone-ui --example gallery --locked`, choose **Diagrams**,
and inspect all three diagrams in Dark/Light at 640/320 widths. Use **Open streaming
fence**, then **Close fence** without changing the body, then Source and Copy.
Use **Malformed** to inspect the line/column error; **All diagrams** restores the
fixtures. Tab/Shift+Tab, Enter and Space exercise source/copy without a pointer.
The gallery supports Ctrl+Q for a clean exit. Native acceptance must record the
binary hash and inspect screenshots, not only named-button clicks or SVG snapshots.

The existing `keystone-verify` native lane needs its patched driver, Sway and a
working graphics device; see the repository verification skill. A software Vulkan
X11 gallery run can separately prove native component rendering on cloud machines,
but must not be described as a `keystone-verify` transcript screenshot or as a
Wayland/device acceptance run.
