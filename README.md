# vpm-login-api

A tiny Rust (`axum`) HTTP/HTTPS API that behaves like a "deny everything"
endpoint:

- **Every** request — any path, any method — returns HTTP `401 Not Authorised`.
- The client IP is **logged** to `logs/access.log` (and the console).
- The same IP is **included in the JSON error body**.

## Response

```http
HTTP/1.1 401 Unauthorized
content-type: application/json

{
  "error": {
    "code": "not authorised",
    "ip": "127.0.0.1"
  }
}
```

## Configuration

Environment variables (also supported via `.env`, see `.env.example`):

| Variable      | Default                    | Description                                      |
| ------------- | -------------------------- | ------------------------------------------------ |
| `PORT`        | `2095` / `2096` (with TLS) | Port to bind. Defaults to unused Cloudflare-proxyable ports. |
| `USE_TLS`     | `0`                        | `1`/`true` to serve HTTPS instead of HTTP.       |
| `TLS_CERT`    | `certs/server.crt`         | PEM certificate (auto-generated if missing).     |
| `TLS_KEY`     | `certs/server.key`         | PEM private key (auto-generated if missing).     |
| `ACCESS_LOG`  | `logs/access.log`          | File that receives one line per request + IP.    |
| `RUST_LOG`    | `info`                     | Console logging filter.                          |

## Run

```bash
cargo run            # HTTP on :2095 (Cloudflare proxyable, no root needed)
USE_TLS=1 cargo run  # HTTPS on :2096 (Cloudflare proxyable, no root needed)
```

Test it:

```bash
curl -i http://127.0.0.1:2095/anything
curl -i -X POST http://127.0.0.1:2095/login
curl -ki https://127.0.0.1:2096/   # -k ignores the self-signed cert
```

## Scripts

```bash
./build.sh            # release build -> target/release/vpm-login-api
./build.sh --debug    # debug build   -> target/debug/vpm-login-api

./launch.sh                 # run in foreground (builds first if needed)
./launch.sh --background    # run in background, console logs -> logs/api.log
```

`launch.sh` reads the same env vars / `.env` as the binary itself.

## Cloudflare

With Cloudflare's proxy enabled (orange cloud), the edge only forwards to these
origin ports:

| HTTP origin ports  | HTTPS origin ports  |
| ------------------ | ------------------- |
| `80`, `8080`, `8880`, `2052`, `2082`, `2086`, `2095` | `443`, `2053`, `2083`, `2087`, `2096`, `8443` |

This machine already has `80`, `8080`, `443`, and `8443` taken by other services,
so the API defaults to the next Cloudflare-proxyable pair: `2095` (HTTP) and
`2096` (HTTPS) — both above `1024`, so **no root required**.

- SSL mode **Flexible** → plain HTTP origin on `2095` (default).
- SSL mode **Full / Full (strict)** → HTTPS origin on `2096` with `USE_TLS=1` and a
  real certificate at `TLS_CERT`/`TLS_KEY`.

You can switch to any other port in the tables above via `PORT` (e.g. `8880` for
HTTP or `2053` for HTTPS). Only `80`/`443` require root/setcap, e.g.
`sudo setcap cap_net_bind_service=+ep target/debug/vpm-login-api`.

Every response is `401` with `{"error":{"code":"not authorised","ip":"<client ip>"}}`,
and the IP is appended to `logs/access.log`.

## Notes

- Client IP resolution prefers the `X-Forwarded-For` / `X-Real-IP` headers (set by
  reverse proxies such as nginx, caddy, cloudflare). **If you expose this directly
  to the internet, those headers are spoofable** — either use them only behind a
  trusted proxy, or remove the proxy-header handling and rely on the TCP peer address.
- For production TLS, drop your real certificate/key at `TLS_CERT`/`TLS_KEY`; the
  self-signed generation only runs when the files are missing.
