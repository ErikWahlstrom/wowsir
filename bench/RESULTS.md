# kite vs chrome-devtools-mcp

2026-11-11 · chrome-devtools-mcp 1.12.0 · 3 runs per tool, medians · same Chromium binary for both

Reproduce:

```sh
./bench/fetch-sites.sh                                   # W3C examples at a pinned commit (14 MB)
node bench/bench.mjs bench/scenarios/*.mjs --runs 3 --mcp
```

## Sites

| scenario | site | what it exercises |
|---|---|---|
| apg-combobox | [W3C ARIA examples](https://github.com/w3c/aria-practices) | custom select-only combobox: open it, pick an option |
| apg-datepicker | W3C ARIA examples | date picker in a modal dialog, keyboard navigation |
| apg-tabs | W3C ARIA examples | tabs with automatic activation, arrow keys |
| internet-login | [the-internet.herokuapp.com](https://the-internet.herokuapp.com) | form login and logout (demo credentials are printed on the page) |
| internet-dynamic-loading | the-internet | content that appears 5 s after a click |
| internet-errors | the-internet | an uncaught exception on load, then a 500 response |
| todomvc-filters | [demo.playwright.dev/todomvc](https://demo.playwright.dev/todomvc/) | single-page app: add and complete todos, switch hash-routed filters |

The W3C examples are served locally by `bench.mjs` from W3C's own repository (commit `0f765e4`). w3.org answers headless browsers with a Cloudflare check page, and local copies make the runs repeatable anyway.

## Results

| scenario | tool | pass | calls | tokens (est) | time | stale views | errors surfaced |
|---|---|---|---|---|---|---|---|
| apg-combobox | kite | 3/3 | 4 | 2,342 | 0.4 s | – | 0 |
| | devtools | **0/3** | 4 | 12,108 | 0.6 s | – | 0 |
| apg-datepicker | kite | 3/3 | 7 | 3,143 | 1.1 s | – | 0 |
| | devtools | 3/3 | 8 | 81,403 | 2.0 s | – | 0 |
| apg-tabs | kite | 3/3 | 5 | 1,536 | 0.5 s | – | 0 |
| | devtools | 3/3 | 6 | 24,913 | 0.9 s | – | 0 |
| internet-login | kite | 3/3 | 9 | 1,071 | 2.7 s | 0/2 | 0 |
| | devtools | 3/3 | 10 | 2,076 | 3.8 s | 0/2 | 0 |
| internet-dynamic-loading | kite | 3/3 | 3 | 238 | 6.6 s | – | 0 |
| | devtools | 3/3 | 4 | 713 | 6.8 s | – | 0 |
| internet-errors | kite | 3/3 | 5 | 876 | 1.9 s | 0/1 | 3 |
| | devtools | 3/3 | 8 | 1,305 | 2.5 s | 0/1 | 3 |
| todomvc-filters | kite | 3/3 | 14 | 595 | 0.9 s | 0/3 | 0 |
| | devtools | 3/3 | 15 | 2,888 | 2.3 s | 0/3 | 0 |

**Totals:** kite used 47 calls, about 9.8k tokens and 14 s. chrome-devtools-mcp used 55 calls, about 125k tokens and 19 s. devtools' numbers for apg-combobox cover only the steps it got through.

## What the numbers mean

- **Tokens.** kite used 4–67% of devtools' tokens per scenario, and 8% overall.
  - After each action, devtools sends a full snapshot. On the W3C pages, which have long documentation around each widget, that is 16–20 KB per action.
  - kite sends only what changed.
- **Combobox.** devtools failed every run of apg-combobox. After the combobox opened, its snapshot showed `combobox "Favorite Fruit" expanded` but not the listbox or its options, even after taking a new snapshot. So an agent can't click "Huckleberry" by reference and has to fall back to the keyboard. kite listed all the options in the click's diff.
- **Time.** kite was as fast or faster in every scenario. It waits for the page to settle before answering, so it needs no extra checks. devtools returns sooner but needs extra calls (`wait_for`, `list_pages`) to see the result.
  - On dynamic loading, both spent most of the 6–7 s waiting for the page's own 5 s delay.
- **Stale views.** On these sites, neither tool returned a stale page after an action.
  - Earlier runs on a single-page app with slow server data showed devtools' `click` answering before navigation finished.
  - The scenarios still measure stale views, so a regression or a slower site shows up.
- **Errors.** Both tools caught the uncaught exception and the 500 response.
  - kite reported them in the action's own response.
  - devtools reported them after two extra calls: `list_console_messages` and `list_network_requests`.
  - the-internet's analytics beacon fails on every page. The `noise` field in its scenarios keeps that out of both tools' counts.

## kite over MCP

`--mcp` adds `kite mcp` as a third driver. It returned the same output as the CLI in every scenario, so calls and tokens were identical. Times were within ±25% of the CLI and differed in both directions. The difference is in what each tool costs before any page is opened:

| | tools | definitions loaded per session |
|---|---|---|
| kite mcp | 2 | ~500 tokens |
| chrome-devtools-mcp 1.12 | 32 | ~6,900 tokens |

## Fairness notes

The devtools driver follows that tool's best practices:
- `includeSnapshot` on actions, instead of a separate snapshot call;
- `includePreservedMessages` and `includePreservedRequests`, with requests filtered to document, xhr and fetch;
- URL checks retried, because its `click` can return before a single-page-app navigation finishes;
- a new snapshot taken whenever an element isn't in the last one.

Each tool's server or daemon startup is excluded from the timings. Token counts are estimates, at 4 bytes per token.
