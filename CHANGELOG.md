# Changelog

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
