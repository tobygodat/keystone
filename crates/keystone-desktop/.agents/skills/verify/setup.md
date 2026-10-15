# Build the native verification driver

The driver is [computer-use-mcp](https://github.com/ALLLLLL703/computer-use-mcp) at commit `7acd4518c0028f034e33dfd20bb5b159adf15be5`, version 0.3.2. Its local executable is `.audit/tools/computer-use-mcp` relative to the Keystone workspace root. It is a tool dependency, not part of the application.

This machine has Sway 1.12, Bubblewrap, D-Bus, AT-SPI, libxkbcommon, and Xwayland installed. Sway installation also added wlroots0.20 and libliftoff. No Hyprland configuration was changed.

If the executable is missing, use a new temporary checkout and the pinned commit. Run from the Keystone workspace root:

```bash
checkout=$(mktemp -d /tmp/keystone-driver-build.XXXXXX)
git clone https://github.com/ALLLLLL703/computer-use-mcp.git "$checkout"
git -C "$checkout" checkout --detach 7acd4518c0028f034e33dfd20bb5b159adf15be5
cargo build --manifest-path "$checkout/Cargo.toml" --locked -j 2
mkdir -p .audit/tools
install -m 755 "$checkout/target/debug/computer-use-mcp" .audit/tools/computer-use-mcp
```

Keep the checkout until the build succeeds. Its removal is optional and must target that exact directory. The debug build is intentional. No performance claim depends on its build profile.

Run from a graphical Wayland session with valid `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`. The driver starts a nested Sway compositor and uses that compositor's capture and input protocols. The nested desktop can appear as a window in Hyprland. The host may choose a different size than requested. Read actual screenshot dimensions before clicking.

The driver uses host access for the app so GPUI can access the GPU. A separate input desktop is not a filesystem or network sandbox. The controller sets an isolated Keystone state file and removes `KEYSTONE_VISUAL_FIXTURE`. Provider execution is not implemented in this preview.

Use the stdio controller in SKILL.md. It requires no global MCP registration, browser extension, exposed HTTP port, or access to host input devices. A restricted agent shell may need permission to access the existing Wayland socket and GPU.
