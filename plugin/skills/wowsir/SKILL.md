---
name: wowsir
description: Test and drive websites with the `wowsir` headless browser CLI. Use when asked to check a web page or flow, verify a UI change in the browser, reproduce a frontend bug, or write or run a browser test.
---

# wowsir: browser testing for agents

`wowsir` is a CLI. Each command is one shell call. The browser stays alive between calls.

Run each `wowsir` command on its own, or chain them with `&&` / `;`. Don't pipe its output into other commands (`wowsir text | head`), redirect it (`> /dev/null`), or wrap it in `$(...)`. Those calls get sandboxed, and wowsir can't reach its daemon there. Use wowsir's own filters (`--grep`, `--max`, `--in`) instead. If even plain `wowsir` calls fail with socket or permission errors, the sandbox is blocking it: tell the user to add `"sandbox": { "excludedCommands": ["wowsir *"] }` to their Claude Code settings, or to use wowsir's MCP server.

## Read-only: look without changing anything

On a real or shared system (production, a customer's tenant, anything you weren't asked to change), turn read-only on before you click around:

```bash
wowsir readonly on
```

Then saves, deletes, form posts, WebSocket messages and GETs whose path says `logout`/`delete`/`remove` are blocked. That covers this tab, its frames, and popups or tabs it opens (from their first request). Each block prints `! read-only: blocked POST /api/lights/3 …`, the page shows its normal failure, and `wowsir net --request /api/lights` shows the payload it would have sent. Clicking Save to see what a form sends is therefore safe.

- Some sites open read-only by themselves: the user lists them in `~/.wowsir/readonly`, and `open` says so. A line with `lock` means you can't turn it off; ask the user instead of working around it.
- When the user asks you to change something, turn it off for that one change, then on again:
  `wowsir readonly off` → do the update → `wowsir readonly on`. Say in your reply that you made a real change.
- A request that only reads but is sent as POST (search, GraphQL queries, a SignalR connect): `wowsir readonly allow /api/search`.
- `mock` rules answer before read-only, so `mock POST /api/save 200` fakes a successful save while everything else stays blocked.
- Tests: put `readonly strict` at the top of a `.wowsir` file. A step that tries to write then fails the test (`tried to write while read-only (strict): POST /api/…`), so an accidental write is caught instead of happening.
- `wowsir readonly` shows the state and how many requests were blocked.

## Login

Saved logins (`~/.wowsir/auth/*.json`) load into every new session automatically. If an action prints `! redirected to a login page`, don't try to log in yourself. Ask the user to run `! wowsir login <site-url>`: a window opens, they sign in, and the cookies are saved and pushed into running sessions. If they have a debug Chrome open, `wowsir login --from http://127.0.0.1:9222 <domain>` copies the cookies from it. Use `--no-auth` for a logged-out session. To test the logged-out experience, or to force a fresh sign-in, run `wowsir logout <site>`: it deletes the saved login and wipes that site's cookies and storage from running sessions.

When a sign-in is needed, run `wowsir login <protected-page-url>` with run_in_background yourself. A window opens on the user's screen, they sign in (BankID, Entra, ...), and it closes by itself and saves the login. Tell the user it's waiting for them.

## Core loop

1. `wowsir open <url>` prints a page outline with refs like `@e5`.
2. Act on refs: `wowsir click @e9`, `wowsir fill @e5 "a@b.c"`, `wowsir select @e8 Norway`, `wowsir check @e7`, `wowsir press Enter`.
3. Read the result printed by the action. You usually don't need another snapshot.
   - `ok …` means the action succeeded. `FAIL …` means it didn't, and the output includes the reason and the current interactive elements.
   - `→ url` means the action navigated.
   - `! …` lines are console errors, uncaught exceptions, failed or 4xx/5xx requests, and dialogs. Treat them as bugs to report.
   - `· console.warn: …` summarizes warnings the page logged (deprecations, React key warnings…): worth a look, not necessarily bugs.
   - `~ N changed:` is followed by `+`/`-` lines: what appeared or disappeared on the page.
4. Assert with `wowsir expect text "Welcome"`, `wowsir expect url /dashboard`, or `wowsir expect no-errors`. Exit code 1 means failure.

## Waiting

Actions already wait for the page to settle. For anything slower: `wowsir wait text "Saved"`, `wowsir wait gone "text=Loading"`, `wowsir wait url /done`, or a plain pause with `wowsir wait 500` (milliseconds). After `press Tab`, the output line `focus → …` says where focus went.

## Reading less

- `wowsir snap -i` shows interactive elements only.
- `wowsir snap --grep checkout` shows matching lines plus their parents.
- `wowsir snap --in "form"` or `--in @e12` scopes the outline to one element.
- `wowsir snap --viewport` shows only what's on screen.
- `wowsir text @e20` returns the plain text of one element.
- Use `-q` on an action to skip the diff when you're chaining steps.

## Targets

Quote CSS selectors that start with `#` on the command line (`wowsir text '#total'`): the shell treats an unquoted `#total` as a comment and wowsir never sees it. In `.wowsir` files and over MCP, `#id` works unquoted, and `# note` (hash, then space) is a comment.

`@e12` · `button:Sign in` · `textbox:Email` · `link:Pricing` · `h1:Welcome` · `text=Forgot password` · CSS (`#id`, `[data-testid=x]`).
Chain with ` >> ` to pick an element near another one: `"h2:Shipping >> checkbox:Gift wrap"` takes the first match after that heading in its section, and `"text=You have unsaved changes >> button:Discard"` finds the Discard next to that message. `snap --in <heading>` and `text <heading>` cover the heading's whole section.
A ref stays valid while its element exists. If you get "stale ref", run `wowsir snap`.

## Layout stability

To check that controls don't move (steppers, toolbars, mobile vs desktop):
```
wowsir watch button:Previous button:Next "#bar"
wowsir click button:Next          # → `! moved: button "Next" @e5 y 538→528 (-10)` if it shifted
wowsir expect still               # fails if any watched element moved or jumped since `watch`
wowsir screenshot --mark          # outlines + coordinates of the watched elements, to judge the result
```
wowsir samples watched elements every frame, so `! jumped: … y 538→528→538` reports a control that moved and came back during one action (e.g. while a status message flashed). Positions are page coordinates, or viewport coordinates inside fixed/sticky bars, so scrolling isn't reported as movement. `viewport` changes are reported too.

## Making a regression test from your exploration

```
wowsir record clear
... do the flow with refs ...
wowsir expect text "Order confirmed"
wowsir record save tests/checkout.wowsir     # refs become stable selectors
wowsir run tests/checkout.wowsir            # deterministic replay, no LLM
```

Run a whole suite with `wowsir run tests/*.wowsir --parallel 4`: each file gets its own fresh session.

Multi-step scripts can be written directly: `wowsir run - <<'EOF'` followed by one command per line, then `EOF`.

## Other

- Debugging: `wowsir console --errors`, `wowsir net --failed`, `wowsir net --api`, `wowsir net --body /api/items` (response body of the latest matching request), `wowsir net --request /api/save` (the body a request sent, including blocked and mocked ones), `wowsir errors`, `wowsir eval "window.store.state"` (runs in the page's main world). For longer scripts, or to avoid quoting trouble, write the JavaScript to a file and run `wowsir eval --file check.js`. In `.wowsir` files and over MCP, `eval` takes the rest of the line verbatim, so quotes inside the JavaScript are kept.
- Screenshots: `wowsir screenshot` prints a path. Use it only for visual checks; text is cheaper.
- Parallel or isolated runs: add `-s <name>` to every command. Each session has its own cookies and storage.
- Iframes, including cross-origin ones (payment, sign-in widgets), show up in the snapshot under their `iframe … cross-origin` line; use their refs or role:name targets like any other. Errors from frames and workers end with `(in iframe host)` / `(in worker file.js)`.
- Dark mode: `wowsir theme dark` (also `light`, `reduced-motion`, `off`).
- Mobile: `wowsir viewport mobile` (touch, coarse pointer and a mobile user agent).
- Scrollable lists and panels: `wowsir scroll 'list:Results' 600` (or `down`/`up`) scrolls inside the element; `wowsir scroll T` alone scrolls the page to it.
- Tri-state checkboxes show `mixed`; `expect mixed T` asserts it, and `check`/`uncheck` click through mixed until they reach the state you asked for.
- `prompt()` dialogs: `wowsir dialog accept 'some text'` before the click that opens them.
- Things with no element to target (map markers, canvas, charts): `wowsir click --at 458,341` clicks a viewport point and says what it hit; `wowsir tap 458,341` (or `tap T`) sends a touch tap.
- Multi-select: `wowsir click @e5 --mod` (Cmd on macOS, Ctrl elsewhere), `--shift`, `--ctrl`, `--alt`.
- Long dropdowns: `wowsir snap --full` lists every option untruncated.
- Error/empty/loading states without touching the backend: `wowsir mock /api/items 500`, `wowsir mock /api/items --json '{"items":[]}'`, `wowsir mock /api/items abort` (network failure), `wowsir mock /api/items --delay 3000` (the action returns while the request is held, so you see the loading state). The pattern is a URL substring or glob. `wowsir mock` lists mocks with hit counts, `wowsir mock off` clears them. Mocks apply from the next request, so click refresh or `wowsir reload`. Put a method first to mock only that: `wowsir mock POST /api/lights 200 --json '{}'` fakes the save while the GET that loads the page stays real.
- Tabs: when a click opens a popup or a `target=_blank` page, wowsir switches to it and prints `! new tab N opened`. When a popup closes itself (e.g. after a sign-in), wowsir goes back to the opener. `wowsir tabs` lists tabs, `wowsir tab N` switches, `wowsir tab close` closes the current one.
- Speed up heavy pages: `wowsir block lite` (blocks images, fonts, and media).
- Waiting: actions already wait for the page to settle. Use `wowsir wait text "Done"`, `wowsir wait gone ".spinner"`, or `--timeout 15000` for slow things.
- Clean up with `wowsir close` (ends the session) or `wowsir stop` (stops the daemon and browser).

## When wowsir itself gets it wrong

If wowsir misbehaves (an element missing from the outline, a wrong diff, a crash, a confusing message), tell the user and offer to write a bug report for https://github.com/ErikWahlstrom/wowsir/issues. Include `wowsir --version`, the commands you ran, what you expected and what you got, and a small HTML snippet that reproduces it if you can. The repository is public: describe the site generically and leave out internal URLs, names, credentials and customer data.
