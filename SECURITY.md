# Security policy

## Supported versions

Only the latest `v0.x` release receives fixes.

## Reporting a vulnerability

Open a GitHub issue with the `security` label, or contact the maintainer
privately. Please allow 14 days before public disclosure.

## Known posture (v0.1.0)

- Auth is a single shared bearer token (`SE_TOKEN`); there are no users,
  roles, or TLS termination — run behind a reverse proxy in production.
- Request bodies are capped at 8 MiB; queries at 4096 chars; bulk at
  100k docs. JSON parsing is a hand-rolled bounded parser.
- Token comparison is a plain string equality check; timing attacks are out
  of scope for a single-secret demo setup but noted here.
- The server binds `127.0.0.1` by default; the Docker image binds
  `0.0.0.0:3000` (set `SE_TOKEN` via compose env).
- No secrets are logged; access logs go to stderr as JSON lines.
