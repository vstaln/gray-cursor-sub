<p align="center">
  <img src="assets/gray-logo.svg" alt="gray" width="96">
</p>
<h1 align="center">gray-cursor-sub</h1>
<p align="center">Run Gray turns and tools through Cursor.</p>

## Native Cursor SDK

The `cursor` provider uses the public Cursor SDK for API-key authentication
(`cursor-api-key`). It discovers models through the SDK and uses a local agent
session for follow-up turns. Gray attachments are sent as native images, and
SDK tool callbacks go back to Gray for execution. Cursor's built-in shell, file,
and subagent tools are not exposed. Disconnecting a running turn cancels it.

1. Install and enable the Gray plugin. For the SDK path, use Node.js 22.13 or later.
2. From the plugin directory containing `package.json`, run `npm ci`. If the
   directory is not writable, copy the package manifests to a directory you
   control, run `npm ci` there, and set `GRAY_CURSOR_SUB_SDK_ROOT` to that directory.
   The plugin does not install Node.js or the SDK for you.
3. Set `CURSOR_API_KEY` in Gray's environment or select the existing Cursor API
   key auth method and supply a Cursor API key to Gray. Gray-supplied keys are
   cached in `GRAY_CURSOR_SUB_CACHE` so the credential-free chat RPC can reach
   them. The cache is created with mode `0600` on Unix. Environment keys take
   precedence for chat. Restart the sidecar after changing the runtime location.

The provider and auth method IDs are unchanged. Gray's `auto` model maps to the
SDK's `default`. Model overrides such as `model[effort=high,fast=false]` use the
SDK's catalog parameters and variants when available.

## Cursor CLI compatibility

The SDK documents API keys and its own sign-in store, not direct reuse of
`cursor-agent login` credentials. Existing CLI logins (`cursor-login`) therefore
continue to work through `cursor-agent acp`, without extracting credentials or
requiring another sign-in. API-key setups with only the CLI installed also keep
working, as do setups that use the existing CLI `CURSOR_API_ENDPOINT` override.
SDK request failures are not retried through the CLI. An explicitly set SDK
runtime location must work; it does not silently fall back when missing.

Run `cursor-agent login` for CLI auth, and use `/cursor status` to check the
available transport. The multi-account pool applies to CLI logins only. Each
managed login has an isolated credential home.

```sh
gray cursor-sub account add <label>          # Sign in to a new home.
gray cursor-sub account add --home <d> <l>    # Adopt an existing home.
gray cursor-sub account list|use|remove|status
```

The pool lives in `$GRAY_HOME/cursor-sub/accounts.json`. If it is absent or
empty, the plugin uses the ambient CLI login. The pool picks the first available
account (`fill_first`), or the least-used account when configured with
`least_used`. `account use` pins an account when available. A 401 or 429 can cool
that account and move the turn to the next one; other errors do not cool it.

## Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `CURSOR_API_KEY` | unset | Cursor API key for `cursor-api-key`. |
| `GRAY_CURSOR_SUB_SDK_ROOT` | Plugin directory | Directory containing `node_modules/@cursor/sdk`. |
| `GRAY_CURSOR_SUB_NODE` | `node` on `PATH` | Node executable override. |
| `GRAY_CURSOR_SUB_COMMAND` | `cursor-agent` on `PATH` | Cursor CLI executable override. |
| `CURSOR_API_ENDPOINT` | unset | Custom CLI endpoint; selects CLI compatibility transport. |
| `GRAY_CURSOR_SUB_MODE` | `agent` | `agent` or `plan`; `ask` is supported by the CLI. SDK mode always restricts execution to Gray's tools. |
| `GRAY_CURSOR_SUB_PERMISSION` | unset | CLI only. `auto` / `auto-review` passes `--auto-review`; `full-access` / `force` passes `--force`. |
| `GRAY_CURSOR_SUB_CACHE` | `~/.cursor-sub.json` | Cache for an API key supplied through Gray. |

## License

MIT — see [LICENSE](LICENSE). The separately installed Cursor SDK is subject
to Cursor's terms.
