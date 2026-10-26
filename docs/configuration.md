# Configuration

Everything has a neutral default, and nothing reads a shell profile or an env file: set variables
in the environment `cross-agent` runs in.

| Setting | Default | Purpose |
|---|---|---|
| `CROSS_AGENT_LEDGER_DIR` | `$XDG_STATE_HOME/cross-agent`, else `~/.local/state/cross-agent` | Directory of the delegation ledger, `cross_agent_delegation.events.jsonl`. One line per delegation: target, labels, requested model and effort, and a digest of the prompt. Best-effort; a failed write is never fatal. |
| `CROSS_AGENT_AGY_FLASH` | `Gemini 3.8 Flash (High)` | The display name the `flash` alias resolves to. |
| `CROSS_AGENT_AGY_PRO` | `Gemini 3.8 Flash (High)` | The display name the `pro` alias resolves to. By default `pro` is the same Flash model at high effort as `flash`; set this variable to a different display name (from `agy models`) to make `pro` mean something else. |
| `XDG_CONFIG_HOME` | `~/.config` | Holds `agent-terminal/model-tags.json` (model tags, read by `status` and `select`). |
| `XDG_CACHE_HOME` | `~/.cache` | Holds `agent-terminal/usage.json` (headroom) and `agent-terminal/models.json` (the model catalogue). |
| `NUVEK_SECRET_SURFACES_FILE` | `$XDG_CONFIG_HOME/nuvek/secret-surfaces.toml`, else `~/.config/nuvek/secret-surfaces.toml` | Extra files and directories whose handling counts as secret. Optional. See [Secret surfaces](#secret-surfaces). |
| `NUVEK_LOG_SINK` | journald and stderr | Set to `stderr` to keep log lines off the journal. |

## How agy resolves `--model`

agy runs on its own display names (for example `Gemini 3.8 Flash (High)`), and `cross-agent`
resolves the value you pass in this order:

1. The aliases `flash` and `pro` become the display names in `CROSS_AGENT_AGY_FLASH` and
   `CROSS_AGENT_AGY_PRO`.
2. A catalogue id (the `--model` value shown by `cross-agent status`) is translated to its display
   name through the cached catalogue. With no catalogue, the value passes through unchanged.
3. Anything else, including a full display name, passes through unchanged.

An explicit `--effort` for agy needs a catalogue id, because the effort rides in the model id
(`<id>-<effort>`). The catalogue must offer that effort for the model, or the run is refused
before anything is spawned.

The default aliases name display names that a harness may retire. Run `agy models` and set the two
`CROSS_AGENT_AGY_*` variables when they go stale.

## Model tags, usage and catalogue

`status` and `select` read three JSON files and never write them. The usage and catalogue files are
written by [Agent Terminal](https://github.com/jean1880/nuvek-agent-terminal), or by any tool that
writes the same files; without them `status` reports each as `missing` and `select` has no models
to rank. `model-tags.json` is yours to write:

```json
{
  "version": 1,
  "vocabulary": { "code": "writes and edits code", "review": "reviews a diff" },
  "models": [
    { "match": "*opus*", "driver": null, "tier": "deep", "tags": ["code", "review"], "best_for": "hard reviews" },
    { "match": "gpt-6*-sol", "driver": "codex", "tier": "deep", "tags": ["review"] }
  ],
  "effort_policy": {
    "ladder": ["minimal", "low", "medium", "high", "xhigh", "max"],
    "fast": "low", "standard": "medium", "deep": "high",
    "reserved": ["xhigh", "max"]
  }
}
```

`match` is a case-insensitive glob over the catalogue id (`*` anywhere); the first entry whose
`driver` (when set) and `match` fit wins. `effort_policy` names an effort per tier; it is clamped to
what a model offers and never lands on a `reserved` rung.

## Secret surfaces

`cross-agent` loads its secret-surface configuration before it does anything else, through
`nuvek_core::scrub::init()` from [nuvek-core](https://github.com/jean1880/nuvek-core). The
built-in surfaces (`*.pem`, `*.key`, `~/.ssh/`, `.env` files and similar) are always on, and a
`secret-surfaces.toml` can add to them. If that file is set but unusable (a missing explicit path, a
parse error, a bad entry), `cross-agent` prints the reason, which carries no secret value, and
exits 78 instead of running with less protection than you configured. Fix the file, or unset
`NUVEK_SECRET_SURFACES_FILE`, to continue.
