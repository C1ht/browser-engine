# bolt

A small browser engine written from scratch in Rust, built for speed.

bolt parses HTML and CSS, runs the cascade, lays out the page and rasterizes it
in software. The only dependencies are `minifb` (opens a window and blits a
pixel buffer) and `fontdue` (turns glyph outlines into coverage bitmaps).
The engine itself has no dependencies: tokenizer, DOM, CSS parser, selector
matching, cascade, layout, display list, rasterizer and PNG encoder are all in
this repo (~6.5k lines).

## Usage

```sh
cargo build --release

bolt                                   # opens the built-in home page
bolt https://news.ycombinator.com      # URL (http/https are fetched with the system curl)
bolt path/to/page.html                 # local file
bolt about:stress                      # 1 MB synthetic page with ~45k nodes

bolt --bench about:stress --iters 30   # time each pipeline phase (median / min)
bolt --shot out.png <url> [--width 1024] [--height 0 = full page]
bolt --dump <url>                      # print the DOM tree
bolt --boxes <url>                     # print the layout box tree
```

Keys in the window: `Ctrl+L`/`F6` or clicking the bar edits the address,
wheel / arrows / `PgUp` / `PgDn` / `Space` / `Home` / `End` scroll,
`Backspace` or `Alt+←` goes back, `Alt+→` goes forward, `F5` reloads,
`F12` prints timings.

## Pipeline

| Stage | File | What makes it fast |
|---|---|---|
| Fetch | `net.rs` | Stylesheets are fetched in parallel. |
| Parse | `html.rs` | A single pass over bytes. A word-at-a-time (SWAR) `memchr` scans for `<` and `&`. Nodes go into an arena with `u32` links, and tag/attribute/class names are interned to integer atoms (`atom.rs`, FxHash). |
| CSS | `css.rs` | Values are fully typed at parse time and shorthands are expanded, so the cascade never re-parses strings (except declarations that use `var()`). |
| Style | `style.rs` | Rules are bucketed by id / class / tag. A 256-bit ancestor bloom filter rejects descendant selectors without walking up the tree. A parent always has a lower arena index than its children, so styles are computed in one linear loop with no recursion. |
| Layout | `layout.rs` | Positions are relative to the parent's content box, so moving a subtree is O(1). Intrinsic sizes are cached per box. Glyph advances live in lock-free atomic tables. |
| Paint | `paint.rs` | A flat display list is culled to the viewport. Glyphs are rasterized once and cached. The frame is split into horizontal bands painted in parallel without locks. Scrolling only repaints and never re-runs layout. |

## Performance

Measured with `bolt --bench` at 1024px width on a 16-thread desktop; each figure is the median of 30 warm runs:

| Page | Nodes | Parse | Style | Layout | Display list | Paint (frame) | Total |
|---|---|---|---|---|---|---|---|
| `about:stress` (1 MB HTML) | 45,270 | 5.7 ms | 8.6 ms | 13.0 ms | 2.2 ms | 1.0 ms | **31 ms** |
| news.ycombinator.com | 1,294 | 0.6 ms | 0.4 ms | 0.4 ms | 0.03 ms | 1.6 ms | **4 ms** |

The first page you load also pays one-time font parsing, about 25–130 ms per face. The common faces are pre-loaded on background threads at startup to hide most of that cost.

## Supported

- HTML: implicit `html`/`head`/`body`, auto-closing `p`/`li`/`dt`/`dd`/`tr`/`td`/`option`, void and raw-text elements, character references, quirks mode for pages without a doctype, and form controls drawn as boxes.
- Selectors: type, `.class`, `#id`, `*`, attribute selectors (all operators, with `i`), all four combinators, `:first-child`, `:last-child`, `:only-child`, `:first-of-type`, `:last-of-type`, `:nth-child()`, `:nth-last-child()`, `:not()`, `:is()`, `:where()`, `:root`, `:empty` and `:link`.
- Cascade: origin, specificity, `!important`, inline `style`, presentational attributes (`bgcolor`, `width`, `align`, `<font>`, `cellpadding`, …), inheritance, `inherit` / `initial` / `unset`, `@media` (width/height ranges, `prefers-*`) and `@supports`.
- Values: `px em rem % pt vw vh ex ch`, named / hex / `rgb()` / `hsl()` colors, `calc()`, custom properties with `var()` and fallbacks.
- Layout: block flow with margin collapsing (sibling, parent–child, through empty blocks), `width` / `min-width` / `max-width` / `height`, `box-sizing`, auto-margin centering, `<center>` / `-webkit-center`, inline formatting with line breaking, `white-space`, `text-align`, `text-transform`, inline-block, an automatic table layout algorithm (`colspan`, percentage columns, `border-spacing`, `vertical-align` in cells) and list markers (disc, circle, square, decimal, alpha, roman).
- Paint: backgrounds, borders, text with underline and line-through, inline backgrounds, and fallback fonts for symbols and CJK.
- Browser: links, hover status bar, back/forward history, `#fragment` scrolling, address bar, window resize.

## Not supported (yet)

JavaScript, image decoding (images draw as placeholders or show their alt text), floats, flexbox and grid (these lay out as plain blocks), positioned elements (`absolute`/`fixed` are hidden), `min()` / `max()` / `clamp()`, transforms, shadows, rounded corners, web fonts, form interaction, and text that is right-to-left or needs complex shaping.

## Tests

```sh
cargo test --release
```
