<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.png">
    <img src="assets/logo.png" alt="wowsir" width="420">
  </picture>
</h1>

A fast, headless, agent-first browser for testing websites. It's a single Rust binary (about 1.3 MB) that drives Chromium over a raw CDP pipe. It has no UI, no Node, and no Playwright.

```
$ wowsir open localhost:3000/login
ok open http://localhost:3000/login · 112ms
# Acme Login · http://localhost:3000/login
main
  h1 "Sign in to Acme"
  form
    textbox "Email" @e5 placeholder="you@company.com" type=email required
    textbox "Password" @e6 type=password required
    button "Sign in" @e9

$ wowsir fill @e5 user@example.com && wowsir fill @e6 hunter2 -q
$ wowsir click @e9
ok click button "Sign in" @e9 · 173ms
→ http://localhost:3000/dashboard
! net 404 GET /api/me
~ 3 changed:
  main
- h1 "Sign in to Acme"
+ h1 "Welcome, user@example.com"
...
```

## Why

Agents that test websites spend most of their tokens and round-trips on two things: reading the page, and asking what happened after an action. wowsir cuts both down:

- **Every action reports its own result.** After a click or fill, wowsir waits until the page settles, then returns:
  - the new URL;
  - console errors, uncaught exceptions, failed or 4xx/5xx requests, and dialogs (plus a one-line summary of console warnings);
  - a diff of the page against what the agent last saw.

  The agent doesn't need to re-snapshot after every step.
- **Compact snapshots.** The output is an accessibility-style outline with `@eN` refs, built in one in-page pass (~10–40 ms on Wikipedia/GitHub). Filters keep it small:
  - `-i` shows interactive elements only;
  - `--grep` filters lines;
  - `--in <target>` scopes to a subtree;
  - `--viewport` shows only what's on screen.
- **Stable refs.** A ref stays valid for as long as its element lives, not just until the next snapshot, so diffs and refs agree. Refs from a page you navigated away from are reported as stale. wowsir never silently clicks something else.
- **Settling that matches real apps.** wowsir waits on navigation, network idle, DOM mutation quiet, *and* pending short `setTimeout`s. That covers debounced search, "fetch then render 120 ms later", and similar patterns. Polling loops are ignored, so they don't block it.
- **Built-in testing.** `expect …` assertions retry until a timeout and set the exit code. `record save` turns an agent's exploration into a deterministic test file, with refs rewritten to stable selectors (`data-testid`, then `role:name`, then `#id`, then a CSS path). `run` replays it with no LLM.
- **Fast and parallel.**
  - A daemon keeps one Chromium (`chrome-headless-shell` when available) alive. Cold start is about 0.4 s. On a warm browser, a snapshot takes about 10–40 ms and an action about 30–60 ms, including waiting for the page to settle.
  - Each `-s session` is an isolated browser context with its own cookies and storage, created in about 10 ms inside the same browser. In testing, 8 parallel login tests finished in 0.63 s.
- **Frames, shadow DOM and workers.** Same-origin and cross-origin iframes (payment and sign-in widgets) appear in the snapshot under their `iframe` line, and their refs work like any other: wowsir fills and clicks inside them with real input. Open and closed shadow roots are walked too. Errors from cross-origin frames and web workers are reported with where they came from: `! uncaught … (in iframe pay.example.com)`, `(in worker sync.js)`. Mocks apply inside frames.
- **Real input.** Clicks are trusted CDP mouse events. Before clicking, wowsir checks that the element is visible, enabled, not covered, and not moving, then re-measures after moving the mouse. The page library runs in an isolated JS world that page scripts can't see or break.

## Install

Runs on macOS, Linux and Windows.

```sh
cargo install wowsir
```

Or download a binary for your platform from [Releases](https://github.com/ErikWahlstrom/wowsir/releases) and put it on your `PATH`. The latest unreleased code: `cargo install --git https://github.com/ErikWahlstrom/wowsir`.

wowsir needs a Chromium-based browser. It uses Playwright's `chrome-headless-shell` if present (fastest), else Chrome, Chromium or Edge (on Windows, Edge is always there):

```sh
npx playwright install chromium-headless-shell   # optional, fastest
WOWSIR_CHROME=/path/to/chrome                    # override
```

## Use with coding agents

Install the binary first (above). Then give your agent the wowsir skill, which teaches it the commands, targets and test files.

**Claude Code** — install the plugin, inside a session:

```
/plugin install wowsir --marketplace ErikWahlstrom/wowsir
```

On Claude Code before 2.1.275, run `/plugin marketplace add ErikWahlstrom/wowsir`, then `/plugin install wowsir@wowsir`. The skill updates with each wowsir release when you run `/plugin marketplace update wowsir`, or turn on auto-update for the marketplace in `/plugin`.

**Codex and other agents that read Agent Skills** — copy the skill into your skills folder:

```sh
mkdir -p ~/.agents/skills/wowsir
curl -fsSL -o ~/.agents/skills/wowsir/SKILL.md \
  https://raw.githubusercontent.com/ErikWahlstrom/wowsir/main/plugin/skills/wowsir/SKILL.md
```

The release downloads include it too, as `skills/wowsir/SKILL.md`. For Claude Code without the plugin, put it in `~/.claude/skills/wowsir/`.

Agents without a shell can use wowsir as an [MCP server](#as-an-mcp-server) instead.

**Sandboxed shells:** the CLI talks to its background process over a local socket and starts Chrome, which agent sandboxes usually block. In Claude Code, let `wowsir` run outside the sandbox in `~/.claude/settings.json`:

```json
{ "sandbox": { "excludedCommands": ["wowsir *"] } }
```

Or use the MCP server, which runs outside the agent's shell sandbox.

## Usage

```
wowsir help
```

| | |
|---|---|
| Navigate | `open URL` `back` `forward` `reload` `url` `title` |
| Layout | `watch T…` (each action then reports `! moved:` / `! jumped:` for those elements, sampled every frame) · `expect still [T…]` · `screenshot --mark` |
| Mock | `mock /api/items 500` · `mock /api/items --json '{"items":[]}'` · `mock POST /api/x 200` (one method only) · `mock /api/x abort` · `mock /api/x --delay 3000` · `mock off` |
| Read-only | `readonly on\|strict\|off` · `readonly allow PATTERN` · `readonly` (status) — see [Read-only mode](#read-only-mode) |
| Tabs | `tabs` `tab N` `tab new [URL]` `tab close [N]` (popups and `target=_blank` pages are followed automatically) |
| See | `snap [-i] [--grep X] [--in T] [--viewport] [--full] [--urls] [--ids] [--all]` `text [T]` `html [T]` `screenshot [PATH] [--full] [--el T]` |
| Act | `click T [--ctrl\|--shift\|--mod]` `click --at X,Y` `tap T\|X,Y` `fill T TEXT` `type [T] TEXT` `press KEY…` `select T OPT…` `check T` `uncheck T` `hover T` `focus T` `upload T FILE…` `scroll [down\|up\|top\|bottom\|T]` `scroll T down\|up\|PX` (inside a scrollable element) |
| Wait | `wait MS` `wait T` `wait text X` `wait gone T` `wait url X` `wait idle` |
| Assert | `expect text\|no-text X` `expect url\|title X` `expect visible\|hidden T` `expect value T V` `expect checked\|unchecked\|mixed T` `expect count T N` `expect no-errors` |
| Debug | `console [--errors]` `net [--failed] [--api] [--grep X]` `net --body PATTERN` `net --request PATTERN` `errors` `eval JS` `cookies [clear]` |
| Setup | `viewport W H [--mobile]` / `viewport mobile` (touch + mobile user agent) · `theme dark\|light\|off` · `block images fonts media css\|lite\|off` · `dialog accept [TEXT]\|dismiss` (TEXT answers `prompt()`) |
| Tests | `run FILE...\|- [--parallel N]` · `record show\|save FILE\|clear` |
| Sessions | `-s NAME` / `WOWSIR_SESSION` · `sessions` · `close` · `stop` · `status` |

**Targets:** `@e12` (snapshot ref) · `button:Sign in` (role:name, case-insensitive, exact before partial) · `text=Forgot password` · any CSS selector (pierces open and closed shadow DOM) · `A >> B` (B near A: inside it, or the first one after it in its section).

**Global flags:** `-q` (no diff) · `--snap` (full snapshot instead of diff) · `--timeout MS` · `--headed`.

### Read-only mode

For looking at systems you must not change. `wowsir readonly on` blocks, for the tab, its frames and any popup or tab it opens (from their first request):

- POST, PUT, PATCH and DELETE requests, including form posts and `sendBeacon`
- outgoing WebSocket messages (incoming ones still arrive)
- GETs whose path says `logout`, `logoff`, `signout`, `delete`, `remove` or `destroy` (a stray `/logout` would also end your saved login)
- service workers are bypassed, so one can't send writes wowsir doesn't see

Each block is reported in the action's output, the page sees an ordinary network failure, and `wowsir net --request PATTERN` shows the payload. `mock` rules answer first (`mock POST /api/save 200` fakes a save), and `readonly allow PATTERN` lets a read that's sent as POST through.

`readonly strict` also fails the step that tried to write, so a test with `readonly strict` at the top catches accidental writes.

**Sites that are always read-only:** list them in `~/.wowsir/readonly` (or `$WOWSIR_READONLY_FILE`). Every session that opens one starts read-only, including when a link or redirect leads there:

```text
# one site per line, subdomains included; * = every site except localhost
staging.example.com
prod.example.com lock          # lock: `readonly off` is refused
ci.example.com strict          # strict: a blocked write fails the step
```

Read-only stops what leaves the browser. It can't stop changes made by other means (WebRTC, extensions), and it doesn't make a page behave as if the write succeeded.

### As an MCP server

```sh
claude mcp add --scope user wowsir -- wowsir mcp      # Claude Code; any MCP client works the same way
```

`wowsir mcp` exposes two tools:
- **`browser`** takes one wowsir command, e.g. `click @e5`.
- **`browser_run`** takes several commands, one per line.

Each call runs the same command line as the CLI and returns exactly what the CLI prints, so the two can't drift apart: every CLI command is also available over MCP. The CLI and MCP share the daemon, sessions and saved logins. Use the CLI for scripts, CI (`wowsir run tests/*.wowsir`) and agents with a shell. Use MCP for clients without one, or to skip shell sandbox setup.

The MCP surface is kept small on purpose, because tool definitions cost context in every session:

| | tools | definitions loaded per session |
|---|---|---|
| wowsir mcp | 2 | ~500 tokens |
| chrome-devtools-mcp 1.12 | 32 | ~6,900 tokens |

### Logins

```sh
wowsir login https://app.example.com/some/protected/page   # a window opens; a human signs in; it closes by itself
wowsir login --from http://127.0.0.1:9222 example.com      # or copy one site's cookies from a debug Chrome
wowsir logout example.com                                  # forget it, and wipe it from running sessions
```

- **Saved logins load automatically.** They live in `~/.wowsir/auth/`, readable only by you, and load into every session (`--no-auth` gives a clean one). They include cookies plus localStorage/sessionStorage, which is where MSAL/Entra keeps its tokens.
- **Refreshed cookies are saved back,** so a login lasts as long as the server allows.
- **wowsir tells the agent when it's signed out.** It warns on a redirect to a sign-in page, a `401` from the site's own API, or an expired saved login, and names the `wowsir login` command to run.

Tested with BankID and Microsoft Entra sign-ins.

### Test files

```sh
# tests/login.wowsir: one command per line, `#` comments
open localhost:8765
fill textbox:Email user@example.com
fill textbox:Password hunter2
click '[data-testid="submit"]'
expect text 'Welcome, user'
expect url /dashboard
```

```
$ wowsir run tests/login.wowsir
...
✓ 7/7 steps passed · 451ms
```

`run` stops at the first failure, prints the failing line and reason, and exits with code 1.

With several files, each runs in its own fresh session (own cookies, tabs and mocks; saved logins still load), and `--parallel N` runs N at a time in the same browser. The 7 files in `tests/` take about 1.4 s with `--parallel 8`. Output is printed per file, in order, followed by `✓ 7/7 files passed`.

```
wowsir run tests/*.wowsir --parallel 4
```

### In CI

GitHub's Ubuntu and macOS runners come with Google Chrome, which wowsir finds by itself:

```yaml
- run: cargo install --git https://github.com/ErikWahlstrom/wowsir
- run: wowsir run e2e/*.wowsir --parallel 4     # exit code 1 on the first failing step
  env:
    WOWSIR_NO_SANDBOX: "1"   # Chrome's sandbox often can't start in CI containers (automatic when running as root)
```

Start your app first and point the tests at it (`open localhost:3000/...`). This repo's own workflow is in [.github/workflows/ci.yml](.github/workflows/ci.yml).

## Benchmark

Seven scenarios on public sites (W3C ARIA examples, the-internet.herokuapp.com, TodoMVC), each driven the way an agent would with wowsir and with chrome-devtools-mcp. wowsir used 47 calls and about 10k tokens; chrome-devtools-mcp used 55 calls and about 125k tokens, and could not pick an option from the W3C combobox because its snapshot left the options out. Details and how to reproduce: [bench/RESULTS.md](bench/RESULTS.md).

## Architecture

```
wowsir (CLI, sync, ~1ms) ──unix socket / named pipe──▶ wowsir daemon (tokio)
                                          ├─ Chromium via --remote-debugging-pipe (fd 3/4 or inherited handles, NUL-delimited JSON)
                                          ├─ session = browser context + page + event tracker
                                          │    network inflight / console / exceptions / dialogs / popups
                                          └─ src/inject.js in an isolated world:
                                               snapshot, refs (WeakMap), target resolution,
                                               actionability, selectors for recording
```

- `src/cdp.rs`: about 150 lines of CDP client, with no generated bindings.
- `src/platform.rs`: everything that differs between Unix and Windows.
- `src/session.rs`: event tracking, the settle algorithm, and the isolated world.
- `src/commands.rs`: every command, plus the diff renderer.
- `src/inject.js`: everything that touches the DOM.

## How it compares

- **[vercel-labs/agent-browser](https://agent-browser.dev)** is the closest prior art: a Rust CLI and daemon with snapshot refs. wowsir focuses on testing:
  - actions return diffs plus errors;
  - refs are stable across snapshots;
  - assertions are built in;
  - sessions can be recorded and replayed;
  - settling also waits on timers.
- **Playwright MCP and chrome-devtools-mcp** are general-purpose and verbose. MCP tool schemas and full snapshots cost a lot of context.
- **[Lightpanda](https://lightpanda.io)** is a new non-Chromium engine. It's faster to boot, but it lacks layout and rendering and only partially supports Web APIs. wowsir uses real Chromium so test results match what users see. The CDP layer is engine-agnostic, so pointing `WOWSIR_CHROME` at another CDP-pipe browser could work.

## Limits / TODO

- Inside a cross-origin iframe, `watch`, `screenshot --el` and `html` see only the outer page; frames nested inside a cross-origin frame are not walked.

## Dev

```sh
./tests/smoke.sh     # builds, serves tests/fixtures, runs tests/*.wowsir
```

Pull requests are squash-merged, and the PR title becomes the commit message, so it follows [Conventional Commits](https://www.conventionalcommits.org/): `fix: …` for bug fixes, `feat: …` for new features, and `docs:`, `ci:`, `test:` or `chore:` for changes that don't need a release.

Releases are automatic: after each merge, [release-please](https://github.com/googleapis/release-please) keeps a "release X.Y.Z" PR up to date with the version bump and `CHANGELOG.md`. A `fix:` gives a patch release and a `feat:` a minor one. Merging that PR tags the release; the release workflow then builds the binaries, attaches them to the GitHub release and publishes to crates.io.
