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

Copy `vpm-shop.conf.example` to `vpm-shop.conf` and fill in your values:

| Variable      | Description                                              |
|---------------|----------------------------------------------------------|
| `VPM_SHOP`    | Where the VPM packages/shop live on disk                 |
| `VALIDATOR`   | Absolute path to the VCC manifest validator DLL          |
| `MASTER_HOST` | Public base URL of the registry (no trailing slash)      |
| `BIND`        | Listen address, e.g. `0.0.0.0:55555`                     |
| `API_USER`    | Upload username (can also be set via env/systemd)        |
| `API_PASS`    | Upload password (can also be set via env/systemd)        |
| `PKG_AUTHOR`  | Default author name stamped into uploaded package.json   |
| `RUST_LOG`    | Tracing filter (default `info`)                          |

`vpm-shop.conf` is gitignored — never commit real URLs or credentials.

## Build & run

```sh
./build.sh            # release build -> target/release/vpm-upload-api
./launch.sh           # run in foreground
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
