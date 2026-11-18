# proof-load-desktop

The Proof Load desktop app is a window for [Proof Load](../../crates/proof-load/README.md),
Forklift's fault-injection harness. It only displays things. The runs, the grades and the report
all come from the `proof-load` library, and its look follows
[`docs/proof-load-design-system.html`](../../docs/proof-load-design-system.html).

It has three screens:

- **Results**: how many rows got each grade, and the rows themselves, worst grade first. The rows
  can be filtered to failures only, or to one binding. A row opens its steps: each command run
  after the fault, what the command printed, and how it ended.
- **Coverage**: the fault class × platform table, with each cell run, sampled, or not run and
  the reason.
- **Run**: a form for the binaries under test, the source tree, the report folder and an optional
  scenario. It starts a run and shows its progress.

For what the grades and the table mean, see [`docs/guide/proof-load.md`](../../docs/guide/proof-load.md).

## Build and open a report

The app is a Cargo workspace of its own, with its own `Cargo.lock`, so egui is never built by the
rest of the repository. From the repository root:

```sh
cargo build --manifest-path tools/proof-load-desktop/Cargo.toml
tools/proof-load-desktop/target/debug/proof-load-desktop <report folder>
```

`<report folder>` is the `--out` folder of a `proof-load run`, the one holding `report.json`. If
the app cannot read that folder's report, it exits with status 2 and opens no window. This also
happens when the report was written in a format this build does not read. Without a folder, the
window opens on the Run screen.

`cargo run --manifest-path tools/proof-load-desktop/Cargo.toml -- <report folder>` builds and opens
the app in one step.

## Platforms

The app builds and runs on Linux, macOS and Windows. The `proof load desktop` CI workflow builds
it, tests it and runs Clippy on all three. On Linux, eframe's windowing and OpenGL backends need
the X11, Wayland, xkbcommon and GL development libraries; the workflow lists the packages.

The tests run without opening a window. They hold every colour and shape token in the design
system page to `src/theme.rs`, and they fail on a colour or font-size literal anywhere else in
`src/`.
