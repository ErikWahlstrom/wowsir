# Changelog

## [0.6.0](https://github.com/ErikWahlstrom/wowsir/compare/v0.5.0...v0.6.0) (2026-11-25)


### Features

* Linux arm64 — find Playwright's arm64 browsers, publish an arm64 binary ([30600b8](https://github.com/ErikWahlstrom/wowsir/commit/30600b83f60a59705e634a375ff81e5eb173d5db))
* Linux arm64 support (Playwright arm64 browsers, arm64 release binary) ([#22](https://github.com/ErikWahlstrom/wowsir/issues/22)) ([30600b8](https://github.com/ErikWahlstrom/wowsir/commit/30600b83f60a59705e634a375ff81e5eb173d5db))

## [0.5.0](https://github.com/ErikWahlstrom/wowsir/compare/v0.4.1...v0.5.0) (2026-11-25)


### Features

* run without a daemon in sandboxes, find PLAYWRIGHT_BROWSERS_PATH, explain Chrome start failures ([#20](https://github.com/ErikWahlstrom/wowsir/issues/20)) ([4bfe62a](https://github.com/ErikWahlstrom/wowsir/commit/4bfe62a8b8620cc895dbc154779024acd2d0fe5f))

## [0.4.1](https://github.com/ErikWahlstrom/wowsir/compare/v0.4.0...v0.4.1) (2026-11-20)


### Bug Fixes

* sticky headers in snap --viewport, unquoted selectors in export ([#18](https://github.com/ErikWahlstrom/wowsir/issues/18)) ([6d38c43](https://github.com/ErikWahlstrom/wowsir/commit/6d38c43720c629b310f0aae39cd1772f719c890c))

## [0.4.0](https://github.com/ErikWahlstrom/wowsir/compare/v0.3.0...v0.4.0) (2026-11-20)


### Features

* fold similar elements, read unquoted selectors, report cached pages ([#16](https://github.com/ErikWahlstrom/wowsir/issues/16)) ([1291fe8](https://github.com/ErikWahlstrom/wowsir/commit/1291fe82da0aa95c1b6885af353c0a9eab87283f))

## [0.3.0](https://github.com/ErikWahlstrom/wowsir/compare/v0.2.1...v0.3.0) (2026-11-15)


### Features

* `locator T` prints a stable target for an element and its Playwright locator ([e291843](https://github.com/ErikWahlstrom/wowsir/commit/e291843940bf368c696c75597edae04b56770009))
* export tests and recordings as Playwright tests ([#14](https://github.com/ErikWahlstrom/wowsir/issues/14)) ([e291843](https://github.com/ErikWahlstrom/wowsir/commit/e291843940bf368c696c75597edae04b56770009))
* warn when a target matches several elements ([#13](https://github.com/ErikWahlstrom/wowsir/issues/13)) ([8c6a154](https://github.com/ErikWahlstrom/wowsir/commit/8c6a154252d3e28ff9bf9b8a513ef9d2d085708d))

## [0.2.1](https://github.com/ErikWahlstrom/wowsir/compare/v0.2.0...v0.2.1) (2026-11-13)


### Bug Fixes

* show &lt;dialog&gt;s opened by CSS (daisyUI .modal-open) in the outline ([#8](https://github.com/ErikWahlstrom/wowsir/issues/8)) ([77db2aa](https://github.com/ErikWahlstrom/wowsir/commit/77db2aa08ebdf4b93b5526735cb2908adab80a1f))

## [0.2.0](https://github.com/ErikWahlstrom/wowsir/releases/tag/v0.2.0) (2026-11-12)

### Features

* Claude Code plugin: `/plugin install wowsir --marketplace ErikWahlstrom/wowsir`
* Stable refs across re-renders, `click --at X,Y` and `tap`, method-specific mocks (`mock POST …`), `net --request`
* Read-only mode: `readonly on|strict|off|allow`, popups guarded from their first request, WebSocket messages and action GETs blocked, `~/.wowsir/readonly` site list with `lock`

### Bug Fixes

* Quoted attribute selectors keep their quotes over MCP and in `.wowsir` files
* Mocked responses the page never reads no longer delay the action by 2.5 s

## [0.1.1](https://github.com/ErikWahlstrom/wowsir/releases/tag/v0.1.1) (2026-11-11)

### Bug Fixes

* A click that closes its own page (a popup's "Approve") no longer hangs for 30 s

## [0.1.0](https://github.com/ErikWahlstrom/wowsir/releases/tag/v0.1.0) (2026-11-11)

First public release.
