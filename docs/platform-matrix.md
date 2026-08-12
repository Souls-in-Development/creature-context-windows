# Windows platform matrix

| Capability | State | Evidence boundary |
| --- | --- | --- |
| Deterministic Rust core | CI-testable on Windows | Windows workflow runs the Rust suite |
| Rust Phi Silica adapter fallback | Host-testable | Reports unavailable without a linked bridge |
| C++/WinRT bridge | Written, unverified | Windows App SDK project and native build still required |
| Live Phi Silica inference | Unverified | Requires a supported Copilot+ PC |
| Filesystem watcher | Implemented, unverified | Requires target execution |
| Windows service adapter | Implemented, unverified | Requires target execution |
| Native metadata projection | Unavailable | No writer is implemented |
