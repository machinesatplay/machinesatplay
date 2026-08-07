# my game

this is a multiplayer Bevy game.

```sh
mach dev
```

the native client opens and connects to the local server.

- `crates/game-server/src/game.rs` contains the authoritative bevy server.
- `crates/game-client/` contains bevy client code.
- `crates/game-core/` contains shared physics, protocol, and world code.
- `assets/` contains game assets.

the client and server use ordinary Bevy plugins, systems, resources,
components, events, schedules, and Avian physics.

```sh
mach validate
mach doctor
mach dev --no-open
mach dev open
mach deploy
cargo fmt --check
```
