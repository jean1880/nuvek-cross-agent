//! `cross-agent status`: read-only view of which models each harness offers, what they are good
//! for (tags in `model-tags.json`) and how much subscription headroom is left (`usage.json`,
//! written by agent-terminal). Never spawns an agent and never writes.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const DRIVERS: [&str; 3] = ["claude", "agy", "codex"];
/// Two agent-terminal account TTLs: older usage readings are not trusted.
pub const STALE_AFTER_S: u64 = 20 * 60;
const LOW: f64 = 0.80;
const EXHAUSTED: f64 = 0.98;

/// Where the three inputs live.
pub struct Dirs {
    pub config: PathBuf,
    pub cache: PathBuf,
}

impl Dirs {
    pub fn from_env() -> Dirs {
        let home = nuvek_core::env::home_dir();
        let abs = |name: &str, fallback: &str| {
            nuvek_core::env::env_var(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
        };
        Dirs {
            config: abs("XDG_CONFIG_HOME", ".config"),
            cache: abs("XDG_CACHE_HOME", ".cache"),
        }
    }
    pub fn tags(&self) -> PathBuf {
        self.config.join("agent-terminal/model-tags.json")
    }
    pub fn usage(&self) -> PathBuf {
        self.cache.join("agent-terminal/usage.json")
    }
    pub fn catalogue(&self) -> PathBuf {
        self.cache.join("agent-terminal/models.json")
    }
}

/// One input file: absent, unreadable/unparseable (with the reason), or parsed.
pub enum Loaded {
    Missing,
    Invalid(String),
    Ok(Value),
}

pub fn load(path: &Path) -> Loaded {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Loaded::Missing,
        Err(e) => Loaded::Invalid(format!("cannot read: {e}")),
        Ok(t) => match serde_json::from_str(&t) {
            Ok(v) => Loaded::Ok(v),
            Err(e) => Loaded::Invalid(format!("not valid JSON: {e}")),
        },
    }
}

pub fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Case-insensitive glob with `*` anywhere, matching the agent-terminal tag catalogue.
pub fn pattern_matches(pattern: &str, id: &str) -> bool {
    let (pattern, id) = (pattern.to_lowercase(), id.to_lowercase());
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = id.strip_prefix(first) else {
        return false;
    };
    if !pattern.contains('*') {
        return rest.is_empty();
    }
    let parts: Vec<_> = parts.collect();
    for (index, part) in parts.iter().enumerate() {
        if index + 1 == parts.len() {
            return rest.ends_with(part);
        }
        let Some(at) = rest.find(part) else {
            return false;
        };
        rest = &rest[at + part.len()..];
    }
    true
}

/// First entry whose driver (if set) and pattern match.
fn lookup<'a>(tags: &'a Value, driver: &str, id: &str) -> Option<&'a Value> {
    tags.get("models")?.as_array()?.iter().find(|e| {
        let d_ok = match e.get("driver").and_then(Value::as_str) {
            Some(d) => d == driver,
            None => true,
        };
        d_ok && e
            .get("match")
            .and_then(Value::as_str)
            .is_some_and(|m| pattern_matches(m, id))
    })
}

/// The fullest window's `used` (0..1): the agent's load. `None` with no windows.
fn max_used(windows: &[Value]) -> Option<f64> {
    windows
        .iter()
        .filter_map(|w| w.get("used").and_then(Value::as_f64))
        .fold(None, |m: Option<f64>, u| Some(m.map_or(u, |x| x.max(u))))
}

/// Headroom from the windows' highest `used`; no windows = unknown.
fn headroom_of(windows: &[Value]) -> &'static str {
    match max_used(windows) {
        None => "unknown",
        Some(u) if u >= EXHAUSTED => "exhausted",
        Some(u) if u >= LOW => "low",
        Some(_) => "ok",
    }
}

/// Usage block for one driver, plus its headroom and load (`None` unless the reading is trusted).
fn usage_for(usage: &Loaded, driver: &str, now: u64) -> (Value, &'static str, Option<f64>) {
    let Loaded::Ok(u) = usage else {
        return (Value::Null, "unknown", None);
    };
    let file_fresh = u
        .get("written_at")
        .and_then(Value::as_u64)
        .is_some_and(|w| now.saturating_sub(w) <= STALE_AFTER_S);
    let Some(a) = u.get("agents").and_then(|a| a.get(driver)) else {
        return (Value::Null, "unknown", None);
    };
    let read_at = a.get("read_at").and_then(Value::as_u64);
    let age = read_at.map(|r| now.saturating_sub(r));
    let windows: Vec<Value> = a
        .get("windows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let stored_unknown = a.get("headroom").and_then(Value::as_str) == Some("unknown");
    let trusted = file_fresh && age.is_some_and(|x| x <= STALE_AFTER_S) && !stored_unknown;
    let (headroom, load) = if trusted {
        (headroom_of(&windows), max_used(&windows))
    } else {
        ("unknown", None)
    };
    let block = json!({
        "age_s": age,
        "refresh_failed": a.get("refresh_failed").and_then(Value::as_bool).unwrap_or(false),
        "signed_out": a.get("signed_out").and_then(Value::as_bool).unwrap_or(false),
        "windows": windows,
    });
    (block, headroom, load)
}

/// SPREAD-USAGE order: agents with room, least loaded first; unknown readings after every known
/// one; `low`/`exhausted` agents are left out.
fn spread_order(loads: &[(&str, &str, Option<f64>)]) -> Vec<String> {
    let mut usable: Vec<_> = loads
        .iter()
        .filter(|(_, h, _)| matches!(*h, "ok" | "unknown"))
        .collect();
    usable.sort_by(|a, b| match (a.2, b.2) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    usable.into_iter().map(|(d, _, _)| d.to_string()).collect()
}

/// The `--model` value to pass for a catalogue id, or `None` when `--model` should be omitted.
fn model_arg(driver: &str, id: &str) -> Option<String> {
    (!(driver == "claude" && id == "default")).then(|| id.to_string())
}

fn state_block(l: &Loaded, path: &Path, missing_hint: &str) -> Value {
    match l {
        Loaded::Missing => {
            json!({"state": "missing", "path": path.display().to_string(), "hint": missing_hint})
        }
        Loaded::Invalid(r) => {
            json!({"state": "invalid", "path": path.display().to_string(), "reason": r})
        }
        Loaded::Ok(_) => json!({"state": "ok", "path": path.display().to_string()}),
    }
}

/// Assemble the status document. Pure: all I/O happens in the caller.
pub fn build(dirs: &Dirs, tags: &Loaded, usage: &Loaded, cat: &Loaded, now: u64) -> Value {
    let mut tags_state = state_block(
        tags,
        &dirs.tags(),
        "create it to tag models (see the README, \"Model tags\")",
    );
    let tags_v = match tags {
        Loaded::Ok(v) if v.get("version").and_then(Value::as_u64) == Some(1) => Some(v),
        Loaded::Ok(_) => {
            tags_state = json!({"state": "invalid", "path": dirs.tags().display().to_string(),
                "reason": "unsupported or missing version (want 1)"});
            None
        }
        _ => None,
    };

    let mut usage_state = state_block(
        usage,
        &dirs.usage(),
        "agent-terminal not running (no usage.json)",
    );
    if let Loaded::Ok(u) = usage {
        match u.get("written_at").and_then(Value::as_u64) {
            Some(w) if now.saturating_sub(w) <= STALE_AFTER_S => {
                usage_state["age_s"] = json!(now.saturating_sub(w));
            }
            other => {
                usage_state["state"] = json!("stale");
                usage_state["age_s"] = json!(other.map(|w| now.saturating_sub(w)));
                usage_state["hint"] = json!("agent-terminal not running (usage older than 20 min)");
            }
        }
    }
    let cat_state = state_block(
        cat,
        &dirs.catalogue(),
        "agent-terminal has not cached its model catalogue yet",
    );

    let policy = tags_v
        .and_then(|v| v.get("effort_policy"))
        .filter(|p| p.is_object());
    let mut drivers = Map::new();
    let mut loads = Vec::new();
    for d in DRIVERS {
        let (usage_block, headroom, load) = usage_for(usage, d, now);
        loads.push((d, headroom, load));
        let models: Vec<Value> = match cat {
            Loaded::Ok(c) => c
                .get(d)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|m| model_row(m, d, tags_v, policy))
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        drivers.insert(
            d.into(),
            json!({"headroom": headroom, "load": load, "usage": usage_block, "models": models}),
        );
    }
    let spread = spread_order(&loads);
    json!({
        "snapshot_at": now,
        "tags": tags_state,
        "usage": usage_state,
        "catalogue": cat_state,
        "vocabulary": tags_v.and_then(|v| v.get("vocabulary")).cloned().unwrap_or_else(|| json!({})),
        "effort_policy": policy.cloned().unwrap_or(Value::Null),
        "spread_order": spread,
        "drivers": drivers,
    })
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Clamp a policy effort to what a model offers: the value itself if offered, else the highest
/// offered effort below it on the ladder, else the lowest offered. Reserved efforts are never
/// chosen by the fallback. `None` when the model has no effort control or nothing eligible.
fn clamp_effort(
    want: &str,
    ladder: &[String],
    reserved: &[String],
    offered: &[String],
) -> Option<String> {
    // A reserved rung is never recommended, even when the policy names it.
    if !reserved.iter().any(|r| r == want) && offered.iter().any(|o| o == want) {
        return Some(want.to_string());
    }
    let pos = |e: &str| ladder.iter().position(|l| l == e);
    let want_pos = pos(want)?;
    let mut eligible: Vec<(usize, &String)> = offered
        .iter()
        .filter(|o| !reserved.contains(o))
        .filter_map(|o| pos(o).map(|p| (p, o)))
        .collect();
    eligible.sort_by_key(|(p, _)| *p);
    eligible
        .iter()
        .rev()
        .find(|(p, _)| *p < want_pos)
        .or_else(|| eligible.first())
        .map(|(_, o)| (*o).clone())
}

/// `{fast, standard, deep}` clamped to `offered`; `Null` with no policy or no effort control.
fn effort_for(policy: Option<&Value>, offered: &[String]) -> Value {
    let Some(p) = policy else { return Value::Null };
    if offered.is_empty() {
        return Value::Null;
    }
    let ladder = strings(p.get("ladder"));
    let reserved = strings(p.get("reserved"));
    let mut out = Map::new();
    for tier in ["fast", "standard", "deep"] {
        let v = p
            .get(tier)
            .and_then(Value::as_str)
            .and_then(|w| clamp_effort(w, &ladder, &reserved, offered));
        out.insert(tier.into(), json!(v));
    }
    Value::Object(out)
}

fn model_row(
    m: &Value,
    driver: &str,
    tags: Option<&Value>,
    policy: Option<&Value>,
) -> Option<Value> {
    let id = m.get("id").and_then(Value::as_str)?;
    let entry = tags.and_then(|t| lookup(t, driver, id));
    let field = |k: &str| entry.and_then(|e| e.get(k)).cloned().unwrap_or(Value::Null);
    let efforts = strings(m.get("efforts"));
    Some(json!({
        "id": id,
        "display": m.get("display").and_then(Value::as_str),
        "model_arg": model_arg(driver, id),
        "tier": field("tier"),
        "tags": entry.and_then(|e| e.get("tags")).cloned().unwrap_or_else(|| json!([])),
        "best_for": field("best_for"),
        "efforts": efforts,
        "default_effort": m.get("default_effort").and_then(Value::as_str),
        "effort_for": effort_for(policy, &efforts),
    }))
}

/// Human table.
pub fn render_table(s: &Value) -> String {
    let mut out = String::new();
    let line = |out: &mut String, t: String| {
        out.push_str(&t);
        out.push('\n');
    };
    for (key, label) in [
        ("tags", "tags"),
        ("usage", "usage"),
        ("catalogue", "catalogue"),
    ] {
        let b = &s[key];
        let state = b["state"].as_str().unwrap_or("?");
        let extra = b["hint"].as_str().or(b["reason"].as_str()).unwrap_or("");
        line(
            &mut out,
            format!("{label}: {state} {extra}").trim_end().to_string(),
        );
    }
    let order: Vec<&str> = s["spread_order"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    line(
        &mut out,
        format!("spread order (least loaded first): {}", order.join(" > ")),
    );
    let empty = Map::new();
    for (d, v) in s["drivers"].as_object().unwrap_or(&empty) {
        line(&mut out, String::new());
        line(
            &mut out,
            format!(
                "{d}  headroom: {}  load: {}",
                v["headroom"].as_str().unwrap_or("unknown"),
                v["load"]
                    .as_f64()
                    .map_or("?".to_string(), |u| format!("{:.0}%", u * 100.0))
            ),
        );
        for w in v["usage"]["windows"].as_array().into_iter().flatten() {
            let pct = w["used"]
                .as_f64()
                .map_or("?".to_string(), |u| format!("{:.0}%", u * 100.0));
            line(
                &mut out,
                format!(
                    "    window {}: {pct} used",
                    w["label"].as_str().unwrap_or("?")
                ),
            );
        }
        for m in v["models"].as_array().into_iter().flatten() {
            let arg = m["model_arg"]
                .as_str()
                .map_or("(omit --model)".to_string(), |a| format!("--model {a}"));
            let tags: Vec<&str> = m["tags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let ef = &m["effort_for"];
            let effort = if ef.is_object() {
                let pick = |k: &str| ef[k].as_str().unwrap_or("-").to_string();
                format!(
                    " effort f/s/d: {}/{}/{}",
                    pick("fast"),
                    pick("standard"),
                    pick("deep")
                )
            } else {
                String::new()
            };
            line(
                &mut out,
                format!(
                    "  {:<26} {:<9} {:<28} [{}] {}{}",
                    m["id"].as_str().unwrap_or("?"),
                    m["tier"].as_str().unwrap_or("-"),
                    arg,
                    tags.join(", "),
                    m["best_for"].as_str().unwrap_or(""),
                    effort
                )
                .trim_end()
                .to_string(),
            );
        }
    }
    out
}

/// agy accepts display names only: map a catalogue id (case-insensitive) to its display name.
pub fn agy_display_for_id(cat: &Loaded, id: &str) -> Option<String> {
    let Loaded::Ok(c) = cat else { return None };
    c.get("agy")?.as_array()?.iter().find_map(|m| {
        let mid = m.get("id")?.as_str()?;
        mid.eq_ignore_ascii_case(id.trim())
            .then(|| m.get("display")?.as_str().map(String::from))?
    })
}

/// The efforts the catalogue offers for an agy id (case-insensitive); `None` for an unknown id.
pub fn agy_efforts_for_id(cat: &Loaded, id: &str) -> Option<Vec<String>> {
    let Loaded::Ok(c) = cat else { return None };
    c.get("agy")?.as_array()?.iter().find_map(|m| {
        let mid = m.get("id")?.as_str()?;
        mid.eq_ignore_ascii_case(id.trim()).then(|| {
            m.get("efforts")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cross-agent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fixture(dirs: &Dirs, written_at: u64, read_at: u64, used: f64) {
        std::fs::create_dir_all(dirs.tags().parent().unwrap()).unwrap();
        std::fs::create_dir_all(dirs.usage().parent().unwrap()).unwrap();
        std::fs::write(
            dirs.tags(),
            json!({"version": 1, "vocabulary": {"code": "c"}, "models": [
                {"match": "*sol", "driver": "codex", "tier": "deep", "tags": ["code"], "best_for": "checker"},
                {"match": "*opus*", "driver": null, "tier": "deep", "tags": ["code"]},
            ]})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dirs.usage(),
            json!({"version": 1, "written_at": written_at, "agents": {
                "claude": {"headroom": "ok", "read_at": read_at, "refresh_failed": false, "signed_out": false,
                           "windows": [{"group": null, "label": "5-hour", "used": used, "resets_at": null}]}}})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dirs.catalogue(),
            json!({"claude": [{"driver": "claude", "id": "default", "display": "Default"},
                              {"driver": "claude", "id": "opus", "display": "Opus 5.5"}],
                   "agy": [{"driver": "agy", "id": "claude-opus-5-5", "display": "Claude Opus 5.5 (Thinking)"}],
                   "codex": [{"driver": "codex", "id": "gpt-6.1-sol", "display": "GPT 6.1 Sol"}]})
            .to_string(),
        )
        .unwrap();
    }

    fn run(dirs: &Dirs, now: u64) -> Value {
        build(
            dirs,
            &load(&dirs.tags()),
            &load(&dirs.usage()),
            &load(&dirs.catalogue()),
            now,
        )
    }

    #[test]
    fn fresh_files_give_tags_headroom_and_model_args() {
        let d = tmp("fresh");
        let dirs = Dirs {
            config: d.join("c"),
            cache: d.join("k"),
        };
        fixture(&dirs, 1000, 990, 0.85);
        let s = run(&dirs, 1100);
        assert_eq!(s["usage"]["state"], "ok");
        assert_eq!(s["drivers"]["claude"]["headroom"], "low");
        assert_eq!(s["drivers"]["claude"]["load"], 0.85);
        assert_eq!(s["drivers"]["agy"]["headroom"], "unknown");
        assert!(s["drivers"]["agy"]["load"].is_null());
        // A `low` agent is left out of the spread order; unknown readings stay, last.
        assert_eq!(s["spread_order"], json!(["agy", "codex"]));
        let claude = &s["drivers"]["claude"]["models"];
        assert!(claude[0]["model_arg"].is_null(), "default omits --model");
        assert_eq!(claude[1]["model_arg"], "opus");
        assert_eq!(claude[1]["tier"], "deep");
        // agy serves Claude too: the driver-less family entry tags it, the codex-only one does not.
        assert_eq!(s["drivers"]["agy"]["models"][0]["tags"][0], "code");
        assert_eq!(s["drivers"]["codex"]["models"][0]["best_for"], "checker");
        assert!(render_table(&s).contains("--model claude-opus-5-5"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn spread_order_ranks_by_load_and_drops_full_agents() {
        let order = spread_order(&[
            ("claude", "ok", Some(0.6)),
            ("agy", "unknown", None),
            ("codex", "ok", Some(0.2)),
        ]);
        assert_eq!(order, ["codex", "claude", "agy"]);
        let order = spread_order(&[
            ("claude", "exhausted", Some(0.99)),
            ("agy", "low", Some(0.85)),
            ("codex", "ok", Some(0.7)),
        ]);
        assert_eq!(order, ["codex"]);
    }

    #[test]
    fn exhausted_and_stale_readings() {
        let d = tmp("stale");
        let dirs = Dirs {
            config: d.join("c"),
            cache: d.join("k"),
        };
        fixture(&dirs, 1000, 1000, 0.99);
        assert_eq!(
            run(&dirs, 1010)["drivers"]["claude"]["headroom"],
            "exhausted"
        );
        let s = run(&dirs, 1000 + STALE_AFTER_S + 1);
        assert_eq!(s["usage"]["state"], "stale");
        assert!(s["usage"]["hint"]
            .as_str()
            .unwrap()
            .contains("agent-terminal not running"));
        assert_eq!(s["drivers"]["claude"]["headroom"], "unknown");
        // File fresh but this driver's own reading old.
        fixture(&dirs, 5000, 1000, 0.1);
        assert_eq!(run(&dirs, 5001)["drivers"]["claude"]["headroom"], "unknown");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_and_invalid_inputs_are_reported_not_fatal() {
        let d = tmp("missing");
        let dirs = Dirs {
            config: d.join("c"),
            cache: d.join("k"),
        };
        let s = run(&dirs, 10);
        assert_eq!(s["tags"]["state"], "missing");
        assert_eq!(s["usage"]["state"], "missing");
        assert!(s["usage"]["hint"].as_str().unwrap().contains("not running"));
        assert_eq!(s["drivers"]["codex"]["models"], json!([]));
        std::fs::create_dir_all(dirs.tags().parent().unwrap()).unwrap();
        std::fs::write(dirs.tags(), "{nope").unwrap();
        assert_eq!(run(&dirs, 10)["tags"]["state"], "invalid");
        std::fs::write(dirs.tags(), r#"{"version": 2}"#).unwrap();
        assert_eq!(run(&dirs, 10)["tags"]["state"], "invalid");
        let _ = std::fs::remove_dir_all(&d);
    }

    fn ladder() -> Vec<String> {
        ["minimal", "low", "medium", "high", "xhigh", "max", "ultra"]
            .map(String::from)
            .to_vec()
    }

    fn eff(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn effort_clamping_rules() {
        let l = ladder();
        let reserved = eff(&["xhigh", "max", "ultra"]);
        // Offered as-is.
        assert_eq!(
            clamp_effort("high", &l, &reserved, &eff(&["low", "high", "max"])).as_deref(),
            Some("high")
        );
        // Not offered: highest offered below it.
        assert_eq!(
            clamp_effort("high", &l, &reserved, &eff(&["low", "medium", "max"])).as_deref(),
            Some("medium")
        );
        // Nothing below: the lowest offered.
        assert_eq!(
            clamp_effort("low", &l, &reserved, &eff(&["medium", "high"])).as_deref(),
            Some("medium")
        );
        // Reserved efforts are never a fallback pick, even when they are the only ones below/above.
        assert_eq!(
            clamp_effort("ultra", &l, &reserved, &eff(&["low", "xhigh", "max"])).as_deref(),
            Some("low")
        );
        assert_eq!(
            clamp_effort("high", &l, &reserved, &eff(&["xhigh", "max"])),
            None
        );
        // A policy naming a reserved rung still never gets it, even when offered.
        assert_eq!(
            clamp_effort("max", &l, &reserved, &eff(&["low", "max"])).as_deref(),
            Some("low")
        );
        assert_eq!(clamp_effort("max", &l, &reserved, &eff(&["max"])), None);
        // Unknown policy value, no efforts.
        assert_eq!(clamp_effort("bogus", &l, &reserved, &eff(&["low"])), None);
        assert!(effort_for(Some(&json!({"ladder": l, "fast": "low"})), &[]).is_null());
        assert!(effort_for(None, &eff(&["low"])).is_null());
    }

    #[test]
    fn effort_fields_in_rows_and_table() {
        let d = tmp("effort");
        let dirs = Dirs {
            config: d.join("c"),
            cache: d.join("k"),
        };
        let tags = Loaded::Ok(json!({"version": 1, "vocabulary": {}, "models": [],
            "effort_policy": {"ladder": ladder(), "fast": "low", "standard": "medium",
                              "deep": "high", "reserved": ["xhigh", "max", "ultra"]}}));
        let cat = Loaded::Ok(json!({
            "claude": [
                {"id": "opus", "display": "Opus", "efforts": ["low", "medium", "high", "xhigh", "max"], "default_effort": "high"},
                {"id": "claude-haiku-4-5", "display": "Haiku 4.5", "efforts": []}],
            "agy": [{"id": "g", "display": "G", "efforts": ["medium", "xhigh"], "default_effort": null}]}));
        let s = build(&dirs, &tags, &Loaded::Missing, &cat, 1);
        let opus = &s["drivers"]["claude"]["models"][0];
        assert_eq!(
            opus["effort_for"],
            json!({"fast": "low", "standard": "medium", "deep": "high"})
        );
        assert_eq!(opus["default_effort"], "high");
        assert_eq!(s["effort_policy"]["deep"], "high");
        let haiku = &s["drivers"]["claude"]["models"][1];
        assert_eq!(haiku["efforts"], json!([]));
        assert!(haiku["effort_for"].is_null());
        assert!(haiku["default_effort"].is_null());
        // agy: low is below everything offered -> lowest offered; deep=high -> medium.
        assert_eq!(
            s["drivers"]["agy"]["models"][0]["effort_for"],
            json!({"fast": "medium", "standard": "medium", "deep": "medium"})
        );
        assert!(render_table(&s).contains("effort f/s/d: low/medium/high"));
        // No policy: null echo, null effort_for, efforts still listed.
        let s = build(&dirs, &Loaded::Missing, &Loaded::Missing, &cat, 1);
        assert!(s["effort_policy"].is_null());
        assert!(s["drivers"]["claude"]["models"][0]["effort_for"].is_null());
        assert_eq!(s["drivers"]["claude"]["models"][0]["efforts"][0], "low");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn match_forms_and_precedence() {
        assert!(pattern_matches("gpt-6*-sol", "GPT-6.1-Sol"));
        assert!(pattern_matches("*gemini*flash*", "gemini-3.8-flash"));
        assert!(!pattern_matches("gpt-6*-sol", "gpt-5.6-sol"));
        assert!(!pattern_matches("a*b*c", "acb"));
        assert!(pattern_matches("é*é", "é🐈é"));
        assert!(pattern_matches("*OPUS*", "claude-opus-5"));
        assert!(pattern_matches("gpt-oss-*", "GPT-OSS-120b"));
        assert!(pattern_matches("*flash", "gemini-3.8-flash"));
        assert!(pattern_matches("exact", "EXACT"));
        assert!(!pattern_matches("exact", "exactly"));
    }

    #[test]
    fn agy_ids_map_to_display_names() {
        let d = tmp("agy");
        let dirs = Dirs {
            config: d.join("c"),
            cache: d.join("k"),
        };
        fixture(&dirs, 1, 1, 0.1);
        let cat = load(&dirs.catalogue());
        assert_eq!(
            agy_display_for_id(&cat, "Claude-Opus-5-5").as_deref(),
            Some("Claude Opus 5.5 (Thinking)")
        );
        assert_eq!(agy_display_for_id(&cat, "Gemini 3.8 Flash (High)"), None);
        assert_eq!(agy_display_for_id(&Loaded::Missing, "x"), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
