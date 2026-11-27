<p align="center">
  <img src="./zappr.svg" alt="zappr" width="320">
</p>

# zappr

**One language. One tool. One artifact per platform.**

Zappr is a compiled, fully native programming language. The tool is `zap`: one binary that is compiler, build driver, runner, package manager and formatter at once. There is no separate compiler entry point and no tooling split — `zap` is the only way in. Zappr yoinks the good parts of C, C++, TypeScript, Rust, Go and the rest, and leaves the bad parts. It compiles to Linux, Windows and the Web, all native. Native UI is built into the language, not bolted on. Configuration is one `Zappr.yaml`. Compile goal: under 5 seconds for a full build, incremental builds faster. Zappr is owned by AXIOM.

> **Status:** `0.1.0` (in development)

## Why

- **No sidecars.** One artifact per platform, nothing transient is ever written into a user project, no runtime files, no `.so` shims. If `zap` cannot produce one clean artifact, it fails with an error. It does not produce partial output.
- **Native, not a wrapper.** Zappr drives its own codegen, its own linker configuration, its own everything. The build pipeline never delegates compilation, linking or verification to external tools — no gcc, no rustc, no chained tooling, no verify-via. One tool: zap.
- **Minimal configuration.** One `Zappr.yaml`, easy to read, edit and parse. Unknown keys are errors; defaults exist but are never silent.
- **Signed by default.** Every build artifact is Ed25519-signed; `zap verify` checks it before you run something you downloaded.
- **Static linking.** Only base system libraries (libc, libm, libgcc and the loader) remain dynamic.
- **Blazing fast.** Under 5 seconds for a full build. The compiler is parallel, incremental and cache-first by design, not by tuning. Fast codegen, not LLVM — Cranelift-tier for v1, hand-tuned backends later.

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

One Rust binary (edition 2024), permanently — Zappr is not self-hosted and is not intended to be. The compiler is an axiom tool; axiom tools are Rust.

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
- **The checker is fast by construction.** Structural types need no cross-module unification; exclusivity checks are per-function local. There is no whole-program type fixpoint anywhere in the pipeline.

## Language

### Assignment is always marked

`!` declares; `/` reassigns; bare `=` is a hard error. Semicolons are optional — a newline ends a statement. The language has no implicit state changes: every mutation is visually loud, and a typo'd assignment (`lenght = 5`) is a compile error, never an implicit global.

```go
!x = 5            /// declare, inferred
!x: int = 5       /// declare, typed
/x = 6            /// reassign — x must already exist
/x+               /// increment (courtesy; /x = x + 1 is preferred)
x = 6             /// COMPILE ERROR — unmarked assignment
```

### The casing is the mutability marker

```go
!retries = 3        /// camelCase → variable, /retries = 4 is fine
!MaxRetries = 3     /// PascalCase → const; /MaxRetries = 4 is a COMPILE ERROR
```

There is no `const` keyword, no `let`/`var`, no `mut`. Immutable-by-default falls out of the naming law. The naming law itself is compiler-enforced: `camelCase` for variables, functions, fields and use-names; `PascalCase` for types and consts; lowercase filenames and folders under `zap/`.

### No shadowing

A name is declared exactly once, ever, in its scope tree. Redeclaration — including in a nested scope — is a compile error.

### Types

| Type | Default | Opt-in fixed form |
|------|---------|-------------------|
| `int` | arbitrary precision, grows by value on the fly — never overflows, never wraps | `i8` `i16` `i32` `i64` `u8` `u16` `u32` `u64` |
| `float` | grows precision on the fly | `f32` `f64` |
| `str` | owning, growable, refcounted | `&str` borrowed slice |

Mixing coerces toward the bigger side (`int + i32` just works); real narrowing is an explicit `as u8` cast. Tuples are unlimited in arity and are the composition primitive: parameter lists, returns, type-argument lists and optionals are all tuples.

**Types are values.** There is exactly one kind of thing in Zappr: values. Types are values — passable, returnable, storable. `null` is a *type*, not a value.

```go
!T = User               /// a type in a variable
!scores: List(int) = [] /// List takes a type, returns a type — generics are calls
```

**Optionals** are `(T, null)` tuples — the pad form, always available and always explicit. `autoNull: true` in `Zappr.yaml` fills the gap for un-annotated values: optionality is inferred, padding is automatic, access auto-unwraps.

**Your own types** are named tuples; unions are tuples of type values, read back with exhaustive `match`:

```go
!User = (name: str, age: int)        /// a type value, declarable in any scope
!ada = User(name: "ada", age: 36)    /// construct by field name
ada.name                             /// access by name — .0 and .1 work too

!Circle = (r: float)
!Rect   = (w: float, h: float)
!Shape  = Circle | Rect              /// sugar for the type tuple (Circle, Rect)

match s {
  Circle as c { print.nl("circle, r {c.r}") }
  Rect as r   { print.nl("rect, {r.w}x{r.h}") }
}
```

Collection literals: `[]` is an empty list, `{}` an empty map, `{k: v}` a populated map — `Map(K, V)` values.

### Errors are values

The error channel is a returned value of a different type. No tuples-of-err, no exceptions, no `?`. `Err(...)` is the universal error value. A function may return *whatever it wants* — there are no preset return types and no return annotations.

`.fnType(T)` takes a type value and checks at **runtime**: match narrows to `T` once and stays narrowed; mismatch becomes a `wrongReturnType` poison carrying the expected type, the actual value, and the origin location. No panic, no crash — mismatches become inspectable data, NaN-style. Using a poison value as data fails loudly at the use site, with the full story.

```go
!user = fetchUser(3).fnType(User)     /// got Err back? user is now wrongReturnType — no crash

if fetchUser(3).fnType(User) as u {   /// branch: handle it properly
  print.nl("hello {u.name}")
} else as e {
  print.errNl("failed: {e}")
}
```

Unchecked use is allowed; misusing an `Err` as data fails loudly at first use, never silently.

### Functions

```go
add(a, b) {
  return a + b
}
```

Parameters bind positionally from the argument tuple. Arity is soft in both directions: fewer args make unbound params `missingArg` poison, surplus lands in `args.rest`, `args.count` inspects — policy is the function's, silent misinterpretation is the only banned behavior.

Built-ins are flat and verb-first: `print.nl(...)`, `print.raw(...)`, `print.err(...)`, `print.errNl(...)`, `print.fmt("hi {name}", name)`, `read(int age)`, `readInto(x)`, `readDialogue(str name)`. `{expr}` interpolation is a string-literal feature, not a print feature — compile-time checked everywhere.

### Generics are calls

No `<T>`. Angle brackets are comparison operators; the paren form is the only parameterization mechanism. A generic is a function that takes types and returns a type; instantiation is indistinguishable from a call. Bounds are structural — the body needs `.0` on `T`, so the call site's type must have `.0`. No traits, no impl blocks, no coherence rules. Compile once per instantiation actually called.

### Control flow

Five loop forms, one keyword:

```go
for (x, y) in z { ... }            /// tuple-loop: per-element destructure; (k, v) for maps
for cond { ... }                   /// while
for { ... }                        /// infinite; break / continue as usual
for i in $(10) { ... }             /// count-loop: exactly n times, i starts at 0
for (!i = 0; i < n; /i+) { ... }   /// C-style; the header obeys the !/ / law — no exemptions
```

There is no range operator. No `..`, no `0..10`. Counting is `for i in $(expr)` where expr is any number expression; inclusive counting is written by hand as `$(maxInclusive + 1)`. Growable `int` means absurd bounds are slow, never wrong.

### Parallelism — structured concurrency, built in

Go's cheap tasks without Go's leaks, no async/await anywhere, no function colors:

```go
main() {
  parallel {
    !images = fetchImages(zapYaml.imageDir)
    !metadata = loadMetadata()
  }                       /// join: all children done or one failed. No orphans, guaranteed.
  print.nl(images.len, metadata.title)

  !(tx, rx) = channel(int)
  spawn worker(tx)
  for value in rx {       /// channels drain until closed
    print.nl(value)
  }
}
```

`parallel { }` is a join point — nothing spawned inside outlives it. The first child to fail fails the block; siblings are cancelled at their next safe point, and every published name holds the same `Err(...)` carrying the failing child's origin. `parallel` is the one construct whose body declares *upward*: its `!` declarations are published into the enclosing scope at the join.

### Memory — move semantics + ARC

No garbage collector, and no Rust-style borrow checker — memory safety comes from ARC keeping referents alive, and borrow safety from the exclusivity law: **overlapping access to the same storage admits at most one writer — or any number of readers — never both.** Moves are compile-time and local; escaped borrows carry a per-object access flag enforced at runtime as `exclusiveAccess` poison. Rust's memory-safety guarantees — no dangling, no use-after-free, no data races, no iterator invalidation, no aliased mutation — with zero lifetime annotations and zero global analysis.

`weak(v)` wraps a shared value; reading it yields the `(T, null)` pad — an expired reference reads as the null side, never a dangling pointer.

### Native UI

UI is plain code. Elements are values; you build them, wire events, and change properties. No declarative tree, no `!{ }` blocks, no view functions, no reactivity graph, no virtual DOM.

```go
main() {
  !win = ui.window("demo", 800, 600)
  !label = win.add.text("count is 0")
  !btn = win.add.button("increment")
  !count = 0

  onClick(btn) {
    /count+
    /label.text = "count is {count}"   /// you change the value, the screen follows
  }

  ui.run(win)
}
```

One rule: **mutate an element's property and it updates.** The loop is frame-locked and lazy — a property change marks the window dirty, the next tick renders exactly one frame, and the loop sleeps. `frameRate: auto` (default) mirrors the device's actual frame production; a number is a hard lock. Routes are built in, same API native and web: `route("/users/{id}", UserPage)` with path params binding like arguments; navigation is `nav.go("/settings")`.

UI state has a three-tier lifecycle, automatic: hot (on screen), cold (navigated away, survives in RAM), gone (unused past `stateTtl`, default 10 minutes, LRU). zap warns when `stateTtl` is not declared.

### Modules and inclusion — calls, nothing else

- Std is **ambient**: `print`, `read`, `nav`, `env` just exist. Never imported.
- `use.depName` opens a namespace — written mid-file, wherever first needed. No header imports.
- Folders namespace **by fusion**: path segments fuse into one camelCase name, one dot ever — `zap/http/auth/session.znz` → `use.httpAuthSession`.
- **No access modifiers.** A file's top-level names are reachable by anyone who can name its use-path. Encapsulation is structural.
- The unit of inclusion is the **call** — transitive. What you never call does not exist in the binary, at any depth. Dead-code elimination is not a pass; it is the default state.
- **Tidiness is law**: scattered files and banned folder names (`server/`, `utils/`, `misc/`, `helpers/`, `common/`, `lib/`) are compile errors. Name parts of your program, not jobs.

### Dependencies — GitHub is the registry

No registry, no lock file. `Zappr.yaml` is the lock — and it locks **content**, not names. `githubDeps:` maps dep names to `github:owner/repo@ref`; on first fetch zap resolves the ref to an exact commit, hashes the source tree, and records both under `resolved:`. Every build after that replays the recorded commit and verifies the hash — verify, never hope. Deps are source code, nothing more: no scripts, no build steps, no toolchain, no build-time privileges. The lock is flat: deps' own `githubDeps` resolve into the app's one `Zappr.yaml`.

### Call flattening

Because the compiler sees every call with full semantics, in one visibility domain, known calls **flatten**: the compiler emits what the call was going to do, not the call. `thisShell.shellOut("git rev-parse HEAD")` compiles to a direct `git` exec plus stdout capture — the shell-parsing wrapper layer is flattened away at build time. Repetition anywhere the compiler can see it collapses to its direct form; external dependencies flatten identically because they ship as source.

## Zappr.yaml

One file. Every key optional except where noted; unknown keys are errors; defaults are never silent.

```go
# Zappr.yaml
name: my-app                /// required
version: 0.1.0              /// required

license: MIT                /// optional — overrides LICENSE fuzzy-matching
licenseHolder: axiomwtf     /// optional — for zap copyright headers

targets:                    /// optional — default is the host
  - linux
  - windows
  - web

stateTtl: 10m               /// optional — WARNING if omitted
frameRate: auto             /// optional — auto or a number
autoNull: false             /// optional — ergonomic optional handling

sign: true                  /// optional — Ed25519 signing, default true
signKey: ./signing.key      /// optional — signing key location

val:                        /// build-time values; read verbatim as zapYaml.x
  appName: Zappr Demo
  maxRetries: 3

githubDeps:                 /// dependencies
  json: github:someone/jsonlib

cli:                        /// custom zap commands, written in Zappr
  - tools/stamp.znz
```

`val:` entries are immutable from within Zappr code — the only mutation path is a **custom zap CLI**, project-registered commands written in Zappr that run in-process with build privileges. Code consts are truly immutable; `val:` entries are immutable at runtime and mutable only at build time.

## Project layout

```
my-app/
├── Zappr.yaml          /// the one config
└── zap/                /// all source lives here
    ├── main.znz        /// the entry point — exactly one, at zap/ root
    ├── gui/
    │   └── widgets.znz
    ├── http/
    │   └── routes.znz
    └── tests/
        ├── test.znz    /// the orchestrator — zap test runs this
        └── fetchuser/
            ├── happypath.znz
            └── rejectsnegative.znz
```

`main.znz` is exactly one, at `zap/` root — duplicates are an error. It orchestrates: calls the pieces in order, wires startup, gets out. Tests live in `zap/tests/`; the assertion toolkit is `assert`, `.fnType` and the poison values — error cases are just values you check.

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

### Build outputs

- **linux / windows:** one static executable per platform. License headers ride inside the binary as a comment section.
- **web:** exactly two files — `app.wasm` (the whole program, UI engine included) and `404.html` (a small, static, self-contained wasm loader, stamped verbatim from inside zap — same bytes for every project). No JS shims, no bundler, no hidden third file. `404.html` is named so GitHub Pages routes deep links into the app.

### zap fmt — the janitor

`zap fmt` is a formatter, organizer and splitter with explicit consent (`y/n/a`: yes to this one, no, or all remaining):

```
zap fmt
  add copyright headers to 14 files? (y/n/a)
  reorganize 3 scattered files? (y/n/a)
  split services.znz (1,204 lines) into 4 files? (y/n/a)
```

Headers are stamped idempotently; organize moves scattered files into proper folders and dissolves banned drawers with proposed names shown in the prompt — fmt never invents names on its own, and every affected `use.` path is mechanically rewritten; split offers topic-based splitting for giant files, never mechanical line-counting.

### Copyright headers

`zap copyright headers` runs on **explicit intent only** — nothing touches files during build. zap fuzzy-matches the project's `LICENSE` against known licenses; no match is a hard error, `license:` in `Zappr.yaml` overrides detection. Headers survive compilation: as a comment in web artifacts, as a comment section inside native binaries. The license travels with the artifact.

## Performance budget

- **Under 5 seconds** for a full build; incremental much faster.
- Parallel compilation by module; incremental content-hashed caches in the system temp dir — never in the project.
- Generics monomorphize per-called-instantiation only; the inclusion rule guarantees no unused instantiations.
- Binary size = sum of what you called. Nothing else.

## Non-goals

- No self-hosting. The compiler is Rust, permanently.
- No package registry, no lock file, no dependency network beyond GitHub.
- No borrow checker — no lifetime annotations, no global borrow analysis; the exclusivity law provides the same guarantees locally. No garbage collector, no async/await, no function coloring.
- No template language for UI, no JSX, no virtual DOM.
- No LLVM backend in v1.
- No sidecar files of any kind — if a feature requires one, the feature is wrong.

## Dependencies

None. The compiler is written in Rust (edition 2024) with zero dependencies.

## Building from source

```bash
cargo build --release
```

The compiler lives in [`src/`](./src). The full specification is in [`SPEC.md`](./SPEC.md).
