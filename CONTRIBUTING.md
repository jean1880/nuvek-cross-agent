# Contributing to cross-agent

Thanks for taking the time to help. Bug reports, ideas, documentation fixes and code are all
welcome. This guide says how to get a change from an idea to a merged pull request.

Everyone taking part is expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

**Security problems are not reported in issues.** Follow [SECURITY.md](SECURITY.md) and use
GitHub's private vulnerability reporting.

## Reporting a bug

[Open a bug report](https://github.com/jean1880/nuvek-cross-agent/issues/new?template=bug_report.yml).
Search the [open and closed issues](https://github.com/jean1880/nuvek-cross-agent/issues?q=is%3Aissue)
first, and try `master`, where fixes land before a release. If the problem also happens when you run
`agy`, `claude` or `codex` directly, report it to that CLI's vendor.

**Review everything you paste for secrets.** Output and logs can hold API keys, tokens, hostnames
and file paths. Replace anything sensitive with `****` before you post.

## Before you write code

**Open an issue first for anything bigger than a small fix**: a new feature, a new subcommand, a
change of behaviour or a new dependency. A short discussion up front avoids a pull request that
cannot be merged as written. Typo fixes, small documentation fixes and obvious bug fixes can go
straight to a pull request.

The rules that most often decide a review:

- **Codex stays read-only.** It is a checker, never a maker.
- **A prompt is data.** It never becomes a flag, a config override or a log line.
- **Every external call is bounded by a timeout.**
- **Fail closed on a security boundary.** An unusable secret-surface configuration stops the run.
- **No `unwrap`, `expect` or `panic!` outside tests.**
- **Strings are sliced by character, never by byte.**
- **Tests never touch real `$HOME` state.** Use a temporary directory.
- **Nothing assumes one machine.** A path, endpoint or model name that would differ elsewhere is a
  setting with a neutral default, documented in `docs/configuration.md`.

## Setting up

You need Rust 1.90 or newer.

```bash
git clone https://github.com/jean1880/nuvek-cross-agent.git
cd nuvek-cross-agent
git config core.hooksPath .githooks   # gitleaks and rustfmt before each commit
```

The pre-commit hook needs [gitleaks](https://github.com/gitleaks/gitleaks) on your `PATH`.

## Testing

What CI runs, and what to run before you push:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check licenses bans sources
```

- **A bug fix** comes with a test that fails without the fix.
- **New logic** comes with unit tests beside it.

## Commits

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):

```text
fix(select): leave a harness with exhausted headroom out of the ranking
feat(status): report the catalogue's default effort
```

- **Types**: `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `style`.
- **Subject**: says what is now true, in lower case, without a full stop.
- **Body**: explains why, when the subject alone doesn't. Mention a breaking change as
  `BREAKING CHANGE:` in the footer.
- **Sign off every commit** (`git commit -s`). The `Signed-off-by:` line certifies the
  [Developer Certificate of Origin](https://developercertificate.org/): that you wrote the change
  or otherwise have the right to submit it under the project's licence. Pull requests with
  unsigned commits are not merged.

Write prose in Canadian English (colour, behaviour, centre, licence as a noun). Code identifiers
keep their own spelling.

## Pull requests

1. Run the checks above and make sure they pass.
2. Open the pull request against `master`. Its title follows the Conventional Commits format.
3. Link the issue with `Fixes #123`, so it closes on merge.
4. Update the README when a setting, option or exit code changes.

Releases are cut by the maintainer, who writes the notes in `docs/releases/`. Leave the version in
`Cargo.toml` alone.

## AI-assisted contributions

Using AI tools in a contribution is fine. The responsibility for it stays with you: say so in the
pull request, understand every line, and run the checks yourself.

## Licence

cross-agent is released under the [MIT Licence](LICENSE). By contributing, you agree that your
contribution is licensed under the same terms, and you certify the Developer Certificate of Origin
with your `Signed-off-by:` line.
