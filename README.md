# Steery drivers

A driver connects [Steery](https://steery.dev) to one harness: an agent product a person
runs conversations in. The Steery runtime starts a driver as a child process and speaks the
program protocol to it, JSON-RPC 2.0 over standard input and output; the driver reads the
harness's conversations, plans what setting it up writes, and says how to start, read and
resume a turn. It decides nothing and writes nothing of its own.

Each driver is its own program, built on the `steery-sdk` crate, and answers version 1 of
the program protocol (`specs/PROTOCOL.md` in Steery's repository).

| Driver | Harness | Levels |
|---|---|---|
| [`omp`](omp/) | omp | observe, drive, live (Steery's plugin door) |

The `omp` driver reads omp's conversations, their forks and their sub-agents, as protocol
events: turns and how they ended, words, calls grouped as Steery groups them, tokens per
response, models. It plans omp's setup (MCP entry, skill, plugin), lists omp's models
with their input cost, and plans turns and one-shot readers. omp's moments reach the
runtime over the plugin door, so the driver has no hook methods. The door's own methods
(`stop`, `receipt/*`, `say`, `pick`, `recap`) are the runtime's, so it has none of those
either.

`pi`, `codex` and `claude-code` follow.

## Building

The SDK and the protocol live in Steery's own repository until they are published. Check it
out beside this one, as `server`:

```sh
git clone https://github.com/steery-dev/server
git clone https://github.com/steery-dev/drivers
cd drivers/omp && cargo build
steery driver test target/debug/steery-driver-omp
```

## License

MIT, see [LICENSE](LICENSE).
