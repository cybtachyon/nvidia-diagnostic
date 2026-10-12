# Agent Guidelines
This is a monorepo of a Rust CLI for diagnosing issues with Nvidia graphics cards on Linux. 

## Technical Overview
- The focus is on useful diagnostics and analysis. 

# Dev Environment Tips
- Read `Cargo.toml` for the list of configured packages and dependencies.
- You can run `cargo` commands with `CARGO_HOME` set to `.cargo` to avoid sandbox restrictions.
    - e.g. `CARGO_HOME=.cargo cargo build` `CARGO_HOME=.cargo cargo fmt` `CARGO_HOME=.cargo cargo test`

## Code Style
- Follow Rust conventions: [Effective Rust]([https://go.dev/doc/effective_go](https://effective-rust.com/title-page.html)). Be concise, declarative, and factual. Use alphabetical order.
- Use a declarative and explicit coding style. Ensure single sources of truth and use language mechanics for deterministic behavior. Use tools like Enums, Generics, and Traits if they fit the problem.
- Never use grammatical shortcuts like emdash. Use separate sentences first or commas and semicolons if necessary. Avoid pronouns and adverbs as much as possible.
- **Doc comments** start with the name and state what it does. Structure: `// [Name] [verb]s [what]. [Optional: when/why to use it].` Put useful information where users make decisions (usually the constructor, not methods).
- **Inline comments** are terse. Prefer end-of-line when short enough.
- **Explain why, not what.** The code shows what it does; comments should explain reasoning, non-obvious decisions, or edge cases.
- **Wrap comments** at ~80 characters, continuing naturally at word boundaries.
- **Naming matters.** Before proposing a name, stop and review existing names in the file. Ask: what would someone assume from this name? Does it fit with how similar things are named? A good name is accurate on its own and consistent in context.
- **Comment content:**
    - Add information beyond what the code shows (not tautologies)
    - State directly: "Returns X" (not "Note that this returns X")
    - Drop filler: "basically", "actually", "really" add nothing

## Docs
- [Rustyscript](https://docs.rs/rustyscript/latest/rustyscript/)
- [indiciatif](https://docs.rs/indicatif/latest/indicatif/)
- [inquire](https://docs.rs/inquire/latest/inquire/)
- [owo_colors](https://docs.rs/owo-colors/latest/owo_colors/)
- [Packager](https://docs.crabnebula.dev/packager/)
- [Packager Updater](https://github.com/crabnebula-dev/cargo-packager/tree/main/crates/updater)

## Testing Instructions
- Lint with `cargo fmt` and `cargo clippy --fix --allow-dirty`
- Test with `cargo test`
