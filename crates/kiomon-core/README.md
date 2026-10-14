# kiomon-core

[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](../../LICENSE)

The storage, retrieval and lifecycle engine behind the [`kiomon`](../kiomon-cli)
CLI: a fast, local, SQLite-backed memory for coding agents.

This crate is the library. It does not print, does not exit, and does not know that
a terminal exists — `kiomon-cli` owns stdout, stderr and the exit code, which is
what lets an MCP server or a daemon reuse the engine without a refactor.

```rust,ignore
// The intended API. WP1 lands `Database`, `NewMemory` and `search`.
use kiomon_core::{Database, MemoryKind, NewMemory};

let db = Database::open_default()?;
let saved = db.add(NewMemory::new(MemoryKind::Semantic, "Deploys need migrations")
    .body("Run migrations before the deploy; the pipeline does not do it."))?;
let hits = db.search("why did the deploy fail")?;
# Ok::<(), kiomon_core::Error>(())
```

The design — the scope cut, the schema, the retrieval ladder, the decay formula and
the performance budget — is argued in
[`plans/001-local-v1-core.md`](../../plans/001-local-v1-core.md).

## Licence

Apache-2.0. The hosted Kiomon product is separate and is not covered by it.
