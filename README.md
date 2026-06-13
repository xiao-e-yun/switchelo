# switchelo

A lightweight dynamic service registry and reverse proxy. Backend services
register themselves at startup, and incoming requests are routed to them based
on the URL path. Dead backends are removed automatically.

## Features

- **Dynamic registration** — services report in at startup via `POST /registry`.
- **Dynamic unregistration** — services go offline gracefully via `POST /unregistry`.
- **Path-based routing & stripping** — `/{name}/{id}/...` is forwarded to the
  matching backend with the `/{name}/{id}` prefix stripped.
- **Fault tolerance** — when forwarding fails (backend unreachable), the service
  is deregistered automatically.
- **Listing** — inspect the registry via `/list` and `/{name}/list`.

## Running

```sh
cargo run
```

Environment variables:

- `BIND` — listen address (default `0.0.0.0:8080`).
- `RUST_LOG` — log filter (default `switchelo=info`).

## API

### `POST /registry`

Register a backend service.

Request:

```json
{ "name": "api", "url": "http://127.0.0.1:8081", "description": "optional" }
```

Response:

```json
{ "success": true, "id": 0 }
```

The returned `id` is globally unique and auto-incremented. A service must keep
its `id` to know its routing path. Registering the same `name` twice yields two
different ids, which lets you run multiple instances under one name.

### `POST /unregistry`

Deregister a service.

Request:

```json
{ "id": 0 }
```

Response:

```json
{ "success": true }
```

`success` is `false` if no service with that id existed.

### `GET /list`

List all registered services.

```json
[
  { "id": 0, "name": "api", "url": "http://127.0.0.1:8081", "description": "" }
]
```

### `GET /{name}/list`

List all instances registered under `name` (same shape as `/list`).

### `/{name}/{id}/...` — proxy

Forwards the request to the backend registered as `id` (verifying that its name
matches `name`), stripping the `/{name}/{id}` prefix. The query string is
preserved.

| Request                  | Forwarded to backend |
|--------------------------|----------------------|
| `/api/0/docs`            | `/docs`              |
| `/api/0/`               | `/`                  |
| `/api/0/search?q=1`      | `/search?q=1`        |

The **trailing slash is required**: `/api/0/` is valid, but `/api/0` (no
trailing slash) returns `404`.

If the backend cannot be reached, the proxy returns `502 Bad Gateway` and the
service is removed from the registry.

## Notes / limitations

- The registry is in-memory only; restarting the server clears it.
- The HTTP client has no request timeout, so a backend that accepts the
  connection but never responds will not trigger active deregistration.
- The request body is buffered fully in memory before forwarding.
