# Phase 1 toolchain

Versions were checked against official releases and registry metadata on November 23, 2026. Rust 1.99.1 is the patched release already available on the development host. Pinning it gives CI and contributors the same compiler. Node.js 24 is an LTS line and satisfies Vite 8.3's engine requirement.

| Component | Selected version | Source |
| --- | --- | --- |
| Rust and Cargo | Rust 1.99.1, edition 2024, workspace resolver 3 | [Rust 1.99.1](https://blog.rust-lang.org/releases/1.99.1/), [Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html), [Cargo resolver](https://doc.rust-lang.org/cargo/reference/resolver.html#resolver-versions) |
| Tauri | Rust runtime 2.12.7, build helper 2.7.6, npm CLI 2.12.6 | [Tauri ecosystem releases](https://v2.tauri.app/release/), [Vite integration](https://v2.tauri.app/start/frontend/vite/) |
| Node and Vite | Node 24 LTS, Vite 8.3.2, React plugin 6.1.1 | [Node.js LTS releases](https://nodejs.org/en/about/previous-releases), [Vite support policy](https://vite.dev/releases), [React plugin releases](https://github.com/vitejs/vite-plugin-react/releases) |
| React and TypeScript | React and React DOM 19.3.1, TypeScript 7.0.2 | [React versions](https://react.dev/versions), [TypeScript 7.0](https://devblogs.microsoft.com/typescript/announcing-typescript-7-0/) |
| Rust applications | Tokio 1.54.1, Clap 4.6.9 | [Tokio](https://docs.rs/tokio/1.54.1/tokio/), [Clap](https://docs.rs/clap/4.6.9/clap/) |
| Android tooling | Documented AGP, JDK, SDK, Gradle, Kotlin, and Compose baseline | [Platform constraints](platforms.md#android-tooling) |

`Cargo.lock` and `apps/desktop/package-lock.json` record exact resolutions. Tauri's native dependencies remain outside the CLI and daemon. The webview has one read-only daemon status command scoped to the local window. [Tauri capabilities](https://v2.tauri.app/security/capabilities/)

Phase 2B uses Snow for in-process peer cryptography. SQLite and Android bindings are planned. [Snow 0.10.0](https://docs.rs/snow/0.10.0/snow/)

Dependency candidates for storage and Android include [rusqlite 0.43.0](https://docs.rs/rusqlite/0.43.0/rusqlite/) and [UniFFI 0.32.2](https://docs.rs/uniffi/0.32.2/uniffi/).
