# game agent guide

this is a multiplayer Bevy game with a browser client and an authoritative
native server.

## start here

- `src/main.rs` starts the application.
- `crates/game-server/src/game.rs` contains the authoritative server plugin and world state.
- `crates/game-client/src/` contains browser client code.
- `crates/game-core/src/` contains shared physics, protocol, and world code.
- `assets/` contains game assets.

server code has the full Bevy and Avian APIs. keep gameplay state authoritative
on the server and replicate the components that clients need to render or
predict.

use the cli for the local loop:

```sh
mach validate
mach dev --no-open
mach dev open
```

before handing work back, run `cargo fmt --check` and `mach validate`. do not
commit browser bundles, certificates, `target`, or `.mach` state.
