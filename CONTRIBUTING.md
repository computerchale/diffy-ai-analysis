# Contributing

Thanks for helping out. Bug reports, fixes, and new editor or agent integrations are all welcome.

## Getting set up

You need the tools listed under [Requirements](README.md#requirements): Rust 1.88 or newer, git, and (to try capture end to end) Cursor and bash.

```bash
cargo build
cargo test
cargo fmt --check
cargo clippy --all-targets
```

CI runs the same commands on Linux and macOS with warnings treated as errors, plus a test run on the minimum supported Rust version (1.88). Please make sure they all pass locally before opening a pull request.

## How changes are made

[SPEC.md](SPEC.md) is the source of truth for behaviour, and development is test-driven:

1. **Spec first.** If your change adds or alters behaviour the spec doesn't cover, amend `SPEC.md` in the same pull request.
2. **Failing test next.** Write a test that encodes the behaviour and watch it fail before you implement anything.
3. **Then the implementation**, until the test passes.

Conventions:

- Test names end with the spec section they check, e.g. `replay_preserves_spans_through_partial_edit_6_1` for §6.1.
- Integration tests use real git repos in temp directories. Don't mock git.
- Every module has `//!` docs naming its spec section, and public items have `///` docs.
- No `.unwrap()` outside tests; clippy denies it. Use `?` with context, or `.expect("…")` explaining why it can't fail. A panic in the hook loses data without anyone seeing it.
- Attribution must fail closed (SPEC §10, invariant I4): when diffy can't be sure who wrote something, it reports `untracked`/unknown rather than guessing.

The same rules live in [`.cursor/rules/tdd.mdc`](.cursor/rules/tdd.mdc), so AI agents working in this repo follow them too.

## Pull requests

- Keep each pull request to one change, and say which spec section it touches.
- Add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything user-visible.
- Don't commit generated reports (`diffy-report.html`) or anything from `target/`.

## Reporting bugs

Please include your OS, `diffy --version`, the command you ran, and what you expected. For capture problems, the hook debug log described under [Debugging](README.md#debugging) helps a lot. Check it for file contents you don't want to share before attaching it.

## License

By contributing, you agree that your contributions are dual-licensed under MIT or Apache-2.0, the same as the project, without any additional terms or conditions.
