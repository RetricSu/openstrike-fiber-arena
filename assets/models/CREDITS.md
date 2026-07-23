# Asset credits

## `duelist.glb`

Original low-poly neon duelist: skeleton, mesh, palette texture, and the
Idle/Walk/Run/Death animation clips are generated procedurally by
`src/bin/build-duelist.rs` in this repository. No third-party art is used.

Regenerate after editing the generator:

```sh
cargo run --features openstrike --bin build-duelist
```

The `vendor/` tree is a pinned upstream submodule and is unaffected; the
Mixamo "Vanguard" soldier under `vendor/open-strike/assets/` is no longer
loaded by default (see its own `CREDITS.md`), but can still be selected with
`--soldier-model`.
