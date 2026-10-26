# Usage

```bash
# Antigravity, with a model alias and a 5 minute bound
cross-agent agy --model flash --timeout 300 "Review this diff for correctness"

# Antigravity, a catalogue model id with a reasoning effort
cross-agent agy --model gemini-4-argon --effort high "Audit the staged changes"

# Claude Code, a named subagent, a model and a reasoning effort
cross-agent claude --agent code-reviewer --model opus --effort high "Audit the staged changes"

# Codex: always a read-only sandbox, so it is a checker, never a maker
cross-agent codex --timeout 600 "Find the flaw in this design"

# A long brief from a file or stdin (at most 1 MiB), with one JSON receipt on stdout
cross-agent claude --prompt-file brief.md --task-id t-42 --review-id r-7 --json
cat brief.md | cross-agent agy --prompt-file - --json

# Which models does each harness offer, and how much headroom is left?
cross-agent status
cross-agent status --json

# Pick one model for a task by capability tag, tier and headroom
cross-agent select --tag code --tier deep --from claude
```

## Commands and flags

- **`agy`, `claude`, `codex`** run one headless delegation. The prompt is a positional argument or
  `--prompt-file PATH` (`-` is stdin). `--task-id` and `--review-id` label the run in the receipt.
  Codex has no personas, so it takes no `--agent`.
- **`--json`** prints one receipt instead of the raw output: outcome, exit code, duration, the model
  and effort that were requested and the ones the launch was configured with, a SHA-256 of the
  prompt, and the scrubbed, length-bounded output. Exit codes are the same with and without it.
- **`--effort`** is a lowercase word (`low`, `medium`, `high`, ...). Claude Code takes it as
  `--effort`, Codex as a `model_reasoning_effort` config override, and agy in the model id, which
  must then be a catalogue id that offers that effort (see `status`).
- **`status`** reads three files and spawns nothing: it lists each harness's models with their tags,
  tier and `--model` value, and its subscription headroom.
- **`select`** filters and ranks those models for a task: the tier must match, every `--tag` must be
  present, the model must offer the effort, and a harness with `low` or `exhausted` headroom is
  left out. It exits 4 when nothing qualifies.

A prompt is never written to a log or to the ledger: only its length and a digest are kept. The
ledger, the logs and the `--json` receipt are scrubbed of secrets. Without `--json`, the delegate's
output is passed through to your terminal unchanged.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | The delegate succeeded (or `status`/`select` printed a result) |
| 1 | A runtime or input error: invalid input, a missing binary, or a failed delegation |
| 2 | A command-line parsing or usage error (unknown flag, missing argument) |
| 4 | `select` found no model that qualifies |
| 75 | `EX_TEMPFAIL`: the delegate failed and its output reads as a quota or rate limit. Try again later or use another harness |
| 78 | `EX_CONFIG`: the secret-surface configuration was refused at start-up (see [configuration](configuration.md#secret-surfaces)) |
| 124 | The wall-clock timeout was reached |
| other | The delegate's own exit code is passed through; death by signal is `128 + signal` |

Exit 75 applies only to a run that failed. The successful output of a review may talk about rate
limits and is never swallowed.

## Troubleshooting

- **Exit 78 at start-up:** the secret-surface file is set but unusable. The message names the
  reason; fix the file or unset `NUVEK_SECRET_SURFACES_FILE`.
- **Exit 75:** the delegate hit a quota or rate limit. Wait, or delegate to another harness.
- **Exit 124:** the wall-clock bound was reached. Raise `--timeout`.
- **An `agy` model is refused:** the default alias names a retired display name. Run `agy models`
  and set `CROSS_AGENT_AGY_FLASH` and `CROSS_AGENT_AGY_PRO`.
