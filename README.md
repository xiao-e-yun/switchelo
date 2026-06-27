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
- **Listing** — inspect the registry via `/list`.

## Architecture

The registry core (`src/daemon.rs`) is **transport-agnostic**: it only knows how
to register, unregister, list, and look up services. It is driven through
**inputs** (`src/inputs/`):

- `inputs/http.rs` — the daemon's only runtime input: dynamic registration,
  listing, and path-based proxying.
- `inputs/cli.rs` — the command-line front-end. With no subcommand it runs the
  daemon; with `register`/`unregister` it acts as a thin client that auto-starts
  the daemon if needed and talks to it over HTTP (reusing `reqwest`, which the
  proxy already depends on).

## Running

With no subcommand, `switchelo` runs the daemon in the foreground:

```sh
switchelo                  # serve on $BIND (default 0.0.0.0:8080)
switchelo --bind 0.0.0.0:9000
```

The `register` / `unregister` subcommands act as a **client**. If no daemon is
running they auto-start one in the background, then send the request over HTTP:

```sh
switchelo register api http://127.0.0.1:8081 "main api"
# -> registered 'api' -> http://127.0.0.1:8081 (id=0); route: /api/0/

switchelo unregister 0
```

### Command-line usage

- `switchelo [--bind <ADDR>]` — run the daemon.
- `switchelo register <NAME> <URL> [DESCRIPTION]` — register a service
  (auto-starts the daemon if needed). Equivalent to `POST /registry`.
- `switchelo unregister <ID>` — deregister a service. Equivalent to
  `POST /unregistry`.
- `-b, --bind <ADDR>` — daemon listen/connect address (overrides `BIND`). A
  wildcard host (`0.0.0.0`) is dialed as `127.0.0.1` by the client.
- `-h, --help` — print help and exit.

Environment variables:

- `BIND` — default address (default `0.0.0.0:8080`).
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
{ "id": 0 }
```

The returned `id` is globally unique and auto-incremented. A service must keep
its `id` to know its routing path. Registering the same `name` twice yields two
different ids, which lets you run multiple instances under one name.

Registration is **idempotent by `url`**: reporting in again from the same port
returns the existing `id` (and refreshes its `name`/`description`) instead of
creating a duplicate entry. The trailing slash is normalized, so
`http://host:8081` and `http://host:8081/` are treated as the same backend.

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

List every service grouped by name. Each entry carries the service description
and a map of `id -> instance` for all instances running under that name.

```json
{
  "api": {
    "description": "echo service",
    "services": {
      "0": { "url": "http://127.0.0.1:8081" },
      "1": { "url": "http://127.0.0.1:8082" }
    }
  }
}
```

Instances sharing a name are grouped together; the group's `description` is taken
from one of them.

### `/{name}/{id}/...` — proxy

Forwards the request to the backend registered as `id` (verifying that its name
matches `name`), stripping the `/{name}/{id}` prefix. The query string is
preserved.

| Request             | Forwarded to backend |
|---------------------|----------------------|
| `/api/0`            | `/`                  |
| `/api/0/docs`       | `/docs`              |
| `/api/0/search?q=1` | `/search?q=1`        |

If the backend cannot be reached, the proxy returns `502 Bad Gateway` and the
service is removed from the registry.

## Notes / limitations

- The registry is in-memory only; restarting the server clears it.
- The HTTP client has no request timeout, so a backend that accepts the
  connection but never responds will not trigger active deregistration.
- The request body is buffered fully in memory before forwarding.
