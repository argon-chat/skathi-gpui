# skathi-gpui

[gpui](https://github.com/zed-industries/zed/tree/main/crates/gpui), the GPU-accelerated UI framework
of the [Zed](https://zed.dev) editor, as [Skathi](https://github.com/argon-chat/skathi) renders with it.

This is not a new UI framework, and not a general-purpose fork. It is gpui and the crates it needs,
cut out of [zed-industries/zed](https://github.com/zed-industries/zed) with `git filter-repo` so they
build as a workspace of their own, plus the renderer work Skathi depends on.

**Issues and pull requests are off here.** Report problems at
[argon-chat/skathi](https://github.com/argon-chat/skathi/issues). Report a gpui bug that Skathi does not
cause to Zed.

## What is in it

| Crate                                                     | Role                                    |
| --------------------------------------------------------- | --------------------------------------- |
| `gpui`                                                    | elements, layout, windows, scene, text  |
| `gpui_macros`                                             | derive and attribute macros             |
| `gpui_platform`                                           | picks the platform crate for the target |
| `gpui_windows`                                            | Win32 and DirectX                       |
| `gpui_macos`, `gpui_ios`, `gpui_apple`                    | AppKit, UIKit and Metal                 |
| `gpui_linux`, `gpui_wgpu`                                 | Wayland and X11, the wgpu renderer      |
| `gpui_web`                                                | the browser, through wgpu               |
| `gpui_util`, `gpui_shared_string`                         | small shared helpers                    |
| `refineable`, `derive_refineable`                         | style refinement                        |
| `collections`, `sum_tree`, `scheduler`                    | data structures and the executor        |
| `http_client`, `http_client_tls`, `reqwest_client`        | the HTTP client gpui loads images with  |
| `ztracing`, `ztracing_macro`, `zlog`                      | tracing and logging                     |
| `bench_metrics`, `util_macros`, `tooling/perf`            | benchmarks and test support             |

On crates.io each crate is named `skathi-gpui-*` after its directory (`gpui` is `skathi-gpui`,
`gpui_windows` is `skathi-gpui-windows`, `refineable` is `skathi-gpui-refineable`), with its library
name unchanged, so code still says `use gpui::…`. `http_client_tls`, `reqwest_client`, `util_macros`
and `tooling/perf` are test support and not published.

`assets/fonts` holds the two font families that gpui's tests and examples load. Apps do not embed
them.

Paths and history are Zed's: `crates/gpui` here is `crates/gpui` there, and every commit that touched
these paths upstream is in this history, with the same author and date.

## What Skathi adds

These changes come on top of upstream gpui. They are copyright Argon Inc. LLC, under the same Apache
License 2.0 as gpui (see `NOTICE`). Each one is marked `vue-native fork` in the source:

- **Compositing.** Isolated layers on every renderer: opacity groups, `clip-path`, CSS masks, blend
  modes, backdrop blur per element, element captures and snapshot composites. Deferred draws stay
  inside their effect and clip scopes.
- **Shader effects.** Effect quads and effect layers that shade an element's content, with
  per-element state, history, image textures, animated parameters and cross-fades. Apps supply their
  own effects, and a running window reloads them.
- **CSS painting.** Transforms on every primitive, content mask and hit test, with
  `transform-origin`. The whole gradient grammar and background layers, Oklab interpolation,
  per-side border colours, outlines, `corner-shape`, percentage and elliptical radii, rounded
  overflow clips, `text-shadow`, `object-position`, SVG colour matrices.
- **Layout.** Arbitrary grid tracks and `repeat(auto-fill | auto-fit)`, `calc()` and `calc-size()`
  lengths, `order`, size containment, baselines and baseline shifts, inline boxes, the same pixel
  snapping for cached and inline frames at fractional scales. It runs on Skathi's fork of taffy.
- **Text.** A font size per run, families composed by `unicode-range`, optical sizes of variable
  fonts, `line-height: normal` from the font's metrics, line breaking under `overflow-wrap`,
  `word-break` and `hyphens`, `tab-size`, letter spacing. Glyph phases that depend on the size,
  re-rasterizing when a transform settles, font smoothing per element, glyph contrast from the text
  colour.
- **Windows.** A DirectX 12 renderer beside DirectX 11, with recovery from a lost device. DWM
  materials, frame pacing on the display's vblank, feedback on when a present reached the screen,
  an idle window that waits for demand.
- **macOS.** Metal parity for layers, effects and transforms. EDR output, display-link pacing with
  a fallback, blur kept in fullscreen, native window controls, panels over fullscreen apps.
- **Linux and the web.** The wgpu renderer draws the same layers, effects and transforms.
- **HDR.** Windows draw HDR output on request, with half-float image tiles.
- **Input.** Pen input with hardware timestamps, pointer lock, pinch gestures, device haptics,
  native keyboard events, native drag and drop in both directions.
- **Embedding.** Hooks for input and frames, hitboxes by tag, GPU textures written into atlas
  tiles, a surface composited under the window, the renderer's device for the embedder, stats for
  the atlas and caches.
- **Build.** A default build without schemars, strum, derive_more, pin-project and rayon, and gpui
  off the `windows` crate. Each crate is published on its own.

The renderers embed Skathi's built-in effects from their own `effects/` directories, which Skathi's
`bun run shaders` writes and `bun run shaders:check` keeps in step with the `.slang` sources.
`SK_EFFECTS_GENERATED_DIR` points the build scripts at another directory.

## How it was made

From a checkout of Skathi's Zed fork:

```sh
git filter-repo --paths-from-file paths.txt
```

`paths.txt`:

```
crates/gpui/
crates/gpui_macros/
crates/gpui_platform/
crates/gpui_windows/
crates/gpui_macos/
crates/gpui_apple/
crates/gpui_ios/
crates/gpui_linux/
crates/gpui_wgpu/
crates/gpui_web/
crates/gpui_util/
crates/gpui_shared_string/
crates/refineable/
crates/collections/
crates/sum_tree/
crates/scheduler/
crates/http_client/
crates/http_client_tls/
crates/reqwest_client/
crates/ztracing/
crates/ztracing_macro/
crates/zlog/
crates/bench_metrics/
crates/util_macros/
tooling/perf/
assets/fonts/ibm-plex-sans/
assets/fonts/lilex/
LICENSE-APACHE
```

A workspace was then added on top: `Cargo.toml` with Zed's workspace settings and only the
dependencies these crates use, `Cargo.lock`, and this README.

`git filter-repo` is deterministic. The same command on upstream Zed gives the same commits, so
upstream changes merge in as ordinary commits:

```sh
git clone https://github.com/zed-industries/zed upstream-zed
cd upstream-zed && git filter-repo --paths-from-file ../paths.txt
cd ../skathi-gpui && git fetch ../upstream-zed main && git merge FETCH_HEAD
```

## Releases

The crates are published from this repository, on their own version line, not Skathi's. Every
published crate shares the workspace version, and a release publishes all of them. The version is
semver for this code: a breaking change to a public item of any crate bumps the minor version (0.4 to
0.5), anything else the patch version. Skathi requires a compatible version (`version = "0.4.0"`) next
to its path dependency on this checkout, and its release stops while that version is not on
crates.io, or while its checkout has changes made since that version was published.

To release:

1. Set `version` in `[workspace.package]` and every `version` in `[workspace.dependencies]` that
   points into `crates/`, and commit.
2. `bun scripts/publish-crates.ts --tag v<version> --dry-run` packages and verifies every crate.
3. Push the tag `v<version>`: `.github/workflows/release.yml` publishes the crates in dependency
   order and waits out crates.io's rate limit. A re-run goes on where the last one stopped.
4. In Skathi, raise the `version` of every `skathi-gpui*` dependency to this one.

`skathi-taffy` comes from crates.io at the version `taffy` names, published from
[argon-chat/taffy](https://github.com/argon-chat/taffy) (`cargo publish` on its `vue-native` branch)
before a release that needs a new one. A change to Skathi's built-in effects regenerates the
renderers' `effects/` directories here, so it needs a release of these crates too.

## License

Apache-2.0, as gpui is upstream ([LICENSE-APACHE](LICENSE-APACHE)). Only Zed's Apache-2.0 crates are
kept. Zed's GPL-licensed editor crates are not part of this history. The fonts carry their own
licenses: SIL Open Font License 1.1, in each font's folder.
