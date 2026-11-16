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

These changes come on top of upstream gpui. Each one is marked `vue-native fork` in the source:

- isolated layers: opacity groups, `clip-path`, CSS masks, blend modes;
- shader effects on elements, with per-element state, textures and animated parameters;
- deferred draws that stay inside their effect and clip scopes;
- a DirectX 12 path and DWM materials on Windows;
- text and input fixes found by Skathi's end-to-end tests.

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

## License

Apache-2.0, as gpui is upstream ([LICENSE-APACHE](LICENSE-APACHE)). Only Zed's Apache-2.0 crates are
kept. Zed's GPL-licensed editor crates are not part of this history. The fonts carry their own
licenses: SIL Open Font License 1.1, in each font's folder.
