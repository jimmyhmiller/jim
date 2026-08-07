# Project instructions

## Restarting Jim

When the user asks to restart, relaunch, or load changes into the running Jim
application, always run:

```sh
./scripts/dev-restart.sh
```

Do not replace this with `cargo build`, `cargo run`, a direct binary launch,
or a manual process restart. Wait until the script prints that `Jim.app` was
launched through LaunchServices before reporting success. The script preserves
Jim's persistent terminal and bus daemons.
