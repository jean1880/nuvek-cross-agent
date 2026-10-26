//! `cross-agent select`: a read-only, capability-aware pick of ONE model for a task.
//!
//! It filters and ranks the document [`status::build`] already assembles from the tag projection,
//! the usage cache and the model catalogue. It spawns nothing, writes nothing, schedules nothing,
//! retries nothing and never widens a permission: it only names a candidate and the arguments to
//! launch it with.
//!
//! A model is rejected unless its tier equals the task tier, it carries every required tag, it
//! supports the effort (the requested one, else the policy's `effort_for[tier]`), and its harness
//! has `ok` or `unknown` headroom (`low` / `exhausted` are out). Survivors rank trusted readings
//! first by lowest load; an unknown or stale reading ranks after every trusted one. Ties prefer a
//! harness other than `--from`.

use crate::status::{self, DRIVERS};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::cmp::Ordering;

pub const TIERS: [&str; 3] = ["fast", "standard", "deep"];
/// Exit status when nothing qualifies (the shared "not found" code).
pub const EXIT_NONE: i32 = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// Required capability tags (lowercase, de-duplicated, never empty).
    pub tags: Vec<String>,
    pub tier: String,
    /// An effort the caller insists on; `None` = use the policy's `effort_for[tier]`.
    pub effort: Option<String>,
    /// The caller's own harness: on a tie another harness is preferred.
    pub from: Option<String>,
}

impl Request {
    pub fn new(
        tags: &[String],
        tier: &str,
        effort: Option<&str>,
        from: Option<&str>,
    ) -> Result<Request> {
        let mut t: Vec<String> = Vec::new();
        for raw in tags {
            let tag = raw.trim().to_lowercase();
            if tag.is_empty()
                || !tag
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!("invalid tag {raw:?}: use letters, digits, - and _");
            }
            if !t.contains(&tag) {
                t.push(tag);
            }
        }
        if t.is_empty() {
            bail!("at least one --tag is required");
        }
        let tier = tier.trim().to_lowercase();
        if !TIERS.contains(&tier.as_str()) {
            bail!("--tier must be one of {TIERS:?} (got {tier:?})");
        }
        let effort = match effort.map(str::trim) {
            None => None,
            Some(e) if !e.is_empty() && e.chars().all(|c| c.is_ascii_lowercase()) => {
                Some(e.to_owned())
            }
            Some(e) => {
                bail!("--effort must be a lowercase word such as low|medium|high (got {e:?})")
            }
        };
        let from = match from.map(|f| f.trim().to_lowercase()) {
            None => None,
            Some(f) if DRIVERS.contains(&f.as_str()) => Some(f),
            Some(f) => bail!("--from must be one of {DRIVERS:?} (got {f:?})"),
        };
        Ok(Request {
            tags: t,
            tier,
            effort,
            from,
        })
    }
}

struct Candidate {
    index: usize,
    driver: &'static str,
    load: Option<f64>,
    row: Value,
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Order two candidates: trusted readings before unknown, lower load first, then another harness
/// than the caller's, then catalogue order.
fn order(a: &Candidate, b: &Candidate, from: Option<&str>) -> Ordering {
    let pref = |c: &Candidate| u8::from(from == Some(c.driver));
    let by_load = match (a.load, b.load) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    by_load
        .then_with(|| pref(a).cmp(&pref(b)))
        .then_with(|| a.index.cmp(&b.index))
}

fn reject(driver: &str, row: &Value, why: String) -> Value {
    json!({"driver": driver, "model_id": row["id"], "reason": why})
}

/// Filter and rank the status document. Pure. An unknown tag is an error (a typo would otherwise
/// read as "nothing qualifies"), judged against the projection's vocabulary when it has one.
pub fn select(s: &Value, req: &Request) -> Result<Value> {
    if let Some(vocab) = s["vocabulary"].as_object().filter(|v| !v.is_empty()) {
        let known: Vec<String> = vocab.keys().map(|k| k.to_lowercase()).collect();
        for t in &req.tags {
            if !known.contains(t) {
                bail!("unknown tag {t:?}; the vocabulary is {known:?}");
            }
        }
    }
    let mut ok: Vec<Candidate> = Vec::new();
    let mut rejected: Vec<Value> = Vec::new();
    let mut index = 0;
    for d in DRIVERS {
        let drv = &s["drivers"][d];
        let headroom = drv["headroom"].as_str().unwrap_or("unknown");
        let load = drv["load"].as_f64().filter(|_| headroom == "ok");
        for row in drv["models"].as_array().into_iter().flatten() {
            index += 1;
            if drv["usage"]["signed_out"] == true {
                rejected.push(reject(d, row, "signed_out".into()));
                continue;
            }
            let tier = row["tier"].as_str();
            if tier != Some(req.tier.as_str()) {
                let why = format!("tier_mismatch: model tier {}", tier.unwrap_or("unknown"));
                rejected.push(reject(d, row, why));
                continue;
            }
            let have: Vec<String> = strings(&row["tags"])
                .iter()
                .map(|t| t.to_lowercase())
                .collect();
            if have.iter().any(|t| t == "high-effort")
                && !req.tags.iter().any(|t| t == "high-effort")
            {
                rejected.push(reject(d, row, "high_effort_requires_explicit_tag".into()));
                continue;
            }
            let missing: Vec<&str> = req
                .tags
                .iter()
                .filter(|t| !have.contains(t))
                .map(String::as_str)
                .collect();
            if !missing.is_empty() {
                rejected.push(reject(
                    d,
                    row,
                    format!("missing_tags: {}", missing.join(",")),
                ));
                continue;
            }
            let offered = strings(&row["efforts"]);
            let (effort, source) = match (&req.effort, offered.is_empty()) {
                (Some(e), true) => {
                    let why = format!("effort_unsupported: no effort control, wanted {e}");
                    rejected.push(reject(d, row, why));
                    continue;
                }
                (Some(e), false) if !offered.contains(e) => {
                    let why = format!("effort_unsupported: offers {offered:?}, wanted {e}");
                    rejected.push(reject(d, row, why));
                    continue;
                }
                (Some(e), false) => (Some(e.clone()), "explicit"),
                (None, true) => (None, "none"),
                (None, false) => match row["effort_for"][req.tier.as_str()].as_str() {
                    Some(e) => (Some(e.to_owned()), "effort_for"),
                    None => {
                        let why =
                            format!("no_eligible_effort: nothing offered fits tier {}", req.tier);
                        rejected.push(reject(d, row, why));
                        continue;
                    }
                },
            };
            if matches!(headroom, "low" | "exhausted") {
                rejected.push(reject(d, row, format!("headroom_{headroom}")));
                continue;
            }
            ok.push(Candidate {
                index,
                driver: d,
                load,
                row: json!({
                    "driver": d,
                    "model_id": row["id"],
                    "display": row["display"],
                    "model_arg": row["model_arg"],
                    "tier": row["tier"],
                    "effort": effort,
                    "effort_source": source,
                    "headroom": headroom,
                    "load": load,
                    "usage_trusted": load.is_some(),
                    "usage": drv["usage"],
                }),
            });
        }
    }
    ok.sort_by(|a, b| order(a, b, req.from.as_deref()));
    let ranked: Vec<Value> = ok.into_iter().map(|c| c.row).collect();
    Ok(json!({
        "request": {"tags": req.tags, "tier": req.tier, "effort": req.effort, "from": req.from},
        "selected": ranked.first().cloned(),
        "ranked": ranked,
        "rejected": rejected,
        "inputs": {"tags": s["tags"], "usage": s["usage"], "catalogue": s["catalogue"]},
        "snapshot_at": s["snapshot_at"],
        "route_verified": false,
    }))
}

/// Human rendering of a [`select`] result.
pub fn render_text(r: &Value) -> String {
    let mut out = String::new();
    let pct = |v: &Value| {
        v.as_f64()
            .map_or("?".to_string(), |u| format!("{:.0}%", u * 100.0))
    };
    let launch = |c: &Value| {
        let arg = c["model_arg"]
            .as_str()
            .map_or("(omit --model)".to_string(), |a| format!("--model {a}"));
        let eff = c["effort"]
            .as_str()
            .map_or(String::new(), |e| format!(" --effort {e}"));
        format!(
            "{} {arg}{eff} (headroom {}, load {})",
            c["driver"].as_str().unwrap_or("?"),
            c["headroom"].as_str().unwrap_or("unknown"),
            pct(&c["load"])
        )
    };
    match r["selected"].as_object() {
        Some(_) => out.push_str(&format!("selected: {}\n", launch(&r["selected"]))),
        None => out.push_str("selected: none\n"),
    }
    for (i, c) in r["ranked"].as_array().into_iter().flatten().enumerate() {
        out.push_str(&format!("  {}. {}\n", i + 1, launch(c)));
    }
    for x in r["rejected"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  rejected {} {}: {}\n",
            x["driver"].as_str().unwrap_or("?"),
            x["model_id"].as_str().unwrap_or("?"),
            x["reason"].as_str().unwrap_or("?")
        ));
    }
    for k in ["tags", "usage", "catalogue"] {
        let state = r["inputs"][k]["state"].as_str().unwrap_or("?");
        if state != "ok" {
            out.push_str(&format!("  input {k}: {state}\n"));
        }
    }
    out
}

/// The command: read the three inputs, select, print. Exit 0 with a pick, [`EXIT_NONE`] without.
pub fn run(json_out: bool, req: &Request) -> Result<i32> {
    let dirs = status::Dirs::from_env();
    let s = status::build(
        &dirs,
        &status::load(&dirs.tags()),
        &status::load(&dirs.usage()),
        &status::load(&dirs.catalogue()),
        status::now_s(),
    );
    let r = select(&s, req)?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        print!("{}", render_text(&r));
    }
    Ok(if r["selected"].is_null() {
        EXIT_NONE
    } else {
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, tier: &str, tags: &[&str], efforts: &[&str], deep: Option<&str>) -> Value {
        json!({"id": id, "display": id, "model_arg": id, "tier": tier, "tags": tags,
               "efforts": efforts,
               "effort_for": if efforts.is_empty() { Value::Null }
                   else { json!({"fast": "low", "standard": "medium", "deep": deep}) }})
    }

    fn doc(drivers: [(&str, &str, Option<f64>, Vec<Value>); 3]) -> Value {
        let mut m = serde_json::Map::new();
        for (d, headroom, load, models) in drivers {
            m.insert(
                d.into(),
                json!({"headroom": headroom, "load": load, "models": models}),
            );
        }
        json!({"vocabulary": {"code": "c", "review": "r"},
               "tags": {"state": "ok"}, "usage": {"state": "ok"}, "catalogue": {"state": "ok"},
               "drivers": m})
    }

    fn req(tags: &[&str], tier: &str, effort: Option<&str>, from: Option<&str>) -> Request {
        let tags: Vec<String> = tags.iter().map(|s| s.to_string()).collect();
        Request::new(&tags, tier, effort, from).unwrap()
    }

    fn deep(id: &str) -> Value {
        row(
            id,
            "deep",
            &["code", "review"],
            &["low", "high"],
            Some("high"),
        )
    }

    fn picked(r: &Value) -> Vec<String> {
        r["ranked"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["driver"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn invalid_requests_are_refused() {
        let t = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(Request::new(&[], "deep", None, None).is_err());
        assert!(Request::new(&t(&["  "]), "deep", None, None).is_err());
        assert!(Request::new(&t(&["a b"]), "deep", None, None).is_err());
        assert!(Request::new(&t(&["code"]), "huge", None, None).is_err());
        assert!(Request::new(&t(&["code"]), "deep", Some("HIGH"), None).is_err());
        assert!(Request::new(&t(&["code"]), "deep", Some("x=y"), None).is_err());
        assert!(Request::new(&t(&["code"]), "deep", None, Some("gemini")).is_err());
        let ok = Request::new(&t(&["Code", "code"]), " Deep ", Some("high"), Some("AGY")).unwrap();
        assert_eq!(ok.tags, ["code"]);
        assert_eq!(ok.tier, "deep");
        assert_eq!(ok.from.as_deref(), Some("agy"));
    }

    #[test]
    fn signed_out_and_unrequested_high_effort_are_never_selected() {
        let mut s = doc([
            ("claude", "ok", Some(0.01), vec![deep("opus")]),
            ("agy", "ok", Some(0.02), vec![deep("large")]),
            ("codex", "ok", Some(0.4), vec![deep("sol")]),
        ]);
        s["drivers"]["claude"]["usage"] = json!({"signed_out": true});
        s["drivers"]["agy"]["models"][0]["tags"] = json!(["code", "high-effort"]);
        let r = select(&s, &req(&["code"], "deep", None, None)).unwrap();
        assert_eq!(picked(&r), ["codex"]);
        assert_eq!(r["route_verified"], false);
    }

    #[test]
    fn unknown_tag_is_an_error_not_an_empty_result() {
        let s = doc([
            ("claude", "ok", Some(0.1), vec![deep("opus")]),
            ("agy", "ok", Some(0.2), vec![]),
            ("codex", "ok", Some(0.3), vec![]),
        ]);
        assert!(select(&s, &req(&["tpyo"], "deep", None, None)).is_err());
    }

    #[test]
    fn lowest_load_wins_and_unknown_ranks_last() {
        let s = doc([
            ("claude", "ok", Some(0.6), vec![deep("opus")]),
            ("agy", "unknown", None, vec![deep("g")]),
            ("codex", "ok", Some(0.2), vec![deep("sol")]),
        ]);
        let r = select(&s, &req(&["code"], "deep", None, None)).unwrap();
        assert_eq!(picked(&r), ["codex", "claude", "agy"]);
        assert_eq!(r["selected"]["driver"], "codex");
        assert_eq!(r["selected"]["effort"], "high");
        assert_eq!(r["selected"]["effort_source"], "effort_for");
        assert_eq!(r["ranked"][2]["usage_trusted"], false);
    }

    #[test]
    fn stale_reading_is_unknown_so_a_busier_trusted_agent_beats_it() {
        // status::build reports a stale driver as headroom "unknown" with a null load.
        let s = doc([
            ("claude", "unknown", None, vec![deep("opus")]),
            ("agy", "ok", Some(0.95), vec![deep("g")]),
            ("codex", "unknown", None, vec![deep("sol")]),
        ]);
        let r = select(&s, &req(&["code"], "deep", None, None)).unwrap();
        assert_eq!(picked(&r), ["agy", "claude", "codex"]);
        let r = select(&s, &req(&["code"], "deep", None, Some("claude"))).unwrap();
        assert_eq!(picked(&r), ["agy", "codex", "claude"]);
    }

    #[test]
    fn low_and_exhausted_are_excluded_with_a_reason() {
        let s = doc([
            ("claude", "exhausted", Some(0.99), vec![deep("opus")]),
            ("agy", "low", Some(0.85), vec![deep("g")]),
            ("codex", "ok", Some(0.7), vec![deep("sol")]),
        ]);
        let r = select(&s, &req(&["code"], "deep", None, None)).unwrap();
        assert_eq!(picked(&r), ["codex"]);
        let why: Vec<&str> = r["rejected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["reason"].as_str().unwrap())
            .collect();
        assert_eq!(why, ["headroom_exhausted", "headroom_low"]);
    }

    #[test]
    fn nothing_qualifying_selects_null() {
        let s = doc([
            ("claude", "exhausted", Some(0.99), vec![deep("opus")]),
            ("agy", "low", Some(0.85), vec![deep("g")]),
            ("codex", "low", Some(0.9), vec![deep("sol")]),
        ]);
        let r = select(&s, &req(&["code"], "deep", None, None)).unwrap();
        assert!(r["selected"].is_null());
        assert!(render_text(&r).starts_with("selected: none"));
    }

    #[test]
    fn equal_load_prefers_another_harness_than_the_caller() {
        let s = doc([
            ("claude", "ok", Some(0.3), vec![deep("opus")]),
            ("agy", "ok", Some(0.3), vec![deep("g")]),
            ("codex", "ok", Some(0.5), vec![deep("sol")]),
        ]);
        let r = select(&s, &req(&["code"], "deep", None, Some("claude"))).unwrap();
        assert_eq!(picked(&r), ["agy", "claude", "codex"]);
        let r = select(&s, &req(&["code"], "deep", None, Some("agy"))).unwrap();
        assert_eq!(picked(&r), ["claude", "agy", "codex"]);
        // A caller never outranks a clearly less loaded model.
        let r = select(&s, &req(&["code"], "deep", None, None)).unwrap();
        assert_eq!(picked(&r), ["claude", "agy", "codex"]);
    }

    #[test]
    fn tags_and_tier_must_both_match() {
        let s = doc([
            (
                "claude",
                "ok",
                Some(0.1),
                vec![
                    row("haiku", "fast", &["code", "review"], &["low"], Some("low")),
                    row("untagged", "deep", &["code"], &["high"], Some("high")),
                    row(
                        "untiered",
                        "deep",
                        &["code", "review"],
                        &["high"],
                        Some("high"),
                    ),
                ],
            ),
            ("agy", "ok", Some(0.2), vec![]),
            ("codex", "ok", Some(0.3), vec![]),
        ]);
        let mut s2 = s.clone();
        s2["drivers"]["claude"]["models"][2]["tier"] = Value::Null;
        let r = select(&s2, &req(&["code", "review"], "deep", None, None)).unwrap();
        assert!(r["selected"].is_null());
        let why: Vec<&str> = r["rejected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["reason"].as_str().unwrap())
            .collect();
        assert!(
            why[0].starts_with("tier_mismatch: model tier fast"),
            "{why:?}"
        );
        assert!(why[1].starts_with("missing_tags: review"), "{why:?}");
        assert!(
            why[2].starts_with("tier_mismatch: model tier unknown"),
            "{why:?}"
        );
        let r = select(&s, &req(&["code", "review"], "fast", None, None)).unwrap();
        assert_eq!(r["selected"]["model_id"], "haiku");
        assert_eq!(r["selected"]["effort"], "low");
    }

    #[test]
    fn unsupported_effort_is_rejected_not_clamped() {
        let s = doc([
            ("claude", "ok", Some(0.1), vec![deep("opus")]),
            (
                "agy",
                "ok",
                Some(0.2),
                vec![row("plain", "deep", &["code"], &[], None)],
            ),
            (
                "codex",
                "ok",
                Some(0.3),
                vec![row(
                    "sol",
                    "deep",
                    &["code"],
                    &["low", "xhigh"],
                    Some("low"),
                )],
            ),
        ]);
        let r = select(&s, &req(&["code"], "deep", Some("xhigh"), None)).unwrap();
        assert_eq!(picked(&r), ["codex"]);
        assert_eq!(r["selected"]["effort"], "xhigh");
        assert_eq!(r["selected"]["effort_source"], "explicit");
        let why: Vec<&str> = r["rejected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["reason"].as_str().unwrap())
            .collect();
        assert!(why[0].starts_with("effort_unsupported: offers"), "{why:?}");
        assert!(
            why[1].starts_with("effort_unsupported: no effort control"),
            "{why:?}"
        );
    }

    #[test]
    fn no_effort_control_is_selectable_without_a_requested_effort() {
        let s = doc([
            (
                "claude",
                "ok",
                Some(0.1),
                vec![row("plain", "deep", &["code"], &[], None)],
            ),
            ("agy", "ok", Some(0.2), vec![]),
            ("codex", "ok", Some(0.3), vec![]),
        ]);
        let r = select(&s, &req(&["code"], "deep", None, None)).unwrap();
        assert!(r["selected"]["effort"].is_null());
        assert_eq!(r["selected"]["effort_source"], "none");
        assert!(render_text(&r).contains("claude --model plain (headroom ok"));
    }

    #[test]
    fn policy_with_no_eligible_effort_rejects_the_model() {
        // effort_for[deep] is null when every offered effort is reserved or below the ladder.
        let s = doc([
            (
                "claude",
                "ok",
                Some(0.1),
                vec![row("resv", "deep", &["code"], &["xhigh"], None)],
            ),
            ("agy", "ok", Some(0.2), vec![]),
            ("codex", "ok", Some(0.3), vec![]),
        ]);
        let r = select(&s, &req(&["code"], "deep", None, None)).unwrap();
        assert!(r["selected"].is_null());
        assert!(r["rejected"][0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("no_eligible_effort"));
    }

    #[test]
    fn missing_inputs_yield_none_and_say_why() {
        let dirs = status::Dirs {
            config: std::env::temp_dir().join(format!("ca-sel-c-{}", std::process::id())),
            cache: std::env::temp_dir().join(format!("ca-sel-k-{}", std::process::id())),
        };
        let s = status::build(
            &dirs,
            &status::Loaded::Missing,
            &status::Loaded::Missing,
            &status::Loaded::Missing,
            10,
        );
        let r = select(&s, &req(&["code"], "deep", None, None)).unwrap();
        assert!(r["selected"].is_null());
        let text = render_text(&r);
        assert!(text.contains("input tags: missing"), "{text}");
        assert!(text.contains("input catalogue: missing"), "{text}");
    }
}
