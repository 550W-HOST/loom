# Remote access

The server has **no authentication layer**. There is no token, no session, no
account, no per-scope authorization. That is a deliberate scope decision — the
deployment shape assumes the control plane is reachable only from a trusted
network — and it makes the network boundary the entire security model.

What reaching the server grants an attacker:

- `POST /api/v1/publish` writes any frame into any scope, and
  `GET /api/v1/replay` reads the whole retained window of every scope;
- `GET /internal/ws` is the worker/raw relay endpoint and can subscribe to
  internal scopes, including `host:{id}`, which carries execution dispatches;
- `GET /ws` is the schema-checked public UI invalidation protocol and does not
  accept worker commands or raw scopes;
- `POST /api/v1/threads/{id}/messages` starts a run, which is dispatched to a
  worker — and a worker runs provider CLIs and tools and reads files, as its own
  user, on its own machine.

So "who can open the port" is "who can execute commands and read data on every
enrolled machine". Treat it exactly that way.

## The rule

> **Never bind the server to a public interface.**
>
> `LOOM_BIND=0.0.0.0:38886` (the loom spelling of bb's
> `--server-bind-host 0.0.0.0`) publishes an unauthenticated,
> command-executing API on every interface. Do not set it on a host that has a
> routable address. `LOOM_BIND` has no TLS, no auth, no rate limit and no
> origin check behind it.

Keep `LOOM_BIND=127.0.0.1:38886`, which is the default, and put a network that
authenticates *in front of it*.

## Preferred: Tailscale Serve

[Tailscale](https://tailscale.com/) is the recommended answer because it
provides the two things loom deliberately does not: device authentication and
transport encryption. Every device on the tailnet is a named, authorized peer;
nothing else can connect.

On the server machine:

```bash
sudo tailscale up                       # join the tailnet once
# Publish the loopback server on the tailnet. Leave LOOM_BIND on 127.0.0.1.
tailscale serve --bg 38886
tailscale serve status                  # → https://<machine>.<tailnet>.ts.net/
```

The exact `serve` syntax has moved between Tailscale releases; if `--bg 38886`
is rejected, use the explicit form:

```bash
tailscale serve --bg --https=443 http://127.0.0.1:38886
```

That is the whole remote-access setup. The UI is served from the same origin as
the API, so the tailnet HTTPS URL is the URL a browser, an installed PWA and a
desktop webview all use — no per-client server setting.

Because Tailscale terminates TLS and forwards to loopback, the server never
needs a certificate and never needs to know it is remote. `tailscale serve`
proxies WebSocket upgrades as well as HTTP, so `/ws`, `/internal/ws` and the UI
work through it without extra configuration.

Useful commands:

```bash
tailscale serve status        # what is published
tailscale serve reset         # withdraw everything
```

Restrict *which* tailnet devices may reach it with tailnet ACLs. Being on the
tailnet is authorization for the API's purposes; the tailnet must therefore be
trusted, or scoped by ACL.

For a worker on another machine, point it at the tailnet HTTPS URL:

```
LOOM_SERVER_URL=https://<machine>.<tailnet>.ts.net
```

The worker only makes outbound connections, so it needs no inbound rule at all.

## Alternative: a reverse proxy that terminates TLS and authentication

When the client machines are not on a tailnet, terminate TLS **and
authentication** at a reverse proxy and keep the server on loopback. If there is
no real authentication in front, this is not a safe alternative — it is
`0.0.0.0` with extra steps.

Requirements for any proxy:

- **WebSocket upgrade** on both `/ws` and `/internal/ws`. A proxy that strips
  the upgrade or `Sec-WebSocket-Protocol` headers breaks public realtime and
  worker connectivity. Verify `Upgrade`, `Connection` and
  `Sec-WebSocket-Protocol` are forwarded.
- **No buffering of the socket**, or a long read timeout, so idle connections
  are not reaped mid-stream.
- **`X-Forwarded-*` may be ignored**: the UI derives its API and socket from its
  own origin, so it needs no rewrite as long as `/`, `/ws` and `/internal/ws`
  are on one origin.
- **Streaming responses** left alone: `GET /api/v1/replay`, `/ws` and
  `/internal/ws` are not request/response.

```nginx
location / {
    proxy_pass         http://127.0.0.1:38886;
    proxy_http_version 1.1;
    proxy_set_header   Upgrade    $http_upgrade;
    proxy_set_header   Connection "upgrade";
    proxy_read_timeout 3600s;
    proxy_buffering    off;
}
```

Whether the access control is mutual TLS, an identity-aware proxy, a VPN, or a
tailnet is the operator's call; what loom requires is that *something*
authenticates before a packet reaches loopback.

## Firewall

If the host has a public interface at all, prove the port is not on it:

```bash
ss -ltnp | grep 38886          # must be 127.0.0.1:38886, never 0.0.0.0:38886
```

If the optional Redis backend is enabled, apply the same rule to Redis: bind it
to a private interface, require a password, and firewall its port to the server
nodes. The relay log contains every client frame. See
[`redis-backend.md`](redis-backend.md) § Deployment requirements.

## Why not just put a password on the API

Because a half-authenticated control plane is worse than a clearly-scoped one:
it invites exposing it, and every new route becomes another place a missing
check is a remote-code-execution bug. The boundary is the network, and it is
enforced there — by Tailscale, or by a proxy that is treated as the real gate.
