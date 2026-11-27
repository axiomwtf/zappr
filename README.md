<p align="center">
  <img src="./zappr.svg" alt="zappr" width="320">
</p>

# zappr

**One language. One tool. One artifact per platform.**

Zappr is a compiled, fully native programming language. The tool is `zap`: one binary that is compiler, build driver, runner, package manager and formatter at once. There is no separate compiler entry point and no tooling split — `zap` is the only way in. Zappr yoinks the good parts of C, C++, TypeScript, Rust, Go and the rest, and leaves the bad parts. It compiles to Linux, Windows and the Web, all native. Native UI is built into the language, not bolted on. Configuration is one `Zappr.yaml`. Compile goal: under 5 seconds for a full build. Zappr is owned by AXIOM.

> **Status:** `0.1.0` (in development)

## Why

- **No sidecars.** One artifact per platform, nothing transient is ever written into a user project, no runtime files, no `.so` shims. If `zap` cannot produce one clean artifact, it fails with an error.
- **Native, not a wrapper.** Zappr drives its own codegen, its own linker configuration, its own everything. No gcc, no rustc, no chained or wrapped tooling.
- **Minimal configuration.** One `Zappr.yaml`, easy to read, edit and parse. Unknown keys are errors; defaults exist but are never silent.
- **Signed by default.** Every build artifact is Ed25519-signed; `zap verify` checks it.
- **Static linking.** Only base system libraries (libc, libm, libgcc and the loader) remain dynamic.
- **Blazing fast.** Under 5 seconds for a full build. The compiler is parallel, incremental and cache-first by design.

## Naming

| Name | Role |
|------|------|
| `Zappr` | The language |
| `zap` | CLI: compiler, driver, runner, package manager, formatter — one binary |
| `.znz` | Source file extension ("Zappr, not Zig") |
| `Zappr.yaml` | The one config file |
| `zapYaml` | The read-only namespace exposing `val:` entries in code |
| `use` | The namespace keyword for reaching other files and dependencies |

## Architecture

One Rust binary (edition 2024), permanently — Zappr is not self-hosted and is not intended to be.

```
zap (one Rust binary)
├── lexer          → tokens from .znz
├── parser         → AST
├── checker        → types, exclusivity, bounds, naming law, tidiness law, no-shadowing
├── call-collector → transitive closure of calls (the whole inclusion story)
├── codegen        → native codegen for linux / windows / wasm (no LLVM)
├── linker config  → static binaries, Ed25519 signing
└── ui runtime     → native renderer + DOM bridge for web
```

- **Parallel by module.** Independent `.znz` files compile concurrently, Go-style.
- **Incremental and content-hashed.** A touched file recompiles alone; caches live in the system temp dir, never in the project.
- **One visibility domain.** std, dependencies and app code get identical treatment. There is no "trusted std" tier — std is dependency #1.
- **Fast codegen, not LLVM.** Cranelift-tier codegen for v1; hand-tuned backends later. Fast compile wins over peak runtime perf.

## Language in one breath

Assignment is always marked: `!` declares, `/` reassigns, bare `=` is a hard error. The casing *is* the mutability marker — `camelCase` variables rebind, `PascalCase` consts never do, and there is no `const` keyword. Types are values; generics are calls (`List(int)`); optionals are `(T, null)` tuples and `null` is a type, not a value. Errors are returned values (`Err(...)`, `.fnType`), not exceptions. Inclusion is calls, nothing else: what you never call does not exist in the binary, at any depth. Std is ambient — `print`, `read`, `nav`, `env` just exist, never imported.

```go
!User = (name: str, age: int)        /// a type value, declarable in any scope
!ada  = User(name: "ada", age: 36)

add(a, b) {
  return a + b
}

!scores: List(int) = []
!cache: Map(str, User) = {}

for i in $(10) {
  /count = count + 1
}
```

## Usage

```bash
zap new <name>            # scaffold project + Zappr.yaml
zap run                   # build (in-memory where possible) + execute
zap build                 # linux / windows / web — via flags or targets
zap check                 # type-check only, fast feedback
zap test                  # run zap/tests/test.znz and its pipeline
zap fmt                   # the janitor
zap verify <artifact>     # check an artifact's Ed25519 signature
zap copyright headers     # stamp license headers, on explicit intent only
```

## Dependencies

None. The compiler is written in Rust (edition 2024) with zero dependencies.

## Building from source

```bash
cargo build --release
```

The compiler lives in [`src/`](./src). The full specification is in [`SPEC.md`](./SPEC.md).
