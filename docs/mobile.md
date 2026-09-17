# Mobile

A phone is a **UI client and nothing else**. It points a browser at the server
URL and installs that as a PWA. It never runs a daemon.

```
phone (installed PWA) ── HTTPS + WS ──▶ loom-server ── relay ──▶ frames
                                            ▲
                                         daemon (a real machine)
```

This is not a limitation to work around. From
[`architecture.md`](architecture.md): a UI never talks to a daemon, and a daemon
never talks to a UI. The phone is the "inspect and steer" role; the execution
machines are the "run provider CLIs and tools" role. A phone cannot usefully be
the second: there is no systemd there, provider CLIs are not installed, and the
work product (a workspace on disk) does not live on the phone. Adding a daemon
would give you a machine that is offline half the day holding a host identity.
Don't.

## Installing the PWA

The bb web UI already carries what an installable PWA needs — a web app
manifest, maskable icons, `display: standalone` and safe-area insets — so
nothing mobile-specific needs to be built. Serve it from the server:

```
LOOM_UI_DIR=/usr/local/share/loom/ui     # the built bb bundle
```

and open the server URL on the phone:

- **iOS / iPadOS (Safari):** Share → *Add to Home Screen*. It launches
  full-screen, no browser chrome, with the notch/home-indicator insets handled.
- **Android (Chrome):** ⋮ → *Install app* (or a "Add to Home screen" banner).
- **Desktop (Chrome/Edge):** the install icon in the address bar.

Two prerequisites for installability, both satisfied by the Tailscale path:

- **HTTPS** — service workers and install prompts need a secure origin. The
  tailnet `https://<machine>.<tailnet>.ts.net` URL counts; `http://` on a LAN IP
  does not (loopback is exempt, which does not help a phone). See
  [`remote-access.md`](remote-access.md).
- **Same origin as the API** — the client derives its server from
  `window.location.origin`, so the installed app talks to the server with no
  per-device configuration.

> The buildless reference client (`ui/` in this repository, the zero-config
> default) is intentionally plain: no manifest, no service worker, not
> installable. It exists to prove the path, not to be the phone app. Point
> `LOOM_UI_DIR` at the built bb bundle for a real install.

## What works, and what does not

- Works: browse projects and threads, read timelines, send messages. The product
  app subscribes to typed targets on public `/ws`; reconnect invalidates caches
  and reloads them over HTTP. The embedded reference client uses
  `/internal/ws` plus its persisted raw-relay replay cursor.
- Works: several phones, a desktop and a webview watching the same threads at
  once. Frames fan out from the relay; none of them is authoritative.
- Does not work: modifying the phone's own filesystem or running an agent
  locally. That is what a daemon on a real machine is for.
- Offline: the installed shell may open without a network, but there is no
  offline mode for the data — a UI holds no state, so with no server there is
  nothing to render. Frames resume on reconnect.

## Keeping a phone from appearing as an execution machine

Nothing to do: enrollment happens only when a daemon process calls
`enroll_host` on the socket. A browser never sends it. A phone appears in the UI
only as a client, never in `GET /api/v1/hosts`.
