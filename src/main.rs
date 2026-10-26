//! cross-agent — Cross-harness delegation between Antigravity (Gemini), Claude Code (Anthropic)
//! and Codex (OpenAI).
//!
//! Spawns headless subagent sessions across AI coding harnesses, enforcing wall-clock timeouts,
//! parsing rate-limit conditions, and keeping context budgets lean.

mod input;
mod receipt;
mod select;
mod status;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use nuvek_core::util::command_output_timeout;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Ledger job name: events land in `<ledger dir>/cross_agent_delegation.events.jsonl`.
const JOB: &str = "cross_agent_delegation";

/// Environment variable naming the delegation ledger directory.
const LEDGER_DIR_ENV: &str = "CROSS_AGENT_LEDGER_DIR";

/// Exit code when the secret-surface configuration is refused (`EX_CONFIG`). Distinct from the
/// codes the CLI itself uses (1 failure, 75 rate limited, 124 timeout), and from a delegate's own
/// exit code only by convention: a delegate that exits 78 is passed through unchanged.
const EXIT_CONFIG: i32 = 78;

/// The delegation ledger directory: `$CROSS_AGENT_LEDGER_DIR`, else `$XDG_STATE_HOME/cross-agent`,
/// else `~/.local/state/cross-agent`.
fn ledger_dir() -> PathBuf {
    ledger_dir_from(
        nuvek_core::env::env_var(LEDGER_DIR_ENV),
        nuvek_core::env::env_var("XDG_STATE_HOME"),
        nuvek_core::env::home_dir(),
    )
}

/// [`ledger_dir`] with its inputs passed in. A relative `XDG_STATE_HOME` is ignored, as the XDG
/// specification says.
fn ledger_dir_from(explicit: Option<String>, xdg_state: Option<String>, home: PathBuf) -> PathBuf {
    if let Some(dir) = explicit {
        return PathBuf::from(nuvek_core::env::expand_home(&dir));
    }
    xdg_state
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map_or_else(
            || home.join(".local/state/cross-agent"),
            |p| p.join("cross-agent"),
        )
}

/// Record that `prompt` is about to be sent to `target`, in the [`JOB`] ledger, so a tool that
/// reads the ledger can tell a prompt an agent wrote from the user's own words. Only a digest of
/// the prompt is kept. Best-effort: a failed write is warned and never blocks the delegation.
fn record_delegation(target: &str, prompt: &str, meta: &receipt::Meta) {
    let dir = ledger_dir();
    let mut fields = serde_json::Map::new();
    fields.insert("target".into(), target.into());
    for (name, value) in [
        ("task_id", meta.task_id),
        ("review_id", meta.review_id),
        ("requested_model", meta.requested_model),
        ("requested_effort", meta.requested_effort),
    ] {
        if let Some(value) = value {
            fields.insert(name.into(), nuvek_core::scrub::redact(value).into());
        }
    }
    fields.insert("cold_review".into(), true.into());
    fields.insert(
        "prompt_key".into(),
        nuvek_core::digest::prompt_key(prompt).into(),
    );
    nuvek_core::metrics::Metrics::new(dir).append_event(JOB, fields);
}

#[derive(Parser, Debug)]
#[command(name = "cross-agent", version)]
#[command(about = "Cross-harness delegation between Antigravity, Claude Code and Codex")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug, PartialEq)]
enum Commands {
    /// Dispatch a prompt or subagent to Antigravity (AGY / Google Gemini)
    Agy {
        /// Subagent persona to invoke (an agent defined for the harness)
        #[arg(short, long)]
        agent: Option<String>,

        /// Model override (flash, pro)
        #[arg(short, long)]
        model: Option<String>,

        /// Reasoning effort (low|medium|high|...); use `status` effort_for for the model.
        #[arg(short, long)]
        effort: Option<String>,

        /// Execution timeout in seconds (default: 180)
        #[arg(short, long, default_value_t = 180)]
        timeout: u64,

        #[command(flatten)]
        delegate: Delegate,
    },
    /// Dispatch a prompt or subagent to Claude Code (Anthropic)
    Claude {
        /// Subagent persona to invoke (a Claude Code agent name)
        #[arg(short, long)]
        agent: Option<String>,

        /// Model override (haiku, sonnet, opus)
        #[arg(short, long)]
        model: Option<String>,

        /// Reasoning effort (low|medium|high|...); use `status` effort_for for the model.
        #[arg(short, long)]
        effort: Option<String>,

        /// Execution timeout in seconds (default: 180)
        #[arg(short, long, default_value_t = 180)]
        timeout: u64,

        #[command(flatten)]
        delegate: Delegate,
    },
    /// Dispatch a prompt to Codex (OpenAI), always in a read-only sandbox
    Codex {
        /// Model override (a Codex model name, passed through)
        #[arg(short, long)]
        model: Option<String>,

        /// Reasoning effort (low|medium|high|...); use `status` effort_for for the model.
        #[arg(short, long)]
        effort: Option<String>,

        /// Execution timeout in seconds (default: 180)
        #[arg(short, long, default_value_t = 180)]
        timeout: u64,

        #[command(flatten)]
        delegate: Delegate,
    },
    /// Read-only: models per harness with tags, tier, `--model` value and subscription headroom
    Status {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Read-only: pick one model for a task by tags, tier, effort and subscription headroom
    Select {
        /// Required capability tag (repeat or comma-separate; see `status` vocabulary)
        #[arg(long = "tag", required = true, value_delimiter = ',')]
        tags: Vec<String>,

        /// Task tier: only models of this tier qualify, and `effort_for[tier]` sets the effort
        #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(select::TIERS))]
        tier: String,

        /// Require this exact effort (refused where the model does not offer it)
        #[arg(short, long)]
        effort: Option<String>,

        /// Your own harness (claude|agy|codex): on a tie another harness is preferred
        #[arg(long)]
        from: Option<String>,

        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
}

/// What every delegation needs besides its model settings: the prompt, labels and output mode.
#[derive(Args, Debug, PartialEq)]
struct Delegate {
    /// The prompt to execute (or use --prompt-file)
    #[arg(
        required_unless_present = "prompt_file",
        conflicts_with = "prompt_file"
    )]
    prompt: Option<String>,

    /// Read the prompt from PATH instead (`-` = stdin; at most 1 MiB)
    #[arg(long, value_name = "PATH")]
    prompt_file: Option<PathBuf>,

    /// Label for the task this run serves; echoed in the --json receipt
    #[arg(long, value_parser = input::parse_id)]
    task_id: Option<String>,

    /// Label for the review this run is part of; echoed in the --json receipt
    #[arg(long, value_parser = input::parse_id)]
    review_id: Option<String>,

    /// Print ONE JSON receipt (outcome, exit code, duration, requested vs launch-configured model,
    /// prompt hash, scrubbed output) instead of the raw output; exit codes are unchanged
    #[arg(long)]
    json: bool,
}

/// Propagate exit code matching shell semantics.
fn exit_code(status: &ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        code
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(0)
        }
        #[cfg(not(unix))]
        {
            1
        }
    }
}

/// Detect if Claude Code output indicates rate limits or session quotas.
fn is_claude_rate_limited(combined_output: &str) -> bool {
    let lower = combined_output.to_lowercase();
    lower.contains("session limit")
        || lower.contains("rate limit")
        || lower.contains("hit your limit")
        || (lower.contains("resets") && lower.contains("limit"))
}

/// Detect if Antigravity (Gemini) output indicates quota exhaustion: the gRPC status
/// `RESOURCE_EXHAUSTED`, a "Resets in 3h" reset hint, or HTTP 429. A bare `429` is a common
/// number, so it only counts next to an unambiguous quota phrase (`429 Too Many Requests`,
/// `429 ... quota`, `429 ... rate limit`).
fn is_agy_rate_limited(combined_output: &str) -> bool {
    let lower = combined_output.to_lowercase();
    if lower.contains("resource_exhausted") || lower.contains("resets in") {
        return true;
    }
    lower.contains("429")
        && ["too many requests", "quota", "rate limit"]
            .iter()
            .any(|w| lower.contains(w))
}

/// Detect if Codex output indicates an exhausted plan window or an API rate limit. `codex exec`
/// echoes the prompt into its stderr transcript, so only Codex's own error wording counts — a
/// review brief that merely mentions "rate limit" must not turn an unrelated failure into exit 75.
fn is_codex_rate_limited(combined_output: &str) -> bool {
    let lower = combined_output.to_lowercase();
    [
        "hit your usage limit",
        "usage_limit_reached",
        "rate_limit_exceeded",
        "429 too many requests",
    ]
    .iter()
    .any(|w| lower.contains(w))
}

/// Exit 75 (EX_TEMPFAIL) applies only to a run that FAILED: the successful output of a review
/// may legitimately talk about rate limits, and must never be swallowed.
fn is_rate_limited_run(h: &Harness, exit: i32, combined: &str) -> bool {
    exit != 0 && (h.rate_limited)(combined)
}

/// One headless delegation: which CLI, how it is labelled in logs and errors, and how it spots a
/// rate limit in its own output.
struct Harness {
    /// Ledger and log target, and the binary name: `agy`, `claude` or `codex`.
    target: &'static str,
    /// Whether stderr is passed through on a successful run. `codex exec` writes its whole
    /// session (banner, the echoed prompt, token count) to stderr and only the answer to stdout,
    /// so its stderr is shown only when the run fails.
    stderr_on_success: bool,
    /// Human name for messages.
    label: &'static str,
    rate_limited: fn(&str) -> bool,
    /// Maps a short `--model` alias to the name the harness accepts.
    resolve_model: fn(&str) -> String,
    /// The CLI arguments for one headless run: prompt, optional persona, resolved model.
    args: fn(&str, Option<&str>, Option<&str>) -> Vec<String>,
    /// How the harness takes a reasoning effort: `claude` has `--effort`; `codex exec` has no flag
    /// but reads the `model_reasoning_effort` config key through `-c`; agy lists `--effort` but
    /// refuses it for its models, which take the effort in the id (`gemini-4-argon-high`, as
    /// agent-terminal's `CatalogModel::model_id_for` composes it).
    effort: EffortArg,
}

#[derive(Clone, Copy)]
enum EffortArg {
    /// `<flag> <effort>`.
    Flag(&'static str),
    /// `-c <key>=<effort>`.
    ConfigKey(&'static str),
    /// `--model <catalogue id>-<effort>`.
    ModelSuffix,
}

/// agy's model argument for an effort: `<id>-<effort>`, when the catalogue offers that effort for
/// that id. Anything else is refused with what is offered, before a spawn.
fn agy_effort_model(cat: &status::Loaded, model: Option<&str>, effort: &str) -> Result<String> {
    let Some(model) = model else {
        bail!("agy takes the effort in the model id: pass -m <catalogue id> (see `cross-agent status`)");
    };
    let Some(offered) = status::agy_efforts_for_id(cat, model) else {
        bail!("agy effort needs a catalogue model id (see `cross-agent status`); got {model:?}");
    };
    if !offered.iter().any(|e| e == effort) {
        bail!("agy model {model:?} offers efforts {offered:?}, not {effort:?}");
    }
    Ok(format!("{}-{effort}", model.trim().to_lowercase()))
}

/// Extra CLI arguments for `--effort`. The value must be a bare lowercase word: it lands in a
/// `-c key=value` override for Codex, where anything else could inject more config.
fn effort_args(h: &Harness, effort: Option<&str>) -> Result<Vec<String>> {
    let Some(e) = effort else {
        return Ok(Vec::new());
    };
    let e = e.trim();
    if e.is_empty() {
        bail!("--effort cannot be empty");
    }
    if !e.chars().all(|c| c.is_ascii_lowercase()) {
        bail!("--effort must be a lowercase word such as low|medium|high (got {e:?})");
    }
    Ok(match h.effort {
        EffortArg::Flag(flag) => vec![flag.to_owned(), e.to_owned()],
        EffortArg::ConfigKey(key) => vec!["-c".to_owned(), format!("{key}={e}")],
        // Carried by the model id instead (`agy_effort_model`).
        EffortArg::ModelSuffix => Vec::new(),
    })
}

/// agy and Claude Code share the print-mode flags: `-p <prompt> --output-format text`.
fn print_mode_args(prompt: &str, agent: Option<&str>, model: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-p".to_owned(),
        prompt.to_owned(),
        "--output-format".to_owned(),
        "text".to_owned(),
    ];
    if let Some(a) = agent {
        args.extend(["--agent".to_owned(), a.to_owned()]);
    }
    if let Some(m) = model {
        args.extend(["--model".to_owned(), m.to_owned()]);
    }
    args
}

/// `codex exec` is a checker, never a maker: a read-only sandbox whatever `~/.codex/config.toml`
/// says, no session files, no colour codes in the captured output. Its `-p` is `--profile`, so the
/// prompt goes after `--` (a prompt starting with `-` is never read as a flag). Codex has no
/// personas; a seat's lens goes in the prompt.
fn codex_exec_args(prompt: &str, _agent: Option<&str>, model: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = [
        "exec",
        "--sandbox",
        "read-only",
        "--skip-git-repo-check",
        "--ephemeral",
        "--color",
        "never",
    ]
    .map(str::to_owned)
    .to_vec();
    if let Some(m) = model {
        args.extend(["--model".to_owned(), m.to_owned()]);
    }
    args.extend(["--".to_owned(), prompt.to_owned()]);
    args
}

/// agy only accepts its display names ("Gemini 3.8 Flash (High)"), not `flash`/`pro`. The aliases
/// resolve through `CROSS_AGENT_AGY_FLASH` and `CROSS_AGENT_AGY_PRO`, so a model refresh is an
/// environment change, not a rebuild; anything that is not an alias passes through unchanged.
/// The built-in defaults name display names that may be retired: check `agy models` and override
/// them when they are.
/// A catalogue id (`claude-opus-5-5`, from `cross-agent status`) maps to its display name through
/// agent-terminal's cached catalogue; with no catalogue the value passes through.
fn resolve_agy_model(model: &str) -> String {
    resolve_agy_model_in(model, || {
        status::load(&status::Dirs::from_env().catalogue())
    })
}

/// The catalogue is read only when the value is not an alias.
fn resolve_agy_model_in(model: &str, catalogue: impl FnOnce() -> status::Loaded) -> String {
    match model.trim().to_lowercase().as_str() {
        "flash" => nuvek_core::env::env_or("CROSS_AGENT_AGY_FLASH", "Gemini 3.8 Flash (High)"),
        "pro" => nuvek_core::env::env_or("CROSS_AGENT_AGY_PRO", "Gemini 3.8 Flash (High)"),
        _ => status::agy_display_for_id(&catalogue(), model).unwrap_or_else(|| model.to_string()),
    }
}

fn resolve_identity(model: &str) -> String {
    model.to_string()
}

const AGY: Harness = Harness {
    target: "agy",
    stderr_on_success: true,
    label: "Antigravity",
    rate_limited: is_agy_rate_limited,
    resolve_model: resolve_agy_model,
    args: print_mode_args,
    effort: EffortArg::ModelSuffix,
};
const CLAUDE: Harness = Harness {
    target: "claude",
    stderr_on_success: true,
    label: "Claude Code",
    rate_limited: is_claude_rate_limited,
    resolve_model: resolve_identity,
    args: print_mode_args,
    effort: EffortArg::Flag("--effort"),
};
const CODEX: Harness = Harness {
    target: "codex",
    stderr_on_success: false,
    label: "Codex",
    rate_limited: is_codex_rate_limited,
    resolve_model: resolve_identity,
    args: codex_exec_args,
    effort: EffortArg::ConfigKey("model_reasoning_effort"),
};

/// The audit line for one delegation. Carries the target, persona, model and the prompt's LENGTH,
/// never its text (it may hold a secret); the free-text fields still go through the scrubber.
fn delegation_line(
    h: &Harness,
    agent: Option<&str>,
    model: Option<&str>,
    prompt_len: usize,
    outcome: &str,
) -> String {
    let clean = |v: Option<&str>| nuvek_core::scrub::redact(v.unwrap_or("-"));
    format!(
        "delegate target={} agent={} model={} prompt_chars={prompt_len} {outcome}",
        h.target,
        clean(agent),
        clean(model),
    )
}

/// How the harness is handed its effort, named for the receipt.
fn effort_mechanism(h: &Harness) -> &'static str {
    match h.effort {
        EffortArg::Flag(_) => "flag",
        EffortArg::ConfigKey(_) => "config_override",
        EffortArg::ModelSuffix => "model_id_suffix",
    }
}

/// Everything refused before a spawn: the effort arguments, the model string the CLI will get and
/// the matching receipt `launch_configured` block.
fn prepare(
    h: &Harness,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(Vec<String>, Option<String>, receipt::Launch)> {
    prepare_with(h, model, effort, || {
        status::load(&status::Dirs::from_env().catalogue())
    })
}

/// [`prepare`] with the model catalogue supplied by the caller; it is read only for agy.
fn prepare_with(
    h: &Harness,
    model: Option<&str>,
    effort: Option<&str>,
    catalogue: impl FnOnce() -> status::Loaded,
) -> Result<(Vec<String>, Option<String>, receipt::Launch)> {
    let effort_extra = effort_args(h, effort)?;
    let resolved = match (h.effort, effort) {
        (EffortArg::ModelSuffix, Some(e)) => Some(agy_effort_model(&catalogue(), model, e.trim())?),
        _ => model.map(h.resolve_model),
    };
    let launch = receipt::Launch {
        model: resolved.clone(),
        effort: effort.map(|e| (effort_mechanism(h), e.trim().to_owned())),
    };
    Ok((effort_extra, resolved, launch))
}

/// `--json`: the one receipt line on stdout.
#[allow(clippy::too_many_arguments)]
fn print_receipt(
    meta: &receipt::Meta,
    launch: Option<&receipt::Launch>,
    kind: receipt::Kind,
    code: i32,
    secs: f64,
    stdout: &str,
    stderr: &str,
    error: Option<&str>,
) {
    let r = receipt::build(meta, launch, kind, code, secs, stdout, stderr, error);
    println!("{r}");
}

fn run_harness(
    h: &Harness,
    agent: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    timeout: u64,
    d: Delegate,
) -> Result<i32> {
    let json = d.json;
    let mut meta = receipt::Meta {
        harness: h.target,
        agent: agent.as_deref(),
        task_id: d.task_id.as_deref(),
        review_id: d.review_id.as_deref(),
        requested_model: model.as_deref(),
        requested_effort: effort.as_deref(),
        prompt_sha256: None,
        prompt_chars: None,
    };
    // Refused before anything is recorded or spawned. In text mode the error propagates as it
    // always did (exit 1); with --json it becomes a receipt with the same exit code.
    let refuse = |meta: &receipt::Meta, e: anyhow::Error| -> Result<i32> {
        if json {
            let msg = format!("{e:#}");
            print_receipt(
                meta,
                None,
                receipt::Kind::InvalidInput,
                1,
                0.0,
                "",
                "",
                Some(&msg),
            );
            Ok(1)
        } else {
            Err(e)
        }
    };
    let prompt =
        match input::resolve_prompt(d.prompt.clone(), d.prompt_file.as_deref(), std::io::stdin()) {
            Ok(p) => p,
            Err(e) => return refuse(&meta, e),
        };
    meta.prompt_sha256 = Some(nuvek_core::digest::sha256_hex(&prompt));
    meta.prompt_chars = Some(prompt.chars().count());
    let (effort_extra, resolved, launch) = match prepare(h, model.as_deref(), effort.as_deref()) {
        Ok(p) => p,
        Err(e) => return refuse(&meta, e),
    };
    let log = nuvek_core::log::get_logger("cross-agent");
    let prompt_len = prompt.chars().count();
    let line =
        |outcome: &str| delegation_line(h, agent.as_deref(), model.as_deref(), prompt_len, outcome);

    record_delegation(h.target, &prompt, &meta);
    log.info(&line("outcome=start"));
    let started = Instant::now();

    let mut cmd = Command::new(h.target);
    cmd.args(with_effort(
        (h.args)(&prompt, agent.as_deref(), resolved.as_deref()),
        effort_extra,
    ));
    // codex reads stdin when it is not a terminal; give it nothing to wait on.
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let dur = Duration::from_secs(timeout);
    let output = match command_output_timeout(cmd, dur) {
        Ok(o) => o,
        Err(e) => {
            let secs = started.elapsed().as_secs_f64();
            log.error(&line(&format!("outcome=spawn_failed duration_s={secs:.1}")));
            if json {
                let msg = format!("failed to execute '{}' CLI binary: {e}", h.target);
                let kind = receipt::Kind::SpawnFailed;
                print_receipt(&meta, Some(&launch), kind, 1, secs, "", "", Some(&msg));
                return Ok(1);
            }
            return Err(e).with_context(|| format!("failed to execute '{}' CLI binary", h.target));
        }
    };

    let Some(out) = output else {
        let secs = started.elapsed().as_secs_f64();
        log.warn(&line(&format!(
            "outcome=timeout exit=124 duration_s={secs:.1}"
        )));
        if json {
            let msg = format!("{} execution timed out after {timeout}s", h.label);
            let kind = receipt::Kind::Timeout;
            print_receipt(&meta, Some(&launch), kind, 124, secs, "", "", Some(&msg));
        } else {
            eprintln!(
                "cross-agent: error: {} execution timed out after {timeout}s",
                h.label
            );
        }
        return Ok(124);
    };

    let stdout_str = String::from_utf8_lossy(&out.stdout);
    let stderr_str = String::from_utf8_lossy(&out.stderr);
    let secs = started.elapsed().as_secs_f64();
    // A receipt carries the prompt's hash only: drop any echo of it (codex prints its input).
    let r_out = receipt::omit_prompt(&stdout_str, &prompt);
    let r_err = receipt::omit_prompt(&stderr_str, &prompt);
    let combined = format!("{r_out}\n{r_err}");

    let code = exit_code(&out.status);
    if is_rate_limited_run(h, code, &combined) {
        log.warn(&line(&format!(
            "outcome=rate_limited exit=75 duration_s={secs:.1}"
        )));
        if json {
            let msg = format!("{} is rate-limited or at session limit", h.label);
            let kind = receipt::Kind::RateLimited;
            print_receipt(
                &meta,
                Some(&launch),
                kind,
                75,
                secs,
                &r_out,
                &r_err,
                Some(&msg),
            );
        } else {
            eprintln!(
                "cross-agent: error: {} is currently rate-limited or at session limit (EX_TEMPFAIL):",
                h.label
            );
            eprint!("{combined}");
        }
        return Ok(75);
    }

    if json {
        let kind = failure_kind(code, &combined);
        // Same visibility rule as text mode: a successful codex run's stderr is its transcript.
        let err = if code != 0 || h.stderr_on_success {
            r_err.as_str()
        } else {
            ""
        };
        print_receipt(&meta, Some(&launch), kind, code, secs, &r_out, err, None);
    } else {
        if (code != 0 || h.stderr_on_success) && !stderr_str.trim().is_empty() {
            eprint!("{stderr_str}");
        }
        if !stdout_str.is_empty() {
            print!("{stdout_str}");
        }
    }

    let outcome = format!("outcome=done exit={code} duration_s={secs:.1}");
    if code == 0 {
        log.info(&line(&outcome));
    } else {
        log.error(&line(&outcome));
    }
    Ok(code)
}

/// Only classify explicit diagnostics on failed runs; successful reviews may discuss failures.
fn failure_kind(code: i32, output: &str) -> receipt::Kind {
    if code == 0 {
        return receipt::Kind::Success;
    }
    let text = output.to_ascii_lowercase();
    if text.contains("permission denied") || text.contains("tool execution was denied") {
        receipt::Kind::PermissionDenied
    } else if text.contains("mcp bridge failed") || text.contains("bridge connection failed") {
        receipt::Kind::BridgeFailed
    } else {
        receipt::Kind::Failed
    }
}

/// Inserts the effort arguments before the `--` that ends the options (Codex): after it they would
/// be read as part of the prompt. `-p <prompt>` (claude, agy) has no separator, and a prompt that
/// is itself `--` is `-p`'s value, never one: the effort is appended there.
fn with_effort(mut args: Vec<String>, extra: Vec<String>) -> Vec<String> {
    let at = if args.first().is_some_and(|a| a == "-p") {
        args.len()
    } else {
        args.iter().position(|a| a == "--").unwrap_or(args.len())
    };
    args.splice(at..at, extra);
    args
}

/// `status`: reads three cache/config files; spawns nothing and writes nothing.
fn print_status(json: bool) -> Result<()> {
    let dirs = status::Dirs::from_env();
    let s = status::build(
        &dirs,
        &status::load(&dirs.tags()),
        &status::load(&dirs.usage()),
        &status::load(&dirs.catalogue()),
        status::now_s(),
    );
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
    } else {
        print!("{}", status::render_table(&s));
    }
    Ok(())
}

fn main() -> Result<()> {
    // First, before any text is scanned: refuse to run with a secret-surface file that is
    // unusable. The error names the file and the entry, never a secret value.
    if let Err(e) = nuvek_core::scrub::init() {
        eprintln!("cross-agent: secret-surface configuration refused: {e}");
        std::process::exit(EXIT_CONFIG);
    }
    let cli = Cli::parse();

    let code = match cli.command {
        Commands::Agy {
            agent,
            model,
            effort,
            timeout,
            delegate,
        } => run_harness(&AGY, agent, model, effort, timeout, delegate)?,
        Commands::Claude {
            agent,
            model,
            effort,
            timeout,
            delegate,
        } => run_harness(&CLAUDE, agent, model, effort, timeout, delegate)?,
        Commands::Codex {
            model,
            effort,
            timeout,
            delegate,
        } => run_harness(&CODEX, None, model, effort, timeout, delegate)?,
        Commands::Status { json } => {
            print_status(json)?;
            0
        }
        Commands::Select {
            tags,
            tier,
            effort,
            from,
            json,
        } => {
            let req = select::Request::new(&tags, &tier, effort.as_deref(), from.as_deref())?;
            select::run(json, &req)?
        }
    };

    if code != 0 {
        std::process::exit(code);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_classification_preserves_success_and_unknown_errors() {
        assert_eq!(
            failure_kind(1, "Permission denied"),
            receipt::Kind::PermissionDenied
        );
        assert_eq!(
            failure_kind(1, "MCP bridge failed"),
            receipt::Kind::BridgeFailed
        );
        assert_eq!(
            failure_kind(1, "unrecognised failure"),
            receipt::Kind::Failed
        );
        assert_eq!(
            failure_kind(0, "review permission denied handling"),
            receipt::Kind::Success
        );
    }

    fn prompt_only(p: &str) -> Delegate {
        Delegate {
            prompt: Some(p.into()),
            prompt_file: None,
            task_id: None,
            review_id: None,
            json: false,
        }
    }

    #[test]
    fn prompt_sources_and_labels_parse() {
        let base = ["cross-agent", "claude"];
        let parse = |extra: &[&str]| Cli::try_parse_from(base.iter().chain(extra.iter()).copied());
        // The old text interface: a positional prompt, nothing else needed.
        assert!(parse(&["hi"]).is_ok());
        // Exactly one prompt source.
        assert!(parse(&[]).is_err());
        assert!(parse(&["hi", "--prompt-file", "/tmp/p"]).is_err());
        let Commands::Claude { delegate, .. } = parse(&[
            "--prompt-file",
            "-",
            "--task-id",
            "t-1",
            "--review-id",
            "r:2",
            "--json",
        ])
        .unwrap()
        .command
        else {
            panic!("not claude");
        };
        assert_eq!(delegate.prompt, None);
        assert_eq!(delegate.prompt_file, Some(PathBuf::from("-")));
        assert_eq!(delegate.task_id.as_deref(), Some("t-1"));
        assert_eq!(delegate.review_id.as_deref(), Some("r:2"));
        assert!(delegate.json);
        // Labels are validated at parse time.
        assert!(parse(&["hi", "--task-id", "has space"]).is_err());
        assert!(parse(&["hi", "--review-id", ""]).is_err());
    }

    #[test]
    fn select_parses_and_validates_arguments() {
        let parse = |a: &[&str]| {
            Cli::try_parse_from(["cross-agent", "select"].iter().chain(a.iter()).copied())
        };
        let Commands::Select {
            tags, tier, from, ..
        } = parse(&[
            "--tag",
            "code,review",
            "--tag",
            "x",
            "--tier",
            "deep",
            "--from",
            "agy",
        ])
        .unwrap()
        .command
        else {
            panic!("not select");
        };
        assert_eq!(tags, ["code", "review", "x"]);
        assert_eq!(tier, "deep");
        assert_eq!(from.as_deref(), Some("agy"));
        assert!(parse(&["--tier", "deep"]).is_err(), "tag is required");
        assert!(parse(&["--tag", "code"]).is_err(), "tier is required");
        assert!(parse(&["--tag", "code", "--tier", "huge"]).is_err());
        // A positional prompt is not a select argument: it never spawns anything.
        assert!(parse(&["--tag", "code", "--tier", "deep", "hello"]).is_err());
    }

    #[test]
    fn prepare_reports_what_the_launch_is_configured_with() {
        // claude: the effort is a flag; the requested alias is passed through unchanged.
        let (extra, model, launch) = prepare(&CLAUDE, Some("opus"), Some(" high ")).unwrap();
        assert_eq!(extra, ["--effort", "high"]);
        assert_eq!(model.as_deref(), Some("opus"));
        assert_eq!(launch.model.as_deref(), Some("opus"));
        assert_eq!(launch.effort, Some(("flag", "high".to_owned())));
        // codex: a config override, named as such.
        let (_, _, launch) = prepare(&CODEX, None, Some("low")).unwrap();
        assert_eq!(launch.model, None);
        assert_eq!(launch.effort, Some(("config_override", "low".to_owned())));
        // No effort asked, none claimed.
        let (extra, _, launch) = prepare(&CLAUDE, None, None).unwrap();
        assert!(extra.is_empty() && launch.effort.is_none());
    }

    #[test]
    fn unsupported_effort_is_refused_before_a_spawn() {
        // A fixture catalogue stands in for the real one; the closure panics if a non-agy
        // harness ever reads it.
        let cat = || {
            status::Loaded::Ok(serde_json::json!({"agy": [
                {"driver": "agy", "id": "gemini-4-argon", "display": "Gemini 4 Argon",
                 "efforts": ["low", "high"]}]}))
        };
        let unread = || -> status::Loaded { unreachable!("catalogue read") };
        assert!(prepare_with(&CLAUDE, None, Some("HIGH"), unread).is_err());
        assert!(prepare_with(&CODEX, None, Some("high,model=x"), unread).is_err());
        // agy carries the effort in the model id, so it needs one.
        assert!(prepare_with(&AGY, None, Some("high"), cat).is_err());
        // An unknown id, an effort the model does not offer, and no catalogue are all refused.
        assert!(prepare_with(&AGY, Some("nope"), Some("high"), cat).is_err());
        assert!(prepare_with(&AGY, Some("gemini-4-argon"), Some("max"), cat).is_err());
        assert!(
            prepare_with(&AGY, Some("gemini-4-argon"), Some("high"), || {
                status::Loaded::Missing
            })
            .is_err()
        );
        let (_, model, _) = prepare_with(&AGY, Some("gemini-4-argon"), Some("high"), cat).unwrap();
        assert_eq!(model.as_deref(), Some("gemini-4-argon-high"));
    }

    #[test]
    fn refused_input_under_json_is_a_receipt_with_the_old_exit_code() {
        // run_harness checks the prompt first: an empty prompt never reaches a spawn.
        let d = Delegate {
            prompt: Some("   ".into()),
            ..prompt_only("")
        };
        let d = Delegate { json: true, ..d };
        assert_eq!(run_harness(&CLAUDE, None, None, None, 1, d).unwrap(), 1);
        // When prompt resolution succeeds and prepare fails, --json still emits an invalid_input receipt.
        let d = Delegate {
            json: true,
            ..prompt_only("hi")
        };
        assert_eq!(
            run_harness(&CLAUDE, None, None, Some("HIGH".into()), 1, d).unwrap(),
            1
        );
        // The same refusal without --json is an error (main exits 1), exactly as before.
        let d = Delegate {
            prompt: Some("   ".into()),
            ..prompt_only("")
        };
        assert!(run_harness(&CLAUDE, None, None, None, 1, d).is_err());
    }

    #[test]
    fn parse_agy_command() {
        let args = [
            "cross-agent",
            "agy",
            "-a",
            "network-checker",
            "-t",
            "60",
            "check router",
        ];
        let cli = Cli::parse_from(args);
        assert_eq!(
            cli.command,
            Commands::Agy {
                agent: Some("network-checker".into()),
                model: None,
                effort: None,
                timeout: 60,
                delegate: prompt_only("check router"),
            }
        );
    }

    #[test]
    fn parse_claude_command() {
        let args = ["cross-agent", "claude", "-a", "code-reviewer", "audit diff"];
        let cli = Cli::parse_from(args);
        assert_eq!(
            cli.command,
            Commands::Claude {
                agent: Some("code-reviewer".into()),
                model: None,
                effort: None,
                timeout: 180,
                delegate: prompt_only("audit diff"),
            }
        );
    }

    #[test]
    fn parse_codex_command() {
        let args = [
            "cross-agent",
            "codex",
            "-m",
            "gpt-6-sol",
            "-t",
            "300",
            "review",
        ];
        let cli = Cli::parse_from(args);
        assert_eq!(
            cli.command,
            Commands::Codex {
                model: Some("gpt-6-sol".into()),
                effort: None,
                timeout: 300,
                delegate: prompt_only("review"),
            }
        );
        // Codex has no personas, so `-a` is refused rather than silently dropped.
        assert!(Cli::try_parse_from(["cross-agent", "codex", "-a", "red-team", "x"]).is_err());
    }

    #[test]
    fn codex_runs_read_only_and_never_parses_the_prompt_as_a_flag() {
        let args = codex_exec_args("--sandbox danger-full-access", Some("ignored"), Some("m"));
        assert_eq!(args[0], "exec");
        let sandbox = args
            .iter()
            .position(|a| a == "--sandbox")
            .map(|i| &args[i + 1]);
        assert_eq!(sandbox.map(String::as_str), Some("read-only"));
        assert!(args.contains(&"--ephemeral".to_owned()));
        assert!(
            !args.contains(&"-p".to_owned()),
            "codex -p is --profile: {args:?}"
        );
        assert!(!args.contains(&"ignored".to_owned()), "{args:?}");
        // The prompt is the last argument, after the `--` terminator.
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args[args.len() - 1], "--sandbox danger-full-access");
    }

    #[test]
    fn print_mode_args_keep_the_existing_shape() {
        assert_eq!(
            print_mode_args("hi", Some("code-reviewer"), Some("opus")),
            [
                "-p",
                "hi",
                "--output-format",
                "text",
                "--agent",
                "code-reviewer",
                "--model",
                "opus"
            ]
        );
    }

    #[test]
    fn detect_codex_rate_limit() {
        for hit in [
            "ERROR: You've hit your usage limit. Try again in 2 hours.",
            "stream error: usage_limit_reached",
            "unexpected status 429 Too Many Requests",
        ] {
            assert!(is_codex_rate_limited(hit), "{hit}");
        }
        for miss in ["reviewed 429 lines, no findings", "All tests passed.", ""] {
            assert!(!is_codex_rate_limited(miss), "{miss}");
        }
        // The echoed prompt talks about limits, but the failure is something else.
        let echoed = "user\nreview the rate limit and usage limit code\nERROR: auth failed";
        assert!(!is_rate_limited_run(&CODEX, 1, echoed));
        assert!(!is_rate_limited_run(&CODEX, 0, "usage_limit_reached"));
        assert!(is_rate_limited_run(&CODEX, 1, "usage_limit_reached"));
    }

    #[test]
    fn detect_rate_limit() {
        let sample = "Permission allow rule ...\nYou've hit your session limit · resets 10:40am (Europe/Paris)";
        assert!(is_claude_rate_limited(sample));
        assert!(!is_claude_rate_limited("All tests passed cleanly."));
    }

    #[test]
    fn detect_agy_rate_limit() {
        for hit in [
            "Error: 8 RESOURCE_EXHAUSTED: quota exceeded",
            "Quota exhausted. Resets in 3h12m",
            "request failed: 429 Too Many Requests",
        ] {
            assert!(is_agy_rate_limited(hit), "{hit}");
        }
        for miss in [
            "All 429 tests passed cleanly.",
            "reviewed 429 lines, no findings",
            "error handling in 429 lines",
            "",
        ] {
            assert!(!is_agy_rate_limited(miss), "{miss}");
        }
    }

    #[test]
    fn rate_limit_applies_only_to_failed_runs() {
        let text = "The client should back off on a rate limit; RESOURCE_EXHAUSTED means quota.";
        // Success that merely discusses rate limits is passed through, for both harnesses.
        assert!(!is_rate_limited_run(&AGY, 0, text));
        assert!(!is_rate_limited_run(&CLAUDE, 0, text));
        // The same text on a failed run is a rate limit.
        assert!(is_rate_limited_run(&AGY, 1, text));
        assert!(is_rate_limited_run(&CLAUDE, 1, text));
    }

    #[test]
    fn agy_model_aliases_resolve_and_full_names_pass_through() {
        // Env overrides are unset in the test environment, so the built-in defaults apply. No
        // live catalogue: a fixture stands in for it.
        let none = || status::Loaded::Missing;
        assert!(resolve_agy_model_in("pro", none).starts_with("Gemini"));
        assert!(resolve_agy_model_in("FLASH", none).contains("Flash"));
        assert_eq!(
            resolve_agy_model_in("Gemini 3.6 Flash (Low)", none),
            "Gemini 3.6 Flash (Low)"
        );
        assert_eq!(resolve_identity("sonnet"), "sonnet");
    }

    #[test]
    fn agy_resolver_accepts_catalogue_ids_and_keeps_the_rest() {
        let cat = || {
            status::Loaded::Ok(serde_json::json!({"agy": [
            {"driver": "agy", "id": "claude-opus-5-5", "display": "Claude Opus 5.5 (Thinking)"}]}))
        };
        assert_eq!(
            resolve_agy_model_in("claude-opus-5-5", cat),
            "Claude Opus 5.5 (Thinking)"
        );
        // An alias never reads the catalogue.
        assert!(resolve_agy_model_in("flash", || unreachable!("catalogue read")).contains("Flash"));
        assert_eq!(
            resolve_agy_model_in("Gemini 3.6 Flash (Low)", cat),
            "Gemini 3.6 Flash (Low)"
        );
        // No catalogue: ids pass through unchanged.
        assert_eq!(
            resolve_agy_model_in("claude-opus-5-5", || status::Loaded::Missing),
            "claude-opus-5-5"
        );
    }

    #[test]
    fn effort_reaches_every_harness_before_the_prompt_separator() {
        let cli = Cli::parse_from(["cross-agent", "claude", "-e", "high", "-m", "opus", "hi"]);
        assert!(matches!(
            cli.command,
            Commands::Claude { effort: Some(ref e), .. } if e == "high"
        ));
        // agy: the effort rides in the model id, never as a flag.
        assert!(effort_args(&AGY, Some("high")).unwrap().is_empty());
        let cat = status::Loaded::Ok(serde_json::json!({"agy": [
            {"driver": "agy", "id": "gemini-4-argon", "display": "Gemini 4 Argon",
             "efforts": ["low", "medium", "high"]}]}));
        assert_eq!(
            agy_effort_model(&cat, Some("Gemini-4-Argon"), "high").unwrap(),
            "gemini-4-argon-high"
        );
        assert!(agy_effort_model(&cat, Some("gemini-4-argon"), "xhigh").is_err());
        assert!(agy_effort_model(&cat, Some("Gemini 4 Argon"), "high").is_err());
        assert!(agy_effort_model(&cat, None, "high").is_err());
        assert!(
            agy_effort_model(&status::Loaded::Missing, Some("gemini-4-argon"), "high").is_err()
        );
        {
            let h = &CLAUDE;
            let extra = effort_args(h, Some("xhigh")).unwrap();
            assert_eq!(extra, ["--effort", "xhigh"]);
            let full = with_effort(print_mode_args("p", None, Some("m")), extra);
            // `-p <prompt>` has no `--`: the flag is appended, the prompt stays `-p`'s value.
            assert_eq!(&full[full.len() - 2..], ["--effort", "xhigh"]);
            assert_eq!(&full[..2], ["-p", "p"]);
            // A prompt that is literally `--` stays `-p`'s value.
            let full = with_effort(print_mode_args("--", None, None), vec!["--effort".into()]);
            assert_eq!(&full[..2], ["-p", "--"]);
        }
        let extra = effort_args(&CODEX, Some("high")).unwrap();
        assert_eq!(extra, ["-c", "model_reasoning_effort=high"]);
        let full = with_effort(codex_exec_args("p", None, None), extra);
        let sep = full.iter().position(|a| a == "--").unwrap();
        assert_eq!(&full[sep - 2..sep], ["-c", "model_reasoning_effort=high"]);
        assert!(effort_args(&CLAUDE, None).unwrap().is_empty());
        assert!(effort_args(&CLAUDE, Some("  ")).is_err());
        // Nothing but a bare word reaches a `-c key=value` override.
        for bad in ["high,model=x", "high\nx", "HIGH", "hi gh", "x=y"] {
            assert!(effort_args(&CODEX, Some(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ledger_directory_resolution_order() {
        let home = PathBuf::from("/tmp/example-home");
        let dir = |explicit: Option<&str>, xdg: Option<&str>| {
            ledger_dir_from(
                explicit.map(String::from),
                xdg.map(String::from),
                home.clone(),
            )
        };
        assert_eq!(
            dir(Some("/var/lib/ledger"), Some("/tmp/state")),
            PathBuf::from("/var/lib/ledger")
        );
        assert_eq!(
            dir(None, Some("/tmp/state")),
            PathBuf::from("/tmp/state/cross-agent")
        );
        // A relative XDG_STATE_HOME is not a location.
        assert_eq!(
            dir(None, Some("state")),
            PathBuf::from("/tmp/example-home/.local/state/cross-agent")
        );
        assert_eq!(
            dir(None, None),
            PathBuf::from("/tmp/example-home/.local/state/cross-agent")
        );
    }

    #[test]
    fn status_parses_and_takes_no_prompt() {
        let cli = Cli::parse_from(["cross-agent", "status", "--json"]);
        assert_eq!(cli.command, Commands::Status { json: true });
    }

    #[test]
    fn delegation_line_never_carries_the_prompt() {
        // The line takes only a length, so a prompt cannot reach it; assert the shape anyway.
        let sentinel = "SENTINEL-PROMPT-TEXT";
        let l = delegation_line(
            &AGY,
            Some("network-checker"),
            None,
            sentinel.chars().count(),
            "outcome=start",
        );
        assert!(!l.contains(sentinel), "{l}");
        assert!(l.contains("prompt_chars=20"), "{l}");
        assert!(l.contains("agent=network-checker"), "{l}");
    }
}
