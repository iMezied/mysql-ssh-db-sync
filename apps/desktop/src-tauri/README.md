# Tauri configuration

## Why there are two config files

`tauri.conf.json` is the base. `tauri.bundle.conf.json` is an overlay applied
only when building a distributable:

```bash
npm run bundle            # stages the CLI, then builds with the overlay
```

The overlay contains one thing: `externalBin`, which ships the `dbsync`
command-line tool inside the app bundle.

It has to be separate because **`externalBin` is validated by `tauri-build`**,
which runs on every `cargo build`, `cargo test` and `cargo clippy` of this
crate. With it in the base config, all of those fail with

```
resource path `binaries/dbsync-<triple>` doesn't exist
```

until someone has run `npm run bundle:cli` first. That breaks the ordinary
developer loop and two CI jobs that have no reason to compile the CLI at all.
The same is true of `bundle.resources`; it is not specific to `externalBin`.

Keeping it in an overlay means:

* `cargo build` / `cargo test` / `cargo clippy` need no staging step,
* `npm run bundle` is a single command that cannot forget one.

Use `npm run bundle`, not `npm run tauri build`, to produce a release: the
latter builds a working app but without the CLI inside it.

## Files

| File | Purpose |
|---|---|
| `tauri.conf.json` | Base configuration: window, CSP, bundle metadata, per-platform settings |
| `tauri.bundle.conf.json` | Overlay adding the bundled CLI. Packaging only |
| `entitlements.plist` | macOS entitlements — deliberately empty, with the reasoning recorded inside |
| `icons/source/` | Vector masters. `icons/` is generated from them; see the comments in each SVG |
| `binaries/` | Staged CLI. Build output, git-ignored |
