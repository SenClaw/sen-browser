# sen-browser

SenClaw's browser runtime: the *hands and eyes* of the browser agent. It
observes a page in one atomic call, executes one guarded action with trusted
CDP input, and hands a tab to the person when a sign-in is needed. It never
sees the goal and never calls a model — the decision loop (Jev + LLM) lives in
the daemon's control plane (`senclaw/src/browser_agent`).

Launched by the SenClaw daemon as a child process (slot `browser`) and driven
over loopback HTTP — see [`senclaw/docs/runtime-protocol.md`](../senclaw/docs/runtime-protocol.md).

## Two drivers, one core

| | managed | extension |
|---|---|---|
| Browser | a Chrome this runtime launches, profile `<data_dir>/profiles/<name>` | the person's Chrome |
| Transport | CDP over the profile's `DevToolsActivePort` (random loopback port) | the SenClaw extension's `chrome.debugger`, relayed by the daemon over `/v1/drivers/extension` |
| Handover | Chrome relaunches with a window, same profile | the debugger detaches, the tab comes to the front |

Everything above the tab channel is shared: the snapshot, guards, execution
and settle logic in `src/tab.rs`.

## Safety model

- **Only approved scripts run in a page**, inside SenClaw's isolated world
  (`scripts/*.js`, sent with `Runtime.callFunctionOn`). Their SHA-256 hashes
  form the bundle the extension enforces; `sen-browser scripts` prints it.
- **Only allowlisted CDP methods** reach a tab (`src/allowlist.rs`) — no
  `Network`, `Storage`, `Runtime.evaluate`, file uploads.
- **An action is refused before any input** when the page changed since the
  observation (scoped guard for click/select, full marker otherwise), when
  another element covers the target, or when the field is credential-like.
- **Mutations are never retried**; an interrupted `<select>` is reported as
  `uncertain_mutation`.
- Model output never becomes selectors, coordinates or code: an action is an
  id from the latest observation.

## When a page is ready

A navigation — `navigate`, or an action that starts one (a link, a submitted
form, going back) — is followed to the document it brings, and that document
is observed when it shows what a person would act on. The load event alone is
the wrong signal in both directions: a video stream, an ad or a tracker can
hold it back for many seconds (or for good) while the page is usable, and an
app shell fires it before fetching what it will show. `wait_ready` in
`src/tab.rs` ends the wait on the first of:

| `ready_by` | The document… |
|---|---|
| `load` | fired its load event and is not an app shell: it already shows a full page, or loads no script that could fill it in |
| `quiet` | is parsed, its network went quiet and what it shows stopped changing. Quiet is Chrome's own idle detection: before the load event at most two requests in flight (stragglers), after it none (one request may be all an app's data) |
| `stable` | is parsed and what it shows has stayed the same for 1.5 s (a network that never goes quiet) |
| `grace` | has been parsed for 4 s |
| `deadline` | did not arrive within 15 s |
| `restored`, `same_document`, `cancelled`, `dialog` | came back from the back/forward cache, never left the document, was cancelled (a download), or is blocked by a dialog |

An observation after `navigate` carries `timing_ms.ready` and
`timing_ms.ready_by`; an `act` answer carries `ready_ms` and `ready_by` (null
when the action started no navigation). Chrome holds every call to a page
while a navigation is pending, so nothing is read from the page being left.

## API (bearer token on every route but `/health`)

```
GET    /v1/status                     chrome, extension, script bundle, sessions
GET    /v1/scripts                    {bundle, scripts:[{name, sha256}]}
GET    /v1/sessions                   · POST {driver: managed|extension, profile?, headless?}
DELETE /v1/sessions/:sid
GET    /v1/sessions/:sid/tabs         · POST {url?, owner?, ext_tab?}  → {tab, created, observation}
DELETE /v1/tabs/:tid
POST   /v1/tabs/:tid/observe          {text_chars?, max_candidates?}  → observation
POST   /v1/tabs/:tid/act              {observation_id, action_id, text?} → {executed, executed_ms, settle_ms, ready_ms, ready_by, observation}
POST   /v1/tabs/:tid/navigate         {url}
POST   /v1/tabs/:tid/read             {max_chars?}  → {url, title, text, links}
POST   /v1/tabs/:tid/screenshot       → {mime, data}
POST   /v1/tabs/:tid/handover         · DELETE (resume)
GET    /v1/drivers/extension          WebSocket: the daemon's relay to the extension
```

Errors are `{"error", "code"}`: `stale_page` and `target_covered` (409, nothing
was sent — observe again), `invalid_target` (422), `dialog_open`,
`handover_active`, `user_active`, `debugger_detached` (423),
`extension_not_connected` (503), `uncertain_mutation` (500, never retried).

## Build & test

```bash
cargo test          # unit tests + real-Chrome tests (skipped without Chrome)
make package        # dist/sen-browser-<version>-<platform>.tar.gz + .sha256
make run-dev        # standalone on :4970, no token
```

`SEN_BROWSER_CHROME` points at a specific browser binary; `SEN_BROWSER_HEADFUL=1`
launches managed Chrome with a window.

## Install and release

SenClaw installs the runtime from its catalog (Settings → Runtime → Engines &
Frameworks, type *Browser*) once a release is published: pushing a `v*` tag runs
`.github/workflows/release.yml`, which attaches
`sen-browser-<version>-<platform>.tar.gz` (+ `.sha256`) for darwin-arm64,
linux-x64 and windows-x64; the daemon's `runtimes/index.json` then lists each
archive's URL, sha256 and size. Until then, `make package` and use **Install
from a local path** with the archive (desktop and web), or
`senclaw runtime install-local dist/<archive>` — never against a live
`~/.senclaw` while developing.
