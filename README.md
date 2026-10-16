# bolt

A browser engine written from scratch in Rust, built for speed.

bolt parses HTML and CSS, runs the cascade, lays out pages (block, inline, floats, positioning, flexbox, grid and tables), decodes images, loads web fonts, runs JavaScript, and rasterizes everything in software. It has two dependencies: `minifb` opens a window and blits pixels, and `fontdue` turns glyph outlines into bitmaps. Everything else is in this repo, including the HTML parser, CSS engine, layout, the PNG, JPEG, GIF, WebP and AVIF decoders (including a complete AV1 intra-frame decoder), the SVG renderer, the inflate (zlib) and Brotli decompressors, the JavaScript interpreter and its regex engine, and the rasterizer.

## Usage

```sh
cargo build --release

bolt                                   # opens the built-in home page
bolt https://news.ycombinator.com      # URL (http/https are fetched with the system curl)
bolt path/to/page.html                 # local file
bolt about:features                    # flexbox, grid, floats, positioning, effects
bolt about:js                          # JavaScript demo (counter, to-dos, timers)
bolt about:stress                      # 1 MB synthetic page with ~45k nodes

bolt --bench about:stress --iters 30   # time each pipeline phase (median / min)
bolt --shot out.png <url> [--width 1024] [--height 0 = full page] [--type "text" = type into the first text field]
bolt --js script.js                    # run a JavaScript file in the interpreter
bolt --dump <url>                      # print the DOM tree
bolt --boxes <url>                     # print the layout box tree
```

Keys in the window:

| Key | Action |
|---|---|
| `Ctrl+L` / `F6`, or click the bar | Edit the address |
| Wheel, arrows, `PgUp` / `PgDn` / `Space`, `Home` / `End` | Scroll |
| `Backspace` / `Alt+←` | Back |
| `Alt+→` | Forward |
| `F5` | Reload |
| `F12` | Print timings |
| `Tab` / `Esc` (in a form field) | Next field / leave the field |
| `Ctrl+V` / `Ctrl+Backspace` (in a form field) | Paste / delete a word |

## Pipeline

| Stage | Code | What makes it fast |
|---|---|---|
| Fetch | `net.rs` | Stylesheets and scripts download in one batch, using a single parallel `curl` process that reuses connections. |
| Parse | `html.rs` | A single pass over bytes. A word-at-a-time (SWAR) `memchr` scans for `<` and `&`. Nodes go into an arena with `u32` links, and names are interned to integer atoms. |
| CSS | `css.rs` | Values are typed at parse time and shorthands expanded. `calc()`, `min()`, `max()`, `clamp()`, `var()` and gradients are pre-parsed. |
| Style | `style.rs` | Rules are bucketed by id / class / tag. A 512-bit ancestor bloom filter rejects descendant selectors early. Per-rule flags skip work, and the results of `var()` substitution are cached. |
| Layout | `layout.rs` | Positions are relative to the parent, so moving a subtree is O(1). Intrinsic sizes are cached. Glyph advances live in lock-free atomic tables. |
| Images | `image.rs` | Decoders are written from scratch. Images get mipmaps for smooth downscaling and decode on all cores. |
| Paint | `paint.rs` | A flat display list is culled to the viewport, and banded rasterization runs on all cores without locks. Scrolling only repaints and never re-runs layout. |
| Script | `js/` | A tree-walking interpreter with a step and wall-clock budget, so a page can't hang the browser. |

## Performance

Measured with `bolt --bench` at 1024 px on a 16-thread desktop; each figure is the median of warm runs:

| Page | Nodes | Parse | Style | Layout | Display list | Paint (frame) | Total |
|---|---|---|---|---|---|---|---|
| `about:stress` (1 MB HTML) | 45,270 | 3.6 ms | 9.9 ms | 13.0 ms | 3.4 ms | 0.8 ms | **31 ms** |
| news.ycombinator.com | 1,294 | 0.6 ms | 0.4 ms | 0.4 ms | 0.03 ms | 1.6 ms | **4 ms** |

On real sites, total load time is dominated by the network, not the engine.

## Supported

- **HTML:** implicit `html`/`head`/`body`, auto-closing tags, raw-text elements, character references and quirks mode.
- **Selectors:** type, class, id, attribute selectors, all combinators, `:nth-child()`, `:not()`, `:is()`, `:where()`, `:first-of-type`, `:focus`, `:checked`, `:placeholder-shown`, `:disabled` and more.
- **Cascade:**
  - Specificity, `!important` and presentational attributes.
  - `@media`, `@supports`, `@font-face`.
  - Custom properties with `var()`.
  - `calc()`, `min()`, `max()`, `clamp()`.
- **Layout:**
  - Block flow with margin collapsing, inline formatting with line breaking, and `inline-block`.
  - Floats and `clear`, wrapping text around floats.
  - `position: relative / absolute / fixed / sticky` with `z-index`.
  - Flexbox: direction, wrap, grow / shrink / basis, `justify-content`, `align-*`, `gap`, `order`, auto margins.
  - Grid: `repeat()` (including auto-fill), `minmax`, `fr`, line and span placement, `grid-template-areas`.
  - Tables (automatic layout), lists, `overflow: hidden`.
- **Paint:** backgrounds, borders, rounded corners, box shadows, opacity, linear / radial / repeating gradients, background images (size / position / repeat) and `object-fit`.
- **Images:**
  - PNG (all color types and depths, interlaced), baseline and progressive JPEG, GIF (first frame).
  - WebP: lossy, lossless, alpha and animations (first frame), matching libwebp pixel for pixel.
  - AVIF: 8/10/12-bit, 4:2:0/4:2:2/4:4:4 and monochrome, alpha, grids, rotation and mirroring. The AV1 decoder covers every intra-frame tool (palette, intra block copy, filter intra, CfL, all transforms, deblocking, CDEF, loop restoration, film grain) and is bit-exact with dav1d.
  - SVG in `<img>`, CSS backgrounds and inline `<svg>` (with `currentColor` from CSS): paths, shapes, transforms, `<use>`/`<symbol>`, gradients, clip paths, strokes with dashes, basic text, `<style>` sheets.
  - `data:` URLs (base64 and percent-encoded).
- **Fonts:** `@font-face` with TTF, OTF, WOFF and WOFF2 (Brotli plus the `glyf` / `loca` / `hmtx` transforms), matched by weight, style and `unicode-range`; symbol and CJK fallback fonts.
- **JavaScript:**
  - ES2015+ syntax: classes with private names, destructuring, spread, arrow functions, template literals, optional chaining, `async` / `await`, generators and async generators.
  - Built-ins: Promises, Map / Set, JSON, RegExp, Date, `ArrayBuffer`, typed arrays, `DataView`, `TextEncoder` / `TextDecoder`, `crypto.getRandomValues`.
  - DOM: `querySelector`, `innerHTML`, `classList`, `style`, events with bubbling, timers, `TreeWalker` / `NodeIterator`, `<template>.content`.
  - Network: `fetch` (Request, Response, Headers, `AbortController`) and `XMLHttpRequest`, both asynchronous on background threads; `URL`, `URLSearchParams`, `FormData`, `Blob`.
- **Identity:** bolt sends a Chrome user agent and Chrome's `Accept` headers, so sites serve their modern markup.
- **Forms:** typing into inputs and textareas with a caret, checkboxes, radio buttons, labels, `<select>` (a click cycles through the options), and GET / POST submission, including Enter in a field. Scripts see `keydown`, `input`, `change`, `focus`, `blur` and `submit` events, and can call `focus()`, `form.submit()` and `requestSubmit()`.
- **Browser:** links, back / forward history, `#fragment` scrolling, address bar (plain words search DuckDuckGo), resizing, clicks dispatched to scripts, script navigation via `location`, and cookies kept between runs.

## Not supported (yet)

- Animated images beyond the first frame, AVIF superres, and SVG filters, masks, patterns and markers. A text run uses one font face, so `unicode-range` subsets other than Latin are skipped.
- ES modules, custom elements, shadow DOM, WebSocket and `<canvas>`; text selection and drop-down menus for `<select>`. Generators run their body eagerly on the first `next()`, so an infinite generator stops after 100,000 values.
- Transforms, filters, animations / transitions and right-to-left text.
- YouTube: its content is built from custom elements, so only the loading skeleton renders.
- Google search results: Google only serves them after a JavaScript anti-bot challenge that bolt can't pass yet. Forms on Google (search box, cookie consent) work.

## Tests

```sh
cargo test --release
```

The image decoders are checked against Pillow-decoded fixtures in `testdata/`: WebP pixel-exact against libwebp, and AVIF planes bit-exact against dav1d (`testdata/avif`, plus `BOLT_AVIF_DIR=<folder of x.avif + x.yuv>` for larger sets such as the link-u samples). The Brotli decoder against real CDN responses. The WOFF2 test optionally compares every ASCII glyph with the same font as plain TTF (`BOLT_REF_TTF=<folder with lobster.ttf, roboto-bold.ttf>`). JavaScript semantics, the DOM bindings and end-to-end clicks have unit tests in `src/js` and `src/engine.rs`.
