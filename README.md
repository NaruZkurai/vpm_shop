# vpm_shop

Rust (axum) upload API and web UI for a self-hosted **VPM (VRChat Package Manager)** shop.

This is the **system** repository — the server application itself. It contains **no packages, no unitypackage files, and no personal data**. Uploaded packages live in the shop directory (`VPM_SHOP`), not in this repo.

## Features

- **Upload API** (`POST /upload`): multipart uploads of `.unitypackage`, `.zip`, or raw files. Auth via `API_USER`/`API_PASS` form fields.
  - Auto-builds deterministic VPM zips (package.json + assets).
  - Auto-versioning: existing versions become `<version>-rc.<N>` unless a demote/RC slot is requested.
  - Validates every build against the real VCC manifest DLL (`VALIDATOR`).
  - Regenerates the master `vpm-repo.json` and category repos after each upload.
- **REST APIs**: package listing, file trees, file contents, package.json, dependencies (read + edit), version deletion, and a password-protected checklist.
- **Web UI**: dark-mode single-page app with Browse + Upload tabs, folder tree file viewer, and package.json viewer.

## Configuration

On first run, `./launch.sh` generates a `vpm-shop.conf` with default settings
and a freshly generated username/password if none exists. It also creates the
default shop directory (`./mnt/shop`) and, inside the server, the category
subdirs + an empty master `vpm-repo.json` are bootstrapped automatically.

Credentials are **never hardcoded** — `API_USER`/`API_PASS` must come from
`vpm-shop.conf` (or the environment), or the server refuses to start. Copy
`vpm-shop.conf.example` to `vpm-shop.conf` and adjust for your environment.

| Variable      | Description                                              |
|---------------|----------------------------------------------------------|
| `VPM_SHOP`    | Where the VPM packages/shop live on disk (default `./mnt/shop`) |
| `VALIDATOR`   | Absolute path to the VCC manifest validator DLL          |
| `MASTER_HOST` | Public base URL of the registry (no trailing slash)      |
| `BIND`        | Listen address, e.g. `0.0.0.0:55555`                     |
| `API_USER`    | Upload username (required — set in `vpm-shop.conf`)      |
| `API_PASS`    | Upload password (required — set in `vpm-shop.conf`)      |
| `PKG_AUTHOR`  | Default author name stamped into uploaded package.json   |
| `RUST_LOG`    | Tracing filter (default `info`)                          |

`vpm-shop.conf` is gitignored — never commit real URLs or credentials.

## Build & run

```sh
./build.sh            # release build -> target/release/vpm-upload-api
./launch.sh           # run in foreground (generates vpm-shop.conf on first run)
./launch.sh --background
```

Or via systemd with `EnvironmentFile=/path/to/vpm-shop.conf`.

## Endpoints

- `GET /` — web UI
- `GET /health` — health check
- `POST /upload` — upload & publish a package
- `GET /api/packages` — list all packages
- `GET /api/package/{name}/{version}/files|file|json|deps`
- `POST /api/package/{name}/{version}/deps` — edit dependencies
- `DELETE /api/delete/{name}/{version}` — delete a version
- `GET/POST /api/checklist` — personal to-upload checklist (X-Api-Password header)
