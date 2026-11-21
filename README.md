# Tailscale COSMIC Applet

COSMIC panel applet for the local Tailscale daemon. Shows connection state, local Tailscale IPs and peers; supports connecting and disconnecting this device.

## Requirements

- COSMIC desktop
- Tailscale CLI and a running `tailscaled`
- Rust toolchain and `just`

## Build and run

```sh
just build-release
just run
```

Install system-wide with `sudo just install`. The applet invokes only fixed local CLI commands (`tailscale status --json`, `tailscale up`, `tailscale down`, `tailscale ping --c 1` for a single peer) and opens the fixed admin console URL with `xdg-open`. `up` may open the Tailscale authentication flow. `down` disconnects this device. No tailnet API key is used or stored.

Status refreshes every five seconds. Peer status comes from Tailscale's CLI JSON output, whose format can change between versions.
