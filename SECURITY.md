# Security policy

## Supported versions

Only the latest release (and `master`) receives security fixes.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/jean1880/nuvek-cross-agent/security/advisories/new),
not in a public issue. Only the maintainers see the report.

Include the version, the steps to reproduce and the impact you expect. Leave out real secrets,
tokens and hostnames: a placeholder shows the problem just as well.

You should get an acknowledgement within a week. Once a fix is released, the advisory is published
with credit to the reporter unless you prefer otherwise.

## Scope

cross-agent starts other agent CLIs and records what happened. Of particular interest:

- a secret reaching a log line, the delegation ledger or a `--json` receipt without redaction;
- a prompt, model or effort value that is read as a command-line flag or a config override instead
  of data (the Codex `-c key=value` override, the `--` separator);
- the Codex delegate leaving its read-only sandbox;
- a run that outlives its wall-clock timeout;
- `--prompt-file` reading more than its size bound.

## Known exposure: the prompt is a command-line argument

`cross-agent` hands the prompt to `agy`, `claude` and `codex` as a command-line argument, so any
other local user can read it in the process list (`ps`, `/proc/<pid>/cmdline`) while the delegate
runs. `--prompt-file` does not change this: the file is read by `cross-agent`, and its contents
still end up in the child's argument list. What `--prompt-file` does avoid is the prompt appearing
in your shell history and in `cross-agent`'s own command line. Do not put a secret in a prompt on a
machine shared with users you do not trust.

Vulnerabilities in the agent CLIs themselves (Claude Code, agy, Codex) belong with their vendors.
