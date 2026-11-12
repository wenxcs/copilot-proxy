//! Model access rules and persistent, fixed-window token budgets.

use crate::error::Error;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

const FIVE_HOURS: u64 = 5 * 60 * 60;
const SEVEN_DAYS: u64 = 7 * 24 * 60 * 60;

#[derive(Default)]
struct Limits {
    five_hour: Option<u64>,
    seven_day: Option<u64>,
}

struct Rules {
    default_allowed: bool,
    overrides: BTreeMap<String, bool>,
    limits: BTreeMap<String, Limits>,
}

impl Rules {
    fn parse(source: &str) -> Result<Self, Error> {
        let mut rules = Self {
            default_allowed: true,
            overrides: BTreeMap::new(),
            limits: BTreeMap::new(),
        };
        for directive in source.split(|c: char| c == ',' || c.is_whitespace()) {
            if directive.is_empty() {
                continue;
            }
            if let Some(model) = directive.strip_prefix(['+', '-']) {
                let allowed = directive.starts_with('+');
                if model == "all" {
                    rules.default_allowed = allowed;
                    rules.overrides.clear();
                } else {
                    validate_model(model)?;
                    rules.overrides.insert(model.to_string(), allowed);
                }
                continue;
            }
            let mut fields = directive.split(':');
            let window = fields.next().unwrap_or_default();
            let model = fields.next().unwrap_or_default();
            let limit = fields
                .next()
                .filter(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|value| value.parse::<u64>().ok());
            if !matches!(window, "5h" | "7d") || fields.next().is_some() || limit.is_none() {
                return Err(Error::Config(
                    "Model rules must use +/-model, +/-all, 5h:model:tokens, or 7d:model:tokens"
                        .to_string(),
                ));
            }
            validate_model(model)?;
            if model == "all" {
                return Err(Error::Config(
                    "Token limits must name a specific model, not all".to_string(),
                ));
            }
            let limits = rules.limits.entry(model.to_string()).or_default();
            if window == "5h" {
                limits.five_hour = limit;
            } else {
                limits.seven_day = limit;
            }
        }
        Ok(rules)
    }

    fn allowed(&self, model: &str) -> bool {
        self.overrides
            .get(model)
            .copied()
            .unwrap_or(self.default_allowed)
    }
}

fn validate_model(model: &str) -> Result<(), Error> {
    if model.is_empty()
        || !model
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b",:".contains(&b))
    {
        return Err(Error::Config(
            "Model names in policy rules must be nonempty ASCII without whitespace, commas, or colons"
                .to_string(),
        ));
    }
    Ok(())
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Window {
    start: u64,
    used: u64,
}

impl Window {
    fn advance(&mut self, now: u64, duration: u64) {
        let periods = now.saturating_sub(self.start) / duration;
        if periods > 0 {
            self.start = self.start.saturating_add(periods.saturating_mul(duration));
            self.used = 0;
        }
    }

    fn reset_at(&self, duration: u64) -> u64 {
        self.start.saturating_add(duration)
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelUsage {
    five_hour: Option<Window>,
    seven_day: Option<Window>,
    pending: bool,
}

impl ModelUsage {
    fn advance(&mut self, now: u64) {
        if let Some(window) = &mut self.five_hour {
            window.advance(now, FIVE_HOURS);
        }
        if let Some(window) = &mut self.seven_day {
            window.advance(now, SEVEN_DAYS);
        }
    }

    fn initialize(&mut self, limits: &Limits, now: u64) {
        if limits.five_hour.is_some() && self.five_hour.is_none() {
            self.five_hour = Some(Window {
                start: now,
                used: 0,
            });
        }
        if limits.seven_day.is_some() && self.seven_day.is_none() {
            self.seven_day = Some(Window {
                start: now,
                used: 0,
            });
        }
        self.advance(now);
    }

    fn charge(&mut self, limits: &Limits, tokens: Option<u64>) {
        for (window, limit) in [
            (&mut self.five_hour, limits.five_hour),
            (&mut self.seven_day, limits.seven_day),
        ] {
            if let Some(window) = window {
                window.used = match tokens {
                    Some(tokens) => window.used.saturating_add(tokens),
                    None => window.used.max(limit.unwrap_or(u64::MAX)),
                };
            }
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u8,
    generation: u64,
    models: BTreeMap<String, ModelUsage>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: 1,
            generation: 0,
            models: BTreeMap::new(),
        }
    }
}

pub(crate) struct ModelPolicy {
    rules: Rules,
    state: Mutex<Ledger>,
    state_path: Option<PathBuf>,
    // Keep an OS lock open for this policy's lifetime. Process death releases it.
    _lock: Option<File>,
}

impl ModelPolicy {
    pub(crate) fn new(rules: &str, state_path: Option<PathBuf>) -> Result<Arc<Self>, Error> {
        let rules = Rules::parse(rules)?;
        let lock = state_path.as_deref().map(lock_state_file).transpose()?;
        let mut state = match state_path.as_deref().map(fs::read).transpose() {
            Ok(Some(contents)) => serde_json::from_slice::<Ledger>(&contents).map_err(|_| {
                Error::Config(
                    "Model usage state is invalid JSON or has an invalid schema".to_string(),
                )
            })?,
            Ok(None) => Ledger::default(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ledger::default(),
            Err(error) => return Err(error.into()),
        };
        if state.version != 1 {
            return Err(Error::Config(
                "Unsupported model usage state version".to_string(),
            ));
        }
        // A crash cannot erase an outstanding request whose final usage was lost.
        let mut recovered = false;
        for (model, usage) in &mut state.models {
            if usage.pending {
                usage.charge(rules.limits.get(model).unwrap_or(&Limits::default()), None);
                usage.pending = false;
                recovered = true;
            }
        }
        let policy = Arc::new(Self {
            rules,
            state: Mutex::new(state),
            state_path,
            _lock: lock,
        });
        if recovered {
            let state = policy.lock()?;
            policy.persist(&state)?;
        }
        Ok(policy)
    }

    pub(crate) fn is_enabled(&self) -> bool {
        !self.rules.default_allowed
            || !self.rules.overrides.is_empty()
            || !self.rules.limits.is_empty()
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        model: &str,
        billable: bool,
    ) -> Result<Option<UsagePermit>, Error> {
        self.begin_at(model, billable, now())
    }

    fn begin_at(
        self: &Arc<Self>,
        model: &str,
        billable: bool,
        now: u64,
    ) -> Result<Option<UsagePermit>, Error> {
        if !self.rules.allowed(model) {
            return Err(Error::ModelDenied(model.to_string()));
        }
        let Some(limits) = self.rules.limits.get(model).filter(|_| billable) else {
            return Ok(None);
        };
        let mut state = self.lock()?;
        let generation = state.generation;
        let usage = state.models.entry(model.to_string()).or_default();
        usage.initialize(limits, now);
        check_limits(model, usage, limits, now, false)?;
        if usage.pending {
            return Err(Error::ModelBusy(model.to_string()));
        }
        usage.pending = true;
        if let Err(error) = self.persist(&state) {
            state.models.get_mut(model).unwrap().pending = false;
            return Err(error);
        }
        Ok(Some(UsagePermit {
            policy: Arc::clone(self),
            model: model.to_string(),
            generation,
            recorded: 0,
            finished: false,
        }))
    }

    pub(crate) fn snapshot(&self) -> Result<Value, Error> {
        self.snapshot_at(now())
    }

    fn snapshot_at(&self, now: u64) -> Result<Value, Error> {
        let mut state = self.lock()?;
        for usage in state.models.values_mut() {
            usage.advance(now);
        }
        let models: BTreeSet<_> = self
            .rules
            .limits
            .keys()
            .chain(self.rules.overrides.keys())
            .chain(state.models.keys())
            .collect();
        let mut entries = serde_json::Map::new();
        for model in models {
            let usage = state.models.get(model);
            let limits = self.rules.limits.get(model);
            let mut entry = json!({
                "allowed": self.rules.allowed(model),
                "in_flight": usage.is_some_and(|usage| usage.pending),
            });
            for (name, window, limit, duration) in [
                (
                    "5h",
                    usage.and_then(|usage| usage.five_hour.as_ref()),
                    limits.and_then(|limits| limits.five_hour),
                    FIVE_HOURS,
                ),
                (
                    "7d",
                    usage.and_then(|usage| usage.seven_day.as_ref()),
                    limits.and_then(|limits| limits.seven_day),
                    SEVEN_DAYS,
                ),
            ] {
                if let Some(limit) = limit {
                    let used = window.map_or(0, |window| window.used);
                    entry[name] = json!({ "limit": limit, "used": used, "remaining": limit.saturating_sub(used), "reset_at": window.map(|window| window.reset_at(duration)) });
                }
            }
            entries.insert(model.clone(), entry);
        }
        self.persist(&state)?;
        Ok(json!({
            "default_allowed": self.rules.default_allowed,
            "model_rules": self.rules.overrides,
            "models": entries,
        }))
    }

    pub(crate) fn reset_all(&self) -> Result<(), Error> {
        let mut state = self.lock()?;
        let cleared = Ledger {
            generation: state.generation.wrapping_add(1),
            ..Ledger::default()
        };
        // A failed disk write must not report a reset that was never persisted.
        self.persist(&cleared)?;
        *state = cleared;
        Ok(())
    }

    fn account(
        &self,
        model: &str,
        generation: u64,
        tokens: Option<u64>,
        finished: bool,
        now: u64,
    ) -> Result<(), Error> {
        let mut state = self.lock()?;
        if state.generation != generation {
            return Ok(());
        }
        let Some(usage) = state.models.get_mut(model).filter(|usage| usage.pending) else {
            return Ok(());
        };
        let limits = &self.rules.limits[model];
        usage.advance(now);
        usage.charge(limits, tokens);
        if finished {
            usage.pending = false;
        }
        let exceeded = if tokens.is_some() {
            check_limits(model, usage, limits, now, true)
        } else {
            Ok(())
        };
        self.persist(&state)?;
        exceeded
    }

    fn lock(&self) -> Result<MutexGuard<'_, Ledger>, Error> {
        self.state
            .lock()
            .map_err(|_| Error::Config("Model usage state lock is poisoned".to_string()))
    }

    fn persist(&self, state: &Ledger) -> Result<(), Error> {
        let Some(path) = &self.state_path else {
            return Ok(());
        };
        let contents = serde_json::to_vec(state).map_err(|error| {
            Error::Config(format!("Could not serialize model usage state: {error}"))
        })?;
        let temp = parent_dir(path).join(format!(".model-usage-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<(), Error> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(&contents)?;
            file.sync_all()?;
            fs::rename(&temp, path)?;
            File::open(parent_dir(path))?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}

fn check_limits(
    model: &str,
    usage: &ModelUsage,
    limits: &Limits,
    now: u64,
    after_charge: bool,
) -> Result<(), Error> {
    let mut blocked: Option<(&str, u64, u64, u64)> = None;
    for (name, window, limit, duration) in [
        ("5h", usage.five_hour.as_ref(), limits.five_hour, FIVE_HOURS),
        ("7d", usage.seven_day.as_ref(), limits.seven_day, SEVEN_DAYS),
    ] {
        if let (Some(window), Some(limit)) = (window, limit)
            && (window.used > limit || (!after_charge && window.used == limit))
        {
            let retry_after = window.reset_at(duration).saturating_sub(now).max(1);
            if blocked.is_none_or(|(_, _, _, wait)| retry_after > wait) {
                blocked = Some((name, limit, window.used, retry_after));
            }
        }
    }
    match blocked {
        Some((window, limit, used, retry_after)) => Err(Error::LimitExceeded {
            model: model.to_string(),
            window: window.to_string(),
            limit,
            used,
            retry_after,
        }),
        None => Ok(()),
    }
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn lock_state_file(path: &Path) -> Result<File, Error> {
    // Do not change permissions of preexisting arbitrary parent directories.
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent_dir(path))?;
    let lock_path = path.with_file_name(format!(
        "{}.lock",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)?;
    file.set_permissions(Permissions::from_mode(0o600))?;
    file.try_lock().map_err(|error| {
        Error::Config(format!(
            "Model usage state is already in use or could not be locked: {error}"
        ))
    })?;
    Ok(file)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) struct UsagePermit {
    policy: Arc<ModelPolicy>,
    model: String,
    generation: u64,
    recorded: u64,
    finished: bool,
}

impl UsagePermit {
    pub(crate) fn record(&mut self, total: u64) -> Result<(), Error> {
        self.record_at(total, now())
    }

    fn record_at(&mut self, total: u64, now: u64) -> Result<(), Error> {
        let delta = total.saturating_sub(self.recorded);
        self.recorded = self.recorded.max(total);
        self.policy
            .account(&self.model, self.generation, Some(delta), false, now)
    }

    pub(crate) fn finish(self, total: Option<u64>) -> Result<(), Error> {
        self.finish_at(total, now())
    }

    fn finish_at(mut self, total: Option<u64>, now: u64) -> Result<(), Error> {
        self.finished = true;
        self.policy.account(
            &self.model,
            self.generation,
            total.map(|total| total.saturating_sub(self.recorded)),
            true,
            now,
        )
    }
}

impl Drop for UsagePermit {
    fn drop(&mut self) {
        if !self.finished
            && let Err(error) = self
                .policy
                .account(&self.model, self.generation, None, true, now())
        {
            tracing::error!(%error, "Failed to account for interrupted model request");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(rules: &str) -> Arc<ModelPolicy> {
        ModelPolicy::new(rules, None).unwrap()
    }

    fn permit(policy: &Arc<ModelPolicy>, model: &str, now: u64) -> UsagePermit {
        policy.begin_at(model, true, now).unwrap().unwrap()
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("model-policy-test-{}", uuid::Uuid::new_v4()));
            DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> PathBuf {
            self.0.join("private/usage.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parses_sequential_access_rules_and_independent_limits() {
        let rules = Rules::parse(
            "-all,+gpt-6-astra 5h:gpt-6-astra:1024,7d:gpt-6-astra:2048 -gpt-6-astra +gpt-6-astra",
        )
        .unwrap();
        assert!(rules.allowed("gpt-6-astra"));
        assert!(!rules.allowed("other"));
        assert_eq!(rules.limits["gpt-6-astra"].five_hour, Some(1024));
        assert_eq!(rules.limits["gpt-6-astra"].seven_day, Some(2048));
        let reset = Rules::parse("-all,+first,+all,-second,5h:second:1").unwrap();
        assert!(reset.allowed("first"));
        assert!(!reset.allowed("second"));
        assert_eq!(reset.overrides.len(), 1);
        assert_eq!(reset.limits.len(), 1);
        assert_eq!(
            Rules::parse("5h:model:12,5h:model:3").unwrap().limits["model"].five_hour,
            Some(3)
        );
    }

    #[test]
    fn rejects_invalid_rules_and_limit_overflow() {
        for rule in [
            "model",
            "+",
            "-",
            "8h:model:1",
            "5h::1",
            "7d:model:",
            "5h:model:-1",
            "5h:model:+1",
            "5h:model:1.0",
            "5h:model:1:2",
            "5h:model:18446744073709551616",
            "5h:all:1",
            "+model:name",
        ] {
            assert!(Rules::parse(rule).is_err(), "{rule}");
        }
    }

    #[test]
    fn default_policy_passthrough_and_denied_models_need_no_ledger() {
        let default = policy("");
        assert!(!default.is_enabled());
        assert!(default.begin_at("any", true, 10).unwrap().is_none());
        let restricted = policy("-all,+good");
        assert!(restricted.is_enabled());
        assert!(matches!(
            restricted.begin_at("bad", true, 10),
            Err(Error::ModelDenied(_))
        ));
        assert!(restricted.begin_at("good", true, 10).unwrap().is_none());
        assert!(restricted.lock().unwrap().models.is_empty());
    }

    #[test]
    fn actual_tokens_and_cumulative_updates_count_once() {
        let policy = policy("5h:model:100,7d:model:200");
        let mut active = permit(&policy, "model", 10);
        active.record_at(30, 11).unwrap();
        active.record_at(30, 12).unwrap();
        active.record_at(20, 13).unwrap();
        active.finish_at(Some(50), 14).unwrap();
        let snapshot = policy.snapshot_at(14).unwrap();
        assert_eq!(snapshot["models"]["model"]["5h"]["used"], 50);
        assert_eq!(snapshot["models"]["model"]["7d"]["used"], 50);
        assert_eq!(snapshot["models"]["model"]["5h"]["remaining"], 50);
        assert_eq!(snapshot["models"]["model"]["in_flight"], false);
    }

    #[test]
    fn fixed_windows_do_not_slide_and_expire_independently() {
        let policy = policy("5h:model:100,7d:model:300");
        permit(&policy, "model", 100)
            .finish_at(Some(70), 100)
            .unwrap();
        permit(&policy, "model", 100 + FIVE_HOURS - 1)
            .finish_at(Some(30), 100 + FIVE_HOURS - 1)
            .unwrap();
        assert!(
            matches!(policy.begin_at("model", true, 100 + FIVE_HOURS - 1), Err(Error::LimitExceeded { window, .. }) if window == "5h")
        );
        permit(&policy, "model", 100 + FIVE_HOURS)
            .finish_at(Some(20), 100 + FIVE_HOURS)
            .unwrap();
        let snapshot = policy.snapshot_at(100 + FIVE_HOURS).unwrap();
        assert_eq!(snapshot["models"]["model"]["5h"]["used"], 20);
        assert_eq!(
            snapshot["models"]["model"]["5h"]["reset_at"],
            100 + 2 * FIVE_HOURS
        );
        assert_eq!(snapshot["models"]["model"]["7d"]["used"], 120);
        assert_eq!(
            snapshot["models"]["model"]["7d"]["reset_at"],
            100 + SEVEN_DAYS
        );
        let expired = policy.snapshot_at(100 + SEVEN_DAYS).unwrap();
        assert_eq!(expired["models"]["model"]["7d"]["used"], 0);
    }

    #[test]
    fn models_and_five_hour_seven_day_caps_are_independent() {
        let policy = policy("5h:a:200,7d:a:100,5h:b:20");
        permit(&policy, "a", 10).finish_at(Some(100), 10).unwrap();
        assert!(
            matches!(policy.begin_at("a", true, 11), Err(Error::LimitExceeded { window, .. }) if window == "7d")
        );
        permit(&policy, "b", 11).finish_at(Some(20), 11).unwrap();
        assert!(
            matches!(policy.begin_at("b", true, 12), Err(Error::LimitExceeded { window, .. }) if window == "5h")
        );
    }

    #[test]
    fn rejects_concurrent_metered_requests_but_not_free_model_checks() {
        let policy = policy("5h:model:10");
        let active = permit(&policy, "model", 10);
        assert!(matches!(
            policy.begin_at("model", true, 10),
            Err(Error::ModelBusy(_))
        ));
        assert!(policy.begin_at("model", false, 10).unwrap().is_none());
        active.finish_at(Some(1), 11).unwrap();
        permit(&policy, "model", 12).finish_at(Some(1), 12).unwrap();
    }

    #[test]
    fn over_limit_response_is_accounted_and_reports_retry_after() {
        let policy = policy("5h:model:10");
        assert!(
            matches!(permit(&policy, "model", 100).finish_at(Some(11), 102), Err(Error::LimitExceeded { limit:10, used:11, retry_after, .. }) if retry_after == FIVE_HOURS - 2)
        );
        assert_eq!(
            policy.snapshot_at(102).unwrap()["models"]["model"]["5h"]["used"],
            11
        );
        assert!(matches!(
            policy.begin_at("model", true, 103),
            Err(Error::LimitExceeded { used: 11, .. })
        ));
    }

    #[test]
    fn reports_longest_wait_when_multiple_windows_are_exhausted() {
        let policy = policy("5h:model:10,7d:model:10");
        permit(&policy, "model", 100)
            .finish_at(Some(10), 100)
            .unwrap();
        assert!(matches!(
            policy.begin_at("model", true, 101),
            Err(Error::LimitExceeded { window, limit: 10, used: 10, retry_after, .. })
                if window == "7d" && retry_after == SEVEN_DAYS - 1
        ));
    }

    #[test]
    fn zero_limits_block_before_upstream_but_not_token_counting() {
        let policy = policy("5h:model:0");
        assert!(matches!(
            policy.begin_at("model", true, 1),
            Err(Error::LimitExceeded { limit: 0, .. })
        ));
        assert!(policy.begin_at("model", false, 1).unwrap().is_none());
    }

    #[test]
    fn reset_clears_all_models_and_invalidates_active_permits() {
        let policy = policy("5h:a:10,7d:a:100,5h:b:10");
        let mut old = permit(&policy, "a", 100);
        old.record_at(5, 100).unwrap();
        permit(&policy, "b", 100).finish_at(Some(10), 100).unwrap();
        policy.reset_all().unwrap();
        let new = permit(&policy, "a", 101);
        old.finish_at(Some(99), 101).unwrap();
        assert!(matches!(
            policy.begin_at("a", true, 101),
            Err(Error::ModelBusy(_))
        ));
        new.finish_at(Some(2), 101).unwrap();
        let snapshot = policy.snapshot_at(101).unwrap();
        assert_eq!(snapshot["models"]["a"]["5h"]["used"], 2);
        assert_eq!(snapshot["models"]["b"]["5h"]["used"], 0);
    }

    #[test]
    fn missing_usage_or_dropped_permit_exhausts_windows() {
        let policy = policy("5h:a:10,7d:a:100,5h:b:20");
        permit(&policy, "a", 100).finish_at(None, 100).unwrap();
        assert_eq!(
            policy.snapshot_at(100).unwrap()["models"]["a"]["7d"]["used"],
            100
        );
        let time = now();
        drop(permit(&policy, "b", time));
        assert!(matches!(
            policy.begin_at("b", true, time),
            Err(Error::LimitExceeded { used: 20, .. })
        ));
    }

    #[test]
    fn persists_usage_privately_and_prevents_multiple_writers() {
        let temp = TempDir::new();
        let path = temp.path();
        {
            let policy = ModelPolicy::new("5h:model:100", Some(path.clone())).unwrap();
            assert!(ModelPolicy::new("5h:model:100", Some(path.clone())).is_err());
            permit(&policy, "model", 100)
                .finish_at(Some(40), 100)
                .unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let restored = ModelPolicy::new("5h:model:80", Some(path)).unwrap();
        assert_eq!(
            restored.snapshot_at(100).unwrap()["models"]["model"]["5h"]["used"],
            40
        );
        assert_eq!(
            restored.snapshot_at(100).unwrap()["models"]["model"]["5h"]["remaining"],
            40
        );
    }

    #[test]
    fn persists_reset_and_preserves_usage_across_removed_limits() {
        let temp = TempDir::new();
        let path = temp.path();
        {
            let policy = ModelPolicy::new("5h:model:100", Some(path.clone())).unwrap();
            permit(&policy, "model", 100)
                .finish_at(Some(40), 100)
                .unwrap();
        }
        {
            let disabled = ModelPolicy::new("", Some(path.clone())).unwrap();
            assert!(!disabled.is_enabled());
        }
        {
            let restored = ModelPolicy::new("5h:model:100", Some(path.clone())).unwrap();
            assert_eq!(
                restored.snapshot_at(100).unwrap()["models"]["model"]["5h"]["used"],
                40
            );
            restored.reset_all().unwrap();
        }
        let reset = ModelPolicy::new("5h:model:100", Some(path)).unwrap();
        assert_eq!(
            reset.snapshot_at(100).unwrap()["models"]["model"]["5h"]["used"],
            0
        );
    }

    #[test]
    fn crash_recovery_charges_unfinished_persisted_requests() {
        let temp = TempDir::new();
        let path = temp.path();
        {
            let policy = ModelPolicy::new("5h:model:100,7d:model:200", Some(path.clone())).unwrap();
            let mut active = permit(&policy, "model", 100);
            active.record_at(10, 100).unwrap();
            // Simulate process death after the persisted pending marker.
            active.finished = true;
        }
        let restored = ModelPolicy::new("5h:model:100,7d:model:200", Some(path)).unwrap();
        assert!(matches!(
            restored.begin_at("model", true, 101),
            Err(Error::LimitExceeded { used: 200, window, .. }) if window == "7d"
        ));
        assert_eq!(
            restored.snapshot_at(101).unwrap()["models"]["model"]["7d"]["used"],
            200
        );
    }

    #[test]
    fn rejects_corrupt_or_unreadable_state_instead_of_resetting() {
        let temp = TempDir::new();
        let path = temp.path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for bytes in [
            "bad json",
            "{}",
            r#"{"version":2,"generation":0,"models":{}}"#,
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(ModelPolicy::new("5h:model:100", Some(path.clone())).is_err());
        }
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(ModelPolicy::new("5h:model:100", Some(path)).is_err());
    }
}
