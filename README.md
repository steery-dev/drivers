# Steery drivers

A driver connects [Steery](https://steery.dev) to one harness: an agent product a person
runs conversations in. The Steery runtime starts a driver as a child process and speaks the
program protocol to it, JSON-RPC 2.0 over standard input and output; the driver reads the
harness's conversations, plans what setting it up writes, and says how to start, read and
resume a turn. It decides nothing and writes nothing of its own.

Each driver is its own program, built on the `steery-sdk` crate.

| Driver | Harness | Levels |
|---|---|---|
| [`omp`](omp/) | omp | observe, drive; live through Steery's plugin door |

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
