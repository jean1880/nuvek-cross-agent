//! `cross-agent status`: read-only view of which models each harness offers, what they are good
//! for (tags in `model-tags.json`) and how much subscription headroom is left (`usage.json`,
//! written by agent-terminal). Never spawns an agent and never writes.
//!
//! It also lists the user's Local model servers (`drivers.local.profiles`): the enabled
//! `"driver": "local"` profiles of agent-terminal's `config.json`, each with its cost, whether it
//! is available for delegation, and the models its server lists in `models.json` (matched to the
//! profile by provider kind and address). A profile is `free` unless it says otherwise
//! (`free_to_run`) or its provider kind is metered (`litellm`, `openai_compatible`, anything
//! unknown); a free one reports headroom `ok`, load 0.0, a metered one `unknown` and null (there
//! are no vendor windows to read). Every vendor driver reports `cost: "metered"`.
//!
//! Tags come from two layers read here, user over file: the user's own `model-tags.user.json`
//! (path overridable with `user_model_tags_path` in `config.json`) and the shared
//! `model-tags.json`. An entry may carry `profile` (a profile id) and a `driver` of `local`; within
//! a layer an entry for the model's own profile beats an unscoped one, and an entry for another
//! profile never matches. `tags_source` names the layer (`user`, `file`; `built_in` is
//! agent-terminal's own layer, which is not visible here, so a model matching nothing reports
//! `null`). Every input file must be a regular file of at most [`MAX_INPUT_BYTES`].

use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const DRIVERS: [&str; 3] = ["claude", "agy", "codex"];
/// Largest input file read; anything bigger is reported `invalid`, not loaded.
pub const MAX_INPUT_BYTES: u64 = 4 << 20;
/// The pseudo driver under which Local model servers are listed.
pub const LOCAL: &str = "local";
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
    /// agent-terminal's settings: where the Local server profiles are.
    pub fn app_config(&self) -> PathBuf {
        self.config.join("agent-terminal/config.json")
    }
    /// The user's tag file when `config.json` does not move it.
    pub fn user_tags_default(&self) -> PathBuf {
        self.config.join("agent-terminal/model-tags.user.json")
    }
}

/// Inputs beyond the three core files: the app settings and the user's tag layer.
pub struct Extra {
    pub config: Loaded,
    pub user_tags: Loaded,
    pub user_tags_path: PathBuf,
}

impl Extra {
    /// No settings and no user tags (a caller that reads neither).
    #[cfg(test)]
    pub fn none(dirs: &Dirs) -> Extra {
        Extra {
            config: Loaded::Missing,
            user_tags: Loaded::Missing,
            user_tags_path: dirs.user_tags_default(),
        }
    }
}

/// The user's tag file: `user_model_tags_path` from the settings (`~/` expanded against `home`),
/// when it is absolute, else the default beside the settings.
pub fn user_tags_path(dirs: &Dirs, config: &Loaded, home: &Path) -> PathBuf {
    let set = match config {
        Loaded::Ok(c) => c
            .get("user_model_tags_path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        _ => None,
    };
    let expanded = set.map(|s| match s.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if s == "~" => home.to_path_buf(),
        None => PathBuf::from(s),
    });
    match expanded {
        Some(p) if p.is_absolute() => p,
        _ => dirs.user_tags_default(),
    }
}

/// Read every input and assemble the status document.
pub fn snapshot(dirs: &Dirs) -> Value {
    let config = load(&dirs.app_config());
    let user_tags_path = user_tags_path(dirs, &config, &nuvek_core::env::home_dir());
    let extra = Extra {
        user_tags: load(&user_tags_path),
        config,
        user_tags_path,
    };
    build_with(
        dirs,
        &load(&dirs.tags()),
        &load(&dirs.usage()),
        &load(&dirs.catalogue()),
        &extra,
        now_s(),
    )
}

/// One input file: absent, unreadable/unparseable (with the reason), or parsed.
pub enum Loaded {
    Missing,
    Invalid(String),
    Ok(Value),
}

/// Read a JSON file: a regular file of at most [`MAX_INPUT_BYTES`] (a FIFO or device is refused
/// before it is opened).
pub fn load(path: &Path) -> Loaded {
    let invalid = |e: std::io::Error| Loaded::Invalid(format!("cannot read: {e}"));
    let meta = match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::Missing,
        Err(e) => return invalid(e),
        Ok(m) => m,
    };
    if !meta.is_file() {
        return Loaded::Invalid("not a regular file".into());
    }
    if meta.len() > MAX_INPUT_BYTES {
        return Loaded::Invalid(format!("larger than {MAX_INPUT_BYTES} bytes"));
    }
    let mut text = String::new();
    let read = std::fs::File::open(path)
        .and_then(|f| f.take(MAX_INPUT_BYTES + 1).read_to_string(&mut text));
    match read {
        Err(e) => invalid(e),
        Ok(_) if text.len() as u64 > MAX_INPUT_BYTES => {
            Loaded::Invalid(format!("larger than {MAX_INPUT_BYTES} bytes"))
        }
        Ok(_) => match serde_json::from_str(&text) {
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

/// The tag layers, strongest first.
#[derive(Clone, Copy, Default)]
struct Layers<'a> {
    user: Option<&'a Value>,
    file: Option<&'a Value>,
}

/// Within one layer: the first entry whose driver (if set) and pattern match and that is scoped to
/// `profile`, else the first matching entry with no profile. An entry for another profile never
/// matches, and with no `profile` only unscoped entries do.
fn lookup_in<'a>(
    tags: &'a Value,
    driver: &str,
    id: &str,
    profile: Option<&str>,
) -> Option<&'a Value> {
    let entries = tags.get("models")?.as_array()?;
    let fits = |e: &&Value| {
        let d_ok = match e.get("driver").and_then(Value::as_str) {
            Some(d) => d == driver,
            None => true,
        };
        d_ok && e
            .get("match")
            .and_then(Value::as_str)
            .is_some_and(|m| pattern_matches(m, id))
    };
    let scoped = profile.and_then(|p| {
        entries
            .iter()
            .filter(fits)
            .find(|e| e.get("profile").and_then(Value::as_str) == Some(p))
    });
    scoped.or_else(|| {
        entries
            .iter()
            .filter(fits)
            .find(|e| e.get("profile").is_none_or(Value::is_null))
    })
}

/// The winning entry across the layers (user, then file) and the layer's name.
fn lookup<'a>(
    layers: Layers<'a>,
    driver: &str,
    id: &str,
    profile: Option<&str>,
) -> Option<(&'a Value, &'static str)> {
    [(layers.user, "user"), (layers.file, "file")]
        .into_iter()
        .find_map(|(layer, name)| Some((lookup_in(layer?, driver, id, profile)?, name)))
}

/// Provider kinds that cost nothing per token unless a profile says otherwise: the same table
/// agent-terminal uses. Every other kind (`litellm`, `openai_compatible`, an unknown one) is
/// metered.
pub fn kind_is_free(kind: &str) -> bool {
    matches!(
        kind,
        "ollama"
            | "lm_studio"
            | "vllm"
            | "llama_cpp"
            | "sglang"
            | "localai"
            | "jan"
            | "text_generation_webui"
    )
}

/// A profile's own `free_to_run`, else its provider kind's default; no provider is metered.
fn profile_is_free(profile: &Value) -> bool {
    let Some(kind) = profile
        .get("provider")
        .and_then(|p| p.get("kind"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    profile
        .get("free_to_run")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| kind_is_free(kind))
}

/// Provider identity for matching a profile to the server a model list came from: kind and
/// address, ignoring case and a trailing slash.
fn source_key(source: &Value) -> Option<(String, String)> {
    let kind = source.get("kind")?.as_str()?.to_lowercase();
    let url = source.get("base_url")?.as_str()?.trim().to_lowercase();
    Some((kind, url.trim_end_matches('/').to_string()))
}

/// The models the catalogue lists for the server `provider` points at: the primary `local` list
/// when its source (`sources`) is that server, else the matching `others` entry.
fn local_catalogue<'a>(cat: &'a Loaded, provider: &Value) -> &'a [Value] {
    let (Loaded::Ok(c), Some(want)) = (cat, source_key(provider)) else {
        return &[];
    };
    let primary = c
        .get("sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s.get(0).and_then(Value::as_str) == Some(LOCAL))
        .filter(|s| s.get(1).and_then(source_key).as_ref() == Some(&want))
        .find_map(|_| c.get(LOCAL).and_then(Value::as_array));
    let other = || {
        c.get("others")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|o| o.get("driver").and_then(Value::as_str) == Some(LOCAL))
            .filter(|o| o.get("source").and_then(source_key).as_ref() == Some(&want))
            .find_map(|o| o.get("models").and_then(Value::as_array))
    };
    primary.or_else(other).map_or(&[], Vec::as_slice)
}

/// `drivers.local`: the enabled Local server profiles, in file order.
fn local_profiles(config: &Loaded, cat: &Loaded, layers: Layers<'_>) -> Value {
    let profiles: Vec<Value> = match config {
        Loaded::Ok(c) => c
            .get("profiles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|p| p.get("driver").and_then(Value::as_str) == Some(LOCAL))
            .filter(|p| p.get("disabled").and_then(Value::as_bool) != Some(true))
            .filter_map(|p| local_profile(p, cat, layers))
            .collect(),
        _ => Vec::new(),
    };
    json!({ "profiles": profiles })
}

fn local_profile(p: &Value, cat: &Loaded, layers: Layers<'_>) -> Option<Value> {
    let id = p.get("id").and_then(Value::as_str)?;
    let free = profile_is_free(p);
    let provider = p.get("provider").cloned().unwrap_or(Value::Null);
    let models: Vec<Value> = local_catalogue(cat, &provider)
        .iter()
        .filter_map(|m| model_row(m, LOCAL, layers, Some(id), None))
        .map(|mut row| {
            // A Local model has no `--model` value to pass the CLI and no effort control.
            if let Some(o) = row.as_object_mut() {
                for k in ["model_arg", "efforts", "default_effort", "effort_for"] {
                    o.remove(k);
                }
            }
            row
        })
        .collect();
    Some(json!({
        "id": id,
        "name": p.get("name").and_then(Value::as_str).unwrap_or(id),
        "kind": provider.get("kind"),
        "cost": if free { "free" } else { "metered" },
        "available_for_delegation": p
            .get("available_for_delegation")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "headroom": if free { "ok" } else { "unknown" },
        "load": if free { json!(0.0) } else { Value::Null },
        "models": models,
    }))
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

/// A tag file as a layer: usable only as a v1 object; otherwise its state says why.
fn tag_layer<'a>(l: &'a Loaded, path: &Path, hint: &str) -> (Value, Option<&'a Value>) {
    let mut state = state_block(l, path, hint);
    let layer = match l {
        Loaded::Ok(v) if v.get("version").and_then(Value::as_u64) == Some(1) => Some(v),
        Loaded::Ok(_) => {
            state = json!({"state": "invalid", "path": path.display().to_string(),
                "reason": "unsupported or missing version (want 1)"});
            None
        }
        _ => None,
    };
    (state, layer)
}

/// Assemble the status document from the three core files, with no app settings and no user tags
/// (so no Local servers). Pure: all I/O happens in the caller.
#[cfg(test)]
pub fn build(dirs: &Dirs, tags: &Loaded, usage: &Loaded, cat: &Loaded, now: u64) -> Value {
    build_with(dirs, tags, usage, cat, &Extra::none(dirs), now)
}

/// [`build`] plus the app settings (Local servers) and the user's tag layer.
pub fn build_with(
    dirs: &Dirs,
    tags: &Loaded,
    usage: &Loaded,
    cat: &Loaded,
    extra: &Extra,
    now: u64,
) -> Value {
    let (tags_state, tags_v) = tag_layer(
        tags,
        &dirs.tags(),
        "create it to tag models (see the README, \"Model tags\")",
    );
    let (user_state, user_v) = tag_layer(
        &extra.user_tags,
        &extra.user_tags_path,
        "optional: the tags you set in agent-terminal",
    );
    let config_state = state_block(
        &extra.config,
        &dirs.app_config(),
        "agent-terminal has not written its settings yet",
    );
    let layers = Layers {
        user: user_v,
        file: tags_v,
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

    let policy = [user_v, tags_v]
        .into_iter()
        .flatten()
        .find_map(|v| v.get("effort_policy").filter(|p| p.is_object()));
    let mut vocabulary = Map::new();
    for v in [tags_v, user_v].into_iter().flatten() {
        if let Some(o) = v.get("vocabulary").and_then(Value::as_object) {
            vocabulary.extend(o.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
    }
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
                        .filter_map(|m| model_row(m, d, layers, None, policy))
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        drivers.insert(
            d.into(),
            json!({"cost": "metered", "headroom": headroom, "load": load,
                   "usage": usage_block, "models": models}),
        );
    }
    drivers.insert(LOCAL.into(), local_profiles(&extra.config, cat, layers));
    let spread = spread_order(&loads);
    json!({
        "snapshot_at": now,
        "tags": tags_state,
        "user_tags": user_state,
        "config": config_state,
        "usage": usage_state,
        "catalogue": cat_state,
        "vocabulary": vocabulary,
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
    layers: Layers<'_>,
    profile: Option<&str>,
    policy: Option<&Value>,
) -> Option<Value> {
    let id = m.get("id").and_then(Value::as_str)?;
    let found = lookup(layers, driver, id, profile);
    let entry = found.map(|(e, _)| e);
    let field = |k: &str| entry.and_then(|e| e.get(k)).cloned().unwrap_or(Value::Null);
    let efforts = strings(m.get("efforts"));
    Some(json!({
        "id": id,
        "display": m.get("display").and_then(Value::as_str),
        "model_arg": model_arg(driver, id),
        "tier": field("tier"),
        "tags": entry.and_then(|e| e.get("tags")).cloned().unwrap_or_else(|| json!([])),
        "best_for": field("best_for"),
        "tags_source": found.map(|(_, s)| s),
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
        if d == LOCAL {
            continue;
        }
        line(&mut out, String::new());
        line(
            &mut out,
            format!(
                "{d}  cost: {}  headroom: {}  load: {}",
                v["cost"].as_str().unwrap_or("metered"),
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
    let profiles = s["drivers"][LOCAL]["profiles"].as_array();
    line(&mut out, String::new());
    line(
        &mut out,
        "local servers (run only through agent-terminal's cross_agent_spawn)".to_string(),
    );
    if profiles.is_none_or(|p| p.is_empty()) {
        line(&mut out, "  none enabled".to_string());
    }
    for p in profiles.into_iter().flatten() {
        let yes_no = |v: &Value| if v == &json!(true) { "yes" } else { "no" };
        line(
            &mut out,
            format!(
                "  {} [{}]  cost: {}  delegation: {}  headroom: {}  load: {}",
                p["name"].as_str().unwrap_or("?"),
                p["id"].as_str().unwrap_or("?"),
                p["cost"].as_str().unwrap_or("metered"),
                yes_no(&p["available_for_delegation"]),
                p["headroom"].as_str().unwrap_or("unknown"),
                p["load"]
                    .as_f64()
                    .map_or("?".to_string(), |u| format!("{:.0}%", u * 100.0))
            ),
        );
        for m in p["models"].as_array().into_iter().flatten() {
            let tags: Vec<&str> = m["tags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let from = m["tags_source"]
                .as_str()
                .map_or(String::new(), |s| format!(" ({s})"));
            line(
                &mut out,
                format!(
                    "    {:<26} {:<9} [{}] {}{}",
                    m["id"].as_str().unwrap_or("?"),
                    m["tier"].as_str().unwrap_or("-"),
                    tags.join(", "),
                    m["best_for"].as_str().unwrap_or(""),
                    from
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

    fn put(path: &Path, v: Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, v.to_string()).unwrap();
    }

    /// Three Local servers (two sharing a model id), a disabled one, and layered tags.
    fn local_fixture(name: &str) -> (PathBuf, Dirs) {
        let d = tmp(name);
        let dirs = Dirs {
            config: d.join("c"),
            cache: d.join("k"),
        };
        let prof = |id: &str, kind: &str, url: &str, extra: Value| {
            let mut p = json!({"id": id, "name": format!("Server {id}"), "driver": "local",
                "available_for_delegation": true,
                "provider": {"kind": kind, "base_url": url}});
            p.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            p
        };
        put(
            &dirs.app_config(),
            json!({"profiles": [
                {"id": "v1", "name": "Vendor", "driver": "claude"},
                prof("pa", "ollama", "http://a.example/", json!({})),
                prof("pb", "ollama", "http://b.example", json!({"available_for_delegation": false})),
                prof("pc", "litellm", "http://c.example", json!({})),
                prof("pd", "ollama", "http://d.example", json!({"disabled": true})),
                prof("pe", "ollama", "http://e.example", json!({"free_to_run": false})),
                {"id": "pf", "name": "No provider", "driver": "local"},
            ]}),
        );
        put(
            &dirs.catalogue(),
            json!({"claude": [], "agy": [], "codex": [],
                "local": [{"id": "m1"}, {"id": "m2"}, {"id": "zz"}],
                "sources": [["local", {"kind": "ollama", "base_url": "http://a.example"}]],
                "others": [
                    {"driver": "local", "source": {"kind": "ollama", "base_url": "HTTP://b.example/"},
                     "models": [{"id": "m1"}]},
                    {"driver": "local", "source": {"kind": "litellm", "base_url": "http://c.example"},
                     "models": []}]}),
        );
        put(
            &dirs.tags(),
            json!({"version": 1, "vocabulary": {"code": "c", "review": "r"}, "models": [
                {"match": "m*", "driver": "local", "tier": "fast", "tags": ["code"]},
                {"match": "m1", "driver": null, "tier": "deep", "tags": ["review"]}]}),
        );
        put(
            &dirs.user_tags_default(),
            json!({"version": 1, "vocabulary": {"mine": "m"}, "models": [
                {"match": "m1", "driver": "local", "tier": "standard", "tags": ["x"]},
                {"match": "m1", "driver": "local", "profile": "pb", "tier": "deep",
                 "tags": ["code", "review"], "best_for": "scoped"},
                {"match": "m2", "profile": "pc", "tags": ["never"]}]}),
        );
        (d, dirs)
    }

    fn profile<'a>(s: &'a Value, id: &str) -> &'a Value {
        s["drivers"]["local"]["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap_or(&Value::Null)
    }

    #[test]
    fn default_cost_follows_the_provider_kind() {
        for kind in [
            "ollama",
            "lm_studio",
            "vllm",
            "llama_cpp",
            "sglang",
            "localai",
            "jan",
            "text_generation_webui",
        ] {
            assert!(kind_is_free(kind), "{kind}");
        }
        for kind in ["litellm", "openai_compatible", "something_new", ""] {
            assert!(!kind_is_free(kind), "{kind}");
        }
        let p = |v: Value| profile_is_free(&v);
        assert!(p(json!({"provider": {"kind": "vllm"}})));
        assert!(!p(json!({"provider": {"kind": "litellm"}})));
        assert!(
            !p(json!({"free_to_run": true})),
            "no provider is never free"
        );
        assert!(p(
            json!({"free_to_run": true, "provider": {"kind": "litellm"}})
        ));
        assert!(!p(
            json!({"free_to_run": false, "provider": {"kind": "ollama"}})
        ));
        assert!(p(json!({"free_to_run": null, "provider": {"kind": "jan"}})));
    }

    #[test]
    fn local_profiles_report_cost_usage_and_only_enabled_ones() {
        let (d, dirs) = local_fixture("local-shape");
        let s = snapshot(&dirs);
        let ids: Vec<&str> = s["drivers"]["local"]["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            ["pa", "pb", "pc", "pe", "pf"],
            "pd disabled, v1 not local"
        );
        let pa = profile(&s, "pa");
        assert_eq!(pa["name"], "Server pa");
        assert_eq!(pa["cost"], "free");
        assert_eq!(pa["headroom"], "ok");
        assert_eq!(pa["load"], 0.0);
        assert_eq!(pa["available_for_delegation"], true);
        assert_eq!(profile(&s, "pb")["available_for_delegation"], false);
        for id in ["pc", "pe", "pf"] {
            let p = profile(&s, id);
            assert_eq!(p["cost"], "metered", "{id}");
            assert_eq!(p["headroom"], "unknown", "{id}");
            assert!(p["load"].is_null(), "{id}");
        }
        // Models come from the server the profile points at, matched ignoring case and a slash.
        assert_eq!(pa["models"].as_array().unwrap().len(), 3);
        assert_eq!(profile(&s, "pb")["models"].as_array().unwrap().len(), 1);
        assert_eq!(profile(&s, "pc")["models"], json!([]));
        let m = &pa["models"][0];
        for k in ["id", "tier", "tags", "best_for", "tags_source"] {
            assert!(m.get(k).is_some(), "{k}");
        }
        assert!(m.get("model_arg").is_none());
        // Vendors are metered; add-only fields leave the old ones in place.
        for d in DRIVERS {
            assert_eq!(s["drivers"][d]["cost"], "metered");
            assert!(s["drivers"][d].get("headroom").is_some());
        }
        assert_eq!(s["spread_order"], json!(["claude", "agy", "codex"]));
        assert_eq!(s["user_tags"]["state"], "ok");
        assert_eq!(s["config"]["state"], "ok");
        let table = render_table(&s);
        assert!(
            table.contains("Server pa [pa]  cost: free  delegation: yes"),
            "{table}"
        );
        assert!(table.contains("Server pc [pc]  cost: metered"), "{table}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn user_layer_beats_file_and_a_profile_scope_beats_an_unscoped_entry() {
        let (d, dirs) = local_fixture("local-layers");
        let s = snapshot(&dirs);
        // pa m1: the unscoped user entry (the pb one is for another profile).
        let m1a = &profile(&s, "pa")["models"][0];
        assert_eq!(m1a["tags_source"], "user");
        assert_eq!(m1a["tier"], "standard");
        assert_eq!(m1a["tags"], json!(["x"]));
        // pb m1: same model id, its own scoped entry wins although the unscoped one is first.
        let m1b = &profile(&s, "pb")["models"][0];
        assert_eq!(m1b["tags_source"], "user");
        assert_eq!(m1b["tier"], "deep");
        assert_eq!(m1b["best_for"], "scoped");
        // pa m2: no user entry applies (the only one is scoped to pc), so the file's wins.
        let m2 = &profile(&s, "pa")["models"][1];
        assert_eq!(m2["tags_source"], "file");
        assert_eq!(m2["tags"], json!(["code"]));
        // A model nothing matches has no source.
        let zz = &profile(&s, "pa")["models"][2];
        assert!(zz["tags_source"].is_null());
        assert_eq!(zz["tags"], json!([]));
        // The user layer also tags a vendor model and its vocabulary joins the file's.
        assert_eq!(s["vocabulary"]["mine"], "m");
        assert_eq!(s["vocabulary"]["code"], "c");
        // Without the user file the file layer answers for everyone.
        std::fs::remove_file(dirs.user_tags_default()).unwrap();
        let s = snapshot(&dirs);
        assert_eq!(s["user_tags"]["state"], "missing");
        let m1b = &profile(&s, "pb")["models"][0];
        assert_eq!(m1b["tags_source"], "file");
        assert_eq!(m1b["tier"], "fast", "first file entry in file order");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn user_tags_apply_to_vendor_models_too() {
        let (d, dirs) = local_fixture("local-vendor");
        put(
            &dirs.catalogue(),
            json!({"claude": [{"id": "opus"}, {"id": "haiku"}], "agy": [], "codex": []}),
        );
        put(
            &dirs.user_tags_default(),
            json!({"version": 1, "models": [
                {"match": "opus", "driver": "claude", "tier": "deep", "tags": ["mine"]},
                {"match": "haiku", "profile": "pa", "tier": "fast"}]}),
        );
        put(
            &dirs.tags(),
            json!({"version": 1, "models": [{"match": "*", "tier": "standard", "tags": ["f"]}]}),
        );
        let s = snapshot(&dirs);
        let claude = &s["drivers"]["claude"]["models"];
        assert_eq!(claude[0]["tags_source"], "user");
        assert_eq!(claude[0]["tier"], "deep");
        assert_eq!(
            claude[1]["tags_source"], "file",
            "a profile-scoped entry skips vendors"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn user_tag_path_comes_from_the_settings_when_absolute() {
        let (d, dirs) = local_fixture("local-path");
        let home = Path::new("/home/example");
        let cfg = |v: Value| Loaded::Ok(json!({ "user_model_tags_path": v }));
        let default = dirs.user_tags_default();
        assert_eq!(user_tags_path(&dirs, &Loaded::Missing, home), default);
        assert_eq!(user_tags_path(&dirs, &cfg(json!("  ")), home), default);
        assert_eq!(
            user_tags_path(&dirs, &cfg(json!("rel/x.json")), home),
            default
        );
        assert_eq!(user_tags_path(&dirs, &cfg(json!(7)), home), default);
        assert_eq!(
            user_tags_path(&dirs, &cfg(json!("/etc/example/t.json")), home),
            Path::new("/etc/example/t.json")
        );
        assert_eq!(
            user_tags_path(&dirs, &cfg(json!("~/t.json")), home),
            Path::new("/home/example/t.json")
        );
        // End to end: the moved file is the one read.
        let moved = d.join("elsewhere/t.json");
        put(
            &moved,
            json!({"version": 1, "models": [{"match": "m1", "tier": "deep"}]}),
        );
        put(
            &dirs.app_config(),
            json!({"user_model_tags_path": moved.display().to_string(), "profiles": []}),
        );
        let s = snapshot(&dirs);
        assert_eq!(s["user_tags"]["path"], moved.display().to_string());
        assert_eq!(s["user_tags"]["state"], "ok");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn inputs_are_bounded_regular_files() {
        let d = tmp("bounded");
        assert!(matches!(load(&d), Loaded::Invalid(r) if r.contains("regular")));
        let big = d.join("big.json");
        std::fs::write(&big, vec![b' '; MAX_INPUT_BYTES as usize + 1]).unwrap();
        assert!(matches!(load(&big), Loaded::Invalid(r) if r.contains("larger")));
        let ok = d.join("ok.json");
        std::fs::write(&ok, "{}").unwrap();
        assert!(matches!(load(&ok), Loaded::Ok(_)));
        assert!(matches!(load(&d.join("none.json")), Loaded::Missing));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_settings_means_no_local_servers() {
        let d = tmp("nosettings");
        let dirs = Dirs {
            config: d.join("c"),
            cache: d.join("k"),
        };
        let s = snapshot(&dirs);
        assert_eq!(s["config"]["state"], "missing");
        assert_eq!(s["drivers"]["local"]["profiles"], json!([]));
        assert!(render_table(&s).contains("none enabled"));
        let _ = std::fs::remove_dir_all(&d);
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
