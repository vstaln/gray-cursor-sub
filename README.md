<p align="center">
  <img src="assets/gray-logo.svg" alt="gray" width="96">
</p>
<h1 align="center">gray-cursor-sub</h1>
<p align="center">Run gray turns on your Cursor plan — a Cursor-subscription provider sidecar.</p>

Relays gray chat turns through Cursor's own `cursor-agent acp` subprocess (ACP over
stdio). This matches t3code's `CursorDriver`: the vendor CLI does the talking, and the
sidecar only translates between gray's Responses wire and ACP.

## Requirements

- `cursor-agent` on `PATH` (or set `GRAY_CURSOR_SUB_COMMAND`).
- Signed in: `cursor-agent login`.

## Setup

1. Install the plugin and enable it.
2. Run `/cursor` to check status. It reports the signed-in account and CLI version.
3. Pick a Cursor model; the catalog is fetched live from `cursor/list_available_models`.

Auth method: `cursor_login` (OAuth kind). Credentials stay with the Cursor CLI; the sidecar
stores only a login marker and metadata.

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `GRAY_CURSOR_SUB_COMMAND` | `cursor-agent` on `PATH` | Binary override |
| `GRAY_CURSOR_SUB_MODE` | `agent` | `ask` (read-only Q&A) or `agent` |
| `GRAY_CURSOR_SUB_PERMISSION` | unset | `auto` / `auto-review` → `--auto-review`; `full-access` / `force` → `--force` |
| `GRAY_CURSOR_SUB_CACHE` | `~/.cursor-sub.json` | Credential cache file (the host-collected login material, so `provider/chat` can reach it) |

## License

MIT — see [LICENSE](LICENSE).
