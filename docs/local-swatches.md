# Local Swatches development

Normal Tablero builds consume [`swatches` 0.1.0](https://crates.io/crates/swatches/0.1.0)
from crates.io. `Cargo.lock` pins that registry crate; a clean checkout does not
need a Swatches git clone, a path dependency, or a patch script.

To try an unpublished Swatches checkout without changing Tablero's committed
manifest, add a workspace `[patch.crates-io]` in a **local-only** override
(for example `~/.cargo/config.toml`, or an uncommitted edit you do not push):

```toml
[patch.crates-io]
swatches = { path = "/absolute/path/to/swatches" }
```

Alternatively, `cargo add swatches --path /absolute/path/to/swatches` in a
throwaway branch. Do not commit path or git dependencies; published and CI
builds must keep `swatches = "0.1.0"`.
