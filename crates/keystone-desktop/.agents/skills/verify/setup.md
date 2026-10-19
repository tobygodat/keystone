# Build the native verification driver

The driver is [computer-use-mcp](https://github.com/ALLLLLL703/computer-use-mcp) at commit `7acd4518c0028f034e33dfd20bb5b159adf15be5`, version 0.3.2, with both `scripts/patches/computer-use-mcp-headless.patch` and `scripts/patches/computer-use-mcp-wheel.patch` applied. Its local executable is `.audit/tools/computer-use-mcp` relative to the Keystone workspace root. It is a tool dependency, not part of the application. An older headless-only executable must be rebuilt for wheel verification; the wrapper cannot repair its wire events.

The patch adds a `headless` option to `session_start`. It starts the private Sway on wlroots' headless backend with the GLES2 renderer instead of as a window on the host compositor. Capture and input use the same screencopy and virtual input protocols. Without it, `start` fails because the driver rejects the unknown field.

This machine has Sway 1.12, Bubblewrap, D-Bus, AT-SPI, libxkbcommon, and Xwayland installed. Sway installation also added wlroots0.20 and libliftoff. No Hyprland configuration was changed.

If the executable is missing, use a new temporary checkout and the pinned commit. Run from the Keystone workspace root:

```bash
checkout=$(mktemp -d /tmp/keystone-driver-build.XXXXXX)
git clone https://github.com/ALLLLLL703/computer-use-mcp.git "$checkout"
git -C "$checkout" checkout --detach 7acd4518c0028f034e33dfd20bb5b159adf15be5
git -C "$checkout" apply "$PWD/scripts/patches/computer-use-mcp-headless.patch"
git -C "$checkout" apply "$PWD/scripts/patches/computer-use-mcp-wheel.patch"
cargo test --manifest-path "$checkout/Cargo.toml" --locked -j 2 wheel_ticks_preserve -- --nocapture
cargo build --manifest-path "$checkout/Cargo.toml" --locked -j 2
mkdir -p .audit/tools
install -m 755 "$checkout/target/debug/computer-use-mcp" .audit/tools/computer-use-mcp
```

Build it once when no other owner is using that executable. For concurrent work, install in your own checkout's `.audit/tools` and pass `--driver` explicitly instead of replacing the shared copy. `native-driver.py` and `native-stress.py` otherwise fall back to the main checkout's copy. Keep the source checkout outside the application workspace until the build and test succeed. Its removal is optional and must target that exact directory. The debug build is intentional. No performance claim depends on its build profile.

The wheel patch preserves upstream `scroll_x`/`scroll_y` signed axis-distance units. Fifteen units equal one discrete notch. It emits `axis_discrete` instead of raw `axis` for whole notches, retaining fractional distances separately per axis between requests. Sub-notch input accumulates until a notch is reached. On a modern Sway client this becomes `wl_pointer.axis_value120`; GPUI ignores raw Wheel-source axis distances without this data. Use the wrapper's `wheel` tool for signed integer notch counts, or ordinary `input` with `scroll_y:-45` for three upward notches. No separate injector is needed.

Headless runs need only GPU access, through a DRM render node. They need no host `WAYLAND_DISPLAY`, so they work from SSH and remote agent shells. `headless:false` still needs a graphical Wayland session with valid `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`. There the host may choose a different size than requested. Read actual screenshot dimensions before clicking.

The driver uses host access for the app so GPUI can access the GPU. A separate input desktop is not a filesystem or network sandbox. The server sets an isolated Keystone state file and element geometry file, and removes `KEYSTONE_VISUAL_FIXTURE`. Provider execution is not implemented in this preview.

The `keystone-verify` server in `.mcp.json` is the only registration needed. It needs no browser extension, exposed HTTP port, or access to host input devices.
