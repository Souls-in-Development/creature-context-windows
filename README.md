# Creature Context for Windows

Creature Context is a local-first repository-context engine with a deterministic
Rust core and a Windows Phi Silica integration surface.

The Rust adapter and C++/WinRT bridge are present, but the native bridge has not
been compiled or executed on Windows hardware. A Windows App SDK project and
live Copilot+ PC verification are still required before the model path can be
called verified. Metadata projection is unavailable; the Windows service adapter
is implemented but target verification remains outstanding.

## Development checks

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
```

See [platform/windows/README.md](platform/windows/README.md) and
[docs/platform-matrix.md](docs/platform-matrix.md) for the exact boundary.
