//! `--json`: the one structured result of a delegation.
//!
//! It records what was REQUESTED, what the launch was CONFIGURED with, and what happened. It never
//! claims what model actually answered (no harness reports that to us), never claims a resumed
//! session (every run is a cold review). Prompt metadata is a hash; verbatim echoes in output are
//! removed where identifiable, but output is still potentially sensitive review content.

use serde_json::{json, Value};

/// Per-stream cap on captured output kept in a receipt (characters, after scrubbing).
pub const MAX_STREAM_CHARS: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Success,
    /// The harness ran and exited non-zero.
    Failed,
    /// Exit 75: a failed run whose output reads as a quota / rate limit.
    RateLimited,
    /// Exit 124: the wall-clock bound was hit.
    Timeout,
    /// The harness binary could not be started.
    SpawnFailed,
    /// Refused before anything was spawned (empty prompt, unreadable file, unsupported effort...).
    InvalidInput,
    PermissionDenied,
    BridgeFailed,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Success => "success",
            Kind::Failed => "failed",
            Kind::RateLimited => "rate_limited",
            Kind::Timeout => "timeout",
            Kind::SpawnFailed => "spawn_failed",
            Kind::InvalidInput => "invalid_input",
            Kind::PermissionDenied => "permission_denied",
            Kind::BridgeFailed => "bridge_failed",
        }
    }
}

/// What the caller asked for and how the run was labelled.
pub struct Meta<'a> {
    pub harness: &'a str,
    pub agent: Option<&'a str>,
    pub task_id: Option<&'a str>,
    pub review_id: Option<&'a str>,
    pub requested_model: Option<&'a str>,
    pub requested_effort: Option<&'a str>,
    /// `None` when the prompt could not be resolved.
    pub prompt_sha256: Option<String>,
    pub prompt_chars: Option<usize>,
}

/// What was actually put on the command line.
pub struct Launch {
    /// The `--model` value handed to the CLI; `None` = the harness default was used.
    pub model: Option<String>,
    /// `(mechanism, value)` of the effort setting handed to the CLI.
    pub effort: Option<(&'static str, String)>,
}

/// Redact secrets, then bound the length by characters (never bytes). Redacting first means a
/// secret cut by the bound cannot slip past the scrubber half-formed.
pub fn clean_stream(raw: &str) -> (String, bool) {
    let red = nuvek_core::scrub::redact_config(raw);
    if red.chars().count() <= MAX_STREAM_CHARS {
        (red, false)
    } else {
        (red.chars().take(MAX_STREAM_CHARS).collect(), true)
    }
}

/// Remove verbatim copies of the prompt from captured output (a harness may echo its input), so a
/// prompt metadata can remain hash-only. Short prompts are left to avoid mangling ordinary output;
/// this is an echo filter, not an information-flow guarantee.
pub fn omit_prompt(text: &str, prompt: &str) -> String {
    let p = prompt.trim();
    if p.chars().count() < 8 {
        return text.to_owned();
    }
    text.replace(prompt, "[prompt omitted]")
        .replace(p, "[prompt omitted]")
}

/// Assemble the receipt. Pure.
#[allow(clippy::too_many_arguments)]
pub fn build(
    meta: &Meta,
    launch: Option<&Launch>,
    kind: Kind,
    exit_code: i32,
    duration_s: f64,
    stdout: &str,
    stderr: &str,
    error: Option<&str>,
) -> Value {
    let (out, out_cut) = clean_stream(stdout);
    let (err, err_cut) = clean_stream(stderr);
    let clean = |s: Option<&str>| s.map(|s| clean_stream(s).0);
    let launched = launch.map(|l| {
        json!({
            "model": clean(l.model.as_deref()),
            "model_source": if l.model.is_some() { "cli_argument" } else { "harness_default" },
            "effort": clean(l.effort.as_ref().map(|(_, v)| v.as_str())),
            "effort_mechanism": l.effort.as_ref().map(|(m, _)| m),
        })
    });
    json!({
        "schema": 1,
        "success": kind == Kind::Success,
        "kind": kind.as_str(),
        "exit_code": exit_code,
        "duration_s": (duration_s * 1000.0).round() / 1000.0,
        "harness": meta.harness,
        "agent": clean(meta.agent),
        "task_id": clean(meta.task_id),
        "review_id": clean(meta.review_id),
        // Every delegation is a fresh headless session; nothing is ever resumed.
        "cold_review": true,
        "prompt": {"sha256": meta.prompt_sha256, "chars": meta.prompt_chars},
        "requested": {"model": clean(meta.requested_model), "effort": clean(meta.requested_effort)},
        "launch_configured": launched,
        // The harness does not tell us which model answered; this is deliberately not guessed.
        "model_identity": {"verified": false, "provenance": "launch_configuration_only"},
        "error": clean(error),
        "stdout": out,
        "stdout_truncated": out_cut,
        "stderr": err,
        "stderr_truncated": err_cut,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Meta<'static> {
        Meta {
            harness: "claude",
            agent: Some("code-reviewer"),
            task_id: Some("t-1"),
            review_id: None,
            requested_model: Some("opus"),
            requested_effort: Some("high"),
            prompt_sha256: Some(nuvek_core::digest::sha256_hex("SECRET-BRIEF")),
            prompt_chars: Some(12),
        }
    }

    fn launch() -> Launch {
        Launch {
            model: Some("opus".into()),
            effort: Some(("flag", "high".into())),
        }
    }

    #[test]
    fn receipt_is_honest_about_cold_review_and_identity() {
        let r = build(
            &meta(),
            Some(&launch()),
            Kind::Success,
            0,
            1.23456,
            "ok",
            "",
            None,
        );
        assert_eq!(r["cold_review"], true);
        assert_eq!(r["success"], true);
        assert_eq!(r["kind"], "success");
        assert_eq!(r["exit_code"], 0);
        assert_eq!(r["duration_s"], 1.235);
        assert_eq!(r["model_identity"]["verified"], false);
        assert_eq!(r["requested"]["model"], "opus");
        assert_eq!(r["launch_configured"]["model_source"], "cli_argument");
        assert_eq!(r["launch_configured"]["effort_mechanism"], "flag");
        assert_eq!(r["task_id"], "t-1");
        assert!(r["review_id"].is_null());
        for forbidden in [
            "resumed",
            "resume",
            "session_id",
            "answered_by",
            "actual_model",
        ] {
            assert!(r.get(forbidden).is_none(), "{forbidden}");
        }
    }

    #[test]
    fn prompt_appears_only_as_a_hash() {
        let r = build(
            &meta(),
            Some(&launch()),
            Kind::Success,
            0,
            0.0,
            "",
            "",
            None,
        );
        let text = r.to_string();
        assert!(!text.contains("SECRET-BRIEF"));
        assert_eq!(
            r["prompt"]["sha256"],
            nuvek_core::digest::sha256_hex("SECRET-BRIEF")
        );
        assert_eq!(r["prompt"]["chars"], 12);
    }

    #[test]
    fn default_model_is_reported_as_default_not_invented() {
        let l = Launch {
            model: None,
            effort: None,
        };
        let r = build(&meta(), Some(&l), Kind::Success, 0, 0.0, "", "", None);
        assert!(r["launch_configured"]["model"].is_null());
        assert_eq!(r["launch_configured"]["model_source"], "harness_default");
        assert!(r["launch_configured"]["effort"].is_null());
    }

    #[test]
    fn failures_carry_kind_and_exit_code() {
        for (kind, code, name) in [
            (Kind::Failed, 3, "failed"),
            (Kind::RateLimited, 75, "rate_limited"),
            (Kind::Timeout, 124, "timeout"),
            (Kind::SpawnFailed, 1, "spawn_failed"),
        ] {
            let r = build(
                &meta(),
                Some(&launch()),
                kind,
                code,
                2.0,
                "",
                "boom",
                Some("why"),
            );
            assert_eq!(r["success"], false);
            assert_eq!(r["kind"], name);
            assert_eq!(r["exit_code"], code);
            assert_eq!(r["error"], "why");
        }
    }

    #[test]
    fn invalid_input_has_no_launch_and_no_prompt_hash() {
        let mut m = meta();
        m.prompt_sha256 = None;
        m.prompt_chars = None;
        let r = build(
            &m,
            None,
            Kind::InvalidInput,
            1,
            0.0,
            "",
            "",
            Some("prompt cannot be empty"),
        );
        assert_eq!(r["kind"], "invalid_input");
        assert!(r["launch_configured"].is_null());
        assert!(r["prompt"]["sha256"].is_null());
        assert_eq!(r["cold_review"], true);
    }

    #[test]
    fn echoed_prompt_is_omitted_from_output() {
        let prompt = "review the whole diff carefully\nand report";
        let echoed = format!("user\n{prompt}\nERROR: boom");
        let cleaned = omit_prompt(&echoed, prompt);
        assert!(!cleaned.contains("review the whole diff"), "{cleaned}");
        assert!(cleaned.contains("ERROR: boom"));
        // A trailing newline on the prompt does not defeat the match.
        let cleaned = omit_prompt(&format!("x {prompt} y"), &format!("{prompt}\n"));
        assert!(!cleaned.contains("whole diff"), "{cleaned}");
        // Tiny prompts are not erased from the output.
        assert_eq!(omit_prompt("ok ok", "ok"), "ok ok");
    }

    #[test]
    fn streams_are_scrubbed_and_bounded_by_characters() {
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        let mut metadata = meta();
        metadata.task_id = Some(&token);
        metadata.requested_model = Some(&token);
        let r = build(
            &metadata,
            Some(&launch()),
            Kind::Failed,
            1,
            0.0,
            &format!("out {token}"),
            &format!("err {token}"),
            Some(&format!("e {token}")),
        );
        assert!(!r.to_string().contains(&token), "{r}");
        // Multi-byte text longer than the bound is cut on a character boundary.
        let long = "é".repeat(MAX_STREAM_CHARS + 5);
        let (cut, flag) = clean_stream(&long);
        assert!(flag);
        assert_eq!(cut.chars().count(), MAX_STREAM_CHARS);
        let (short, flag) = clean_stream("fine");
        assert!(!flag);
        assert_eq!(short, "fine");
    }
}
