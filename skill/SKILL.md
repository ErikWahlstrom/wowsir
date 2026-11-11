---
name: kite
description: Test and drive websites with the `kite` headless browser CLI. Use when asked to check a web page or flow, verify a UI change in the browser, reproduce a frontend bug, or write or run a browser test.
---

# kite: browser testing for agents

`kite` is a CLI. Each command is one shell call. The browser stays alive between calls.

Run each `kite` command on its own, or chain them with `&&` / `;`. Don't pipe its output into other commands (`kite text | head`), redirect it (`> /dev/null`), or wrap it in `$(...)`. Those calls get sandboxed, and kite can't reach its daemon there. Use kite's own filters (`--grep`, `--max`, `--in`) instead.

## Login

Saved logins (`~/.kite/auth/*.json`) load into every new session automatically. If an action prints `! redirected to a login page`, don't try to log in yourself. Ask the user to run `! kite login <site-url>`: a window opens, they sign in, and the cookies are saved and pushed into running sessions. If they have a debug Chrome open, `kite login --from http://127.0.0.1:9222 <domain>` copies the cookies from it. Use `--no-auth` for a logged-out session. To test the logged-out experience, or to force a fresh sign-in, run `kite logout <site>`: it deletes the saved login and wipes that site's cookies and storage from running sessions.

When a sign-in is needed, run `kite login <protected-page-url>` with run_in_background yourself. A window opens on the user's screen, they sign in (BankID, Entra, ...), and it closes by itself and saves the login. Tell the user it's waiting for them.

## Core loop

1. `kite open <url>` prints a page outline with refs like `@e5`.
2. Act on refs: `kite click @e9`, `kite fill @e5 "a@b.c"`, `kite select @e8 Norway`, `kite check @e7`, `kite press Enter`.
3. Read the result printed by the action. You usually don't need another snapshot.
   - `ok …` means the action succeeded. `FAIL …` means it didn't, and the output includes the reason and the current interactive elements.
   - `→ url` means the action navigated.
   - `! …` lines are console errors, uncaught exceptions, failed or 4xx/5xx requests, and dialogs. Treat them as bugs to report.
   - `~ N changed:` is followed by `+`/`-` lines: what appeared or disappeared on the page.
4. Assert with `kite expect text "Welcome"`, `kite expect url /dashboard`, or `kite expect no-errors`. Exit code 1 means failure.

## Waiting

Actions already wait for the page to settle. For anything slower: `kite wait text "Saved"`, `kite wait gone "text=Loading"`, `kite wait url /done`, or a plain pause with `kite wait 500` (milliseconds). After `press Tab`, the output line `focus → …` says where focus went.

## Reading less

- `kite snap -i` shows interactive elements only.
- `kite snap --grep checkout` shows matching lines plus their parents.
- `kite snap --in "form"` or `--in @e12` scopes the outline to one element.
- `kite snap --viewport` shows only what's on screen.
- `kite text @e20` returns the plain text of one element.
- Use `-q` on an action to skip the diff when you're chaining steps.

## Targets

Quote CSS selectors that start with `#` on the command line (`kite text '#total'`): the shell treats an unquoted `#total` as a comment and kite never sees it. In `.kite` files and over MCP, `#id` works unquoted, and `# note` (hash, then space) is a comment.

`@e12` · `button:Sign in` · `textbox:Email` · `link:Pricing` · `h1:Welcome` · `text=Forgot password` · CSS (`#id`, `[data-testid=x]`).
Chain with ` >> ` to pick an element near another one: `"h6:Motion Sensors - Low >> checkbox:PIR kitchen"` takes the first match after that heading in its section, and `"text=You have unsaved changes >> button:Discard"` finds the Discard next to that message. `snap --in <heading>` and `text <heading>` cover the heading's whole section.
A ref stays valid while its element exists. If you get "stale ref", run `kite snap`.

## Layout stability

To check that controls don't move (steppers, toolbars, mobile vs desktop):
```
kite watch button:Previous button:Next "#bar"
kite click button:Next          # → `! moved: button "Next" @e5 y 538→528 (-10)` if it shifted
kite expect still               # fails if any watched element moved or jumped since `watch`
kite screenshot --mark          # outlines + coordinates of the watched elements, to judge the result
```
kite samples watched elements every frame, so `! jumped: … y 538→528→538` reports a control that moved and came back during one action (e.g. while a status message flashed). Positions are page coordinates, or viewport coordinates inside fixed/sticky bars, so scrolling isn't reported as movement. `viewport` changes are reported too.

## Making a regression test from your exploration

```
kite record clear
... do the flow with refs ...
kite expect text "Order confirmed"
kite record save tests/checkout.kite     # refs become stable selectors
kite run tests/checkout.kite            # deterministic replay, no LLM
```

Run a whole suite with `kite run tests/*.kite --parallel 4`: each file gets its own fresh session.

Multi-step scripts can be written directly: `kite run - <<'EOF'` followed by one command per line, then `EOF`.

## Other

- Debugging: `kite console --errors`, `kite net --failed`, `kite net --api`, `kite net --body /api/items` (response body of the latest matching request), `kite errors`, `kite eval "window.store.state"` (runs in the page's main world). For longer scripts, or to avoid quoting trouble, write the JavaScript to a file and run `kite eval --file check.js`. In `.kite` files and over MCP, `eval` takes the rest of the line verbatim, so quotes inside the JavaScript are kept.
- Screenshots: `kite screenshot` prints a path. Use it only for visual checks; text is cheaper.
- Parallel or isolated runs: add `-s <name>` to every command. Each session has its own cookies and storage.
- Mobile: `kite viewport mobile` (touch, coarse pointer and a mobile user agent).
- Scrollable lists and panels: `kite scroll 'list:Results' 600` (or `down`/`up`) scrolls inside the element; `kite scroll T` alone scrolls the page to it.
- Tri-state checkboxes show `mixed`; `expect mixed T` asserts it, and `check`/`uncheck` click through mixed until they reach the state you asked for.
- `prompt()` dialogs: `kite dialog accept 'some text'` before the click that opens them.
- Multi-select: `kite click @e5 --mod` (Cmd on macOS, Ctrl elsewhere), `--shift`, `--ctrl`, `--alt`.
- Long dropdowns: `kite snap --full` lists every option untruncated.
- Error/empty/loading states without touching the backend: `kite mock /api/items 500`, `kite mock /api/items --json '{"items":[]}'`, `kite mock /api/items abort` (network failure), `kite mock /api/items --delay 3000` (the action returns while the request is held, so you see the loading state). The pattern is a URL substring or glob. `kite mock` lists mocks with hit counts, `kite mock off` clears them. Mocks apply from the next request, so click refresh or `kite reload`.
- Tabs: when a click opens a popup or a `target=_blank` page, kite switches to it and prints `! new tab N opened`. When a popup closes itself (e.g. after a sign-in), kite goes back to the opener. `kite tabs` lists tabs, `kite tab N` switches, `kite tab close` closes the current one.
- Speed up heavy pages: `kite block lite` (blocks images, fonts, and media).
- Waiting: actions already wait for the page to settle. Use `kite wait text "Done"`, `kite wait gone ".spinner"`, or `--timeout 15000` for slow things.
- Clean up with `kite close` (ends the session) or `kite stop` (stops the daemon and browser).
