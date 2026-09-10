# Obscura CLI release build verification

This record binds the CLI build result to the integration candidate.

| Field | Value |
| --- | --- |
| Working tree | `/Volumes/extension/code/obscura/playground/m1-obscura-integration-c12-20260910` |
| Source commit | `c527ec91575fe8c230b73127b77600d057ccc18f` |
| Source tree | `3968496ff2f72abbef7b09a84b9c744fe59e079d` |
| Command exit status | `0` |
| Build output | `target/release/obscura` |
| Build output SHA-256 | `aa8249085c95cd93efd51688a8bc9a82f22e14df1fecbfb59af98511d63ee430` |

Exact command:

```sh
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 \
  cargo build --release -p obscura-cli --bins --features render
```

The release profile completed successfully. Compiler warnings were emitted by
unchanged vendored/render sources and did not change the zero exit status.
