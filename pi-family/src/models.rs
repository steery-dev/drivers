//! `models`: what the harness runs here, each as its `--model` takes it exactly.
//!
//! omp lists its catalog with `omp models --json`, its default being `modelRoles.default`
//! in its `config.yml`. pi lists its models two ways, both needed: `pi --list-models`
//! prints every model it has a sign-in for, in its own order, and its RPC mode's
//! `get_available_models` says each one's thinking levels and price. pi names no default.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use steery_driver_common::program::helper;
use steery_sdk::wire::harness::{Model, Models};
use steery_sdk::{Error, refusal};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::sync::watch;

use crate::harness::Harness;

/// How long the harness has to answer `models`: omp took over 20 s on a Mac that had just
/// started and was swapping.
const LISTED_WITHIN: Duration = Duration::from_secs(25);
/// How long a `start` waits to learn whether omp lists the model it was told to run, where
/// no catalog it listed before names it: inside `start`'s 10 s, with room to answer.
const A_TURN_WAITS: Duration = Duration::from_secs(8);
/// pi's thinking levels, lowest first.
const PI_THINKING: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
/// The settings pi's catalog and reader run with, in their private folder: none of pi's
/// own tools reaches a model there.
pub const PI_SETTINGS: &str =
    r#"{"extensions":["-builtin:mcp","-builtin:codemode","-builtin:tool-search"]}"#;

/// The catalog as the harness listed it last, and the ask of it in flight: one per driver,
/// shared by every call.
#[derive(Default, Clone)]
pub struct Catalog(Arc<Kept>);

#[derive(Default)]
struct Kept {
    listed: Mutex<Option<Vec<Model>>>,
    asking: Mutex<Option<Asking>>,
}

/// An ask of the harness's list: `None` while it runs, then what it gave.
type Asking = watch::Receiver<Option<Result<Vec<Model>, String>>>;

impl Catalog {
    /// Ask omp for its list as the driver starts, so a `start` right after finds it. A
    /// driver the runtime started again had none: the runtime keeps its own list and asks
    /// only after 10 minutes, so each start with a pick ran `omp models` itself within 8 s,
    /// which a Mac under builds missed (2026-10-15 18:10, 13.6 s; 10-14 09:30, over 25 s).
    pub fn warm(&self, harness: Harness, root: &Path) {
        if harness == Harness::Omp {
            drop(self.asking(harness, root, false));
        }
    }

    /// `models`: every model the harness has a sign-in for, as it lists them, the default
    /// first, and the files whose change changes the list. `no-binary` where the harness
    /// is not here; a list it will not give is refused with why. Always asked afresh: the
    /// runtime asks when what decides the list changed.
    pub async fn list(&self, harness: Harness, home: &Path, root: &Path) -> Result<Models, Error> {
        harness.need_binary()?;
        let models = answer(self.asking(harness, root, true))
            .await
            .map_err(|why| Error::refused(refusal::REFUSED, why))?;
        Ok(Models {
            models,
            unlisted: None,
            watch: watched(harness, home, root),
            signed_in: None,
        })
    }

    /// Whether omp runs `model` as itself: one of its catalog's entries, which its
    /// `--model` finds exactly. omp resolves anything else loosely and runs what it found
    /// without a word, so a pick it does not list is never handed to it.
    ///
    /// A catalog omp listed before that names `model` answers, however old: an entry goes
    /// only when omp's configuration or sign-ins change, which the runtime watches and
    /// answers by asking `models` again, which lists afresh here. Only a model no catalog
    /// here names is asked about, within what a `start` may wait. A start once threw away
    /// a catalog naming the conversation's own model the moment it was ten minutes old —
    /// as long as the runtime keeps a list before asking again — and `omp models` then
    /// took over 8 s on a busy Mac, so a resume and each of its Retries failed "omp did not
    /// list its models within 8 s" (2026-10-13 and 10-14, on two members' Macs).
    ///
    /// The ask joins one in flight, and a start that stops waiting leaves it running: a
    /// slow `omp models` killed with each start never finished, so no Retry could pass.
    pub async fn lists(&self, root: &Path, model: &str) -> Result<bool, String> {
        let named = |models: &[Model]| models.iter().any(|offer| offer.model == model);
        if self.listed().as_deref().is_some_and(named) {
            return Ok(true);
        }
        let asked = answer(self.asking(Harness::Omp, root, false));
        let models = tokio::time::timeout(A_TURN_WAITS, asked)
            .await
            .map_err(|_| {
                format!(
                    "omp did not list its models within {} s",
                    A_TURN_WAITS.as_secs()
                )
            })??;
        Ok(named(&models))
    }

    fn listed(&self) -> std::sync::MutexGuard<'_, Option<Vec<Model>>> {
        self.0.listed.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The ask in flight, or a new one where none runs or `afresh`. It runs apart from
    /// whoever waits on it, within [`LISTED_WITHIN`], and a list it gives is kept.
    fn asking(&self, harness: Harness, root: &Path, afresh: bool) -> Asking {
        let mut asking = self.0.asking.lock().unwrap_or_else(|e| e.into_inner());
        if !afresh && let Some(running) = asking.as_ref().filter(|asked| asked.borrow().is_none()) {
            return running.clone();
        }
        let (give, asked) = watch::channel(None);
        let catalog = self.clone();
        let root = root.to_owned();
        tokio::spawn(async move {
            let models = ask(harness, &root).await;
            if let Ok(models) = &models {
                *catalog.listed() = Some(models.clone());
            }
            give.send_replace(Some(models));
        });
        *asking = Some(asked.clone());
        asked
    }
}

/// What `asked` gives once it is done.
async fn answer(mut asked: Asking) -> Result<Vec<Model>, String> {
    let ended = || "the models ask ended without an answer".to_owned();
    let given = Option::clone(&*asked.wait_for(Option::is_some).await.map_err(|_| ended())?);
    given.unwrap_or_else(|| Err(ended()))
}

/// The harness's list as it gives it now, within [`LISTED_WITHIN`].
async fn ask(harness: Harness, root: &Path) -> Result<Vec<Model>, String> {
    let binary = harness
        .binary()
        .ok_or_else(|| format!("{} is not installed here", harness.id()))?;
    Ok(match harness {
        Harness::Omp => {
            let mut models =
                omp_offers(&said(&binary, &["models", "--json"], LISTED_WITHIN).await?)?;
            if let Some(default) = omp_default(root, &models)
                && let Some(at) = models.iter().position(|offer| offer.model == default)
            {
                let offer = models.remove(at);
                models.insert(0, offer);
            }
            models
        }
        Harness::Pi => {
            let (rows, defined) = tokio::join!(
                async {
                    let printed = said(&binary, &["--list-models"], LISTED_WITHIN).await?;
                    Ok::<_, String>(pi_rows(&String::from_utf8_lossy(&printed)))
                },
                pi_catalog(&binary, LISTED_WITHIN),
            );
            pi_joined(rows?, defined?)?
        }
    })
}

/// What the harness prints on standard output for `args`, within `within`; an error
/// where it did not exit well.
async fn said(binary: &Path, args: &[&str], within: Duration) -> Result<Vec<u8>, String> {
    let name = binary
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let output = tokio::time::timeout(
        within,
        helper(binary)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| {
        format!(
            "{name} did not list its models within {} s",
            within.as_secs()
        )
    })?
    .map_err(|e| format!("{name} {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "{name} {} exited with {}",
            args.join(" "),
            output.status
        ));
    }
    Ok(output.stdout)
}

/// `{"models": [{provider, id, selector, name, thinking, cost}]}`: each by its selector,
/// `provider/id`, which is what `--model` takes exactly, at `cost.input` dollars per
/// million input tokens where omp prices it.
fn omp_offers(printed: &[u8]) -> Result<Vec<Model>, String> {
    let listed: Value =
        serde_json::from_slice(printed).map_err(|e| format!("omp models said no JSON: {e}"))?;
    let rows = listed
        .get("models")
        .and_then(Value::as_array)
        .ok_or("omp models has no models array")?;
    rows.iter()
        .map(|row| {
            let selector = row
                .get("selector")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or("omp model has no selector")?;
            let label = row
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty() && *name != selector);
            let thinking = match row.get("thinking") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(levels)) => levels
                    .iter()
                    .map(|level| {
                        level
                            .as_str()
                            .filter(|level| !level.is_empty())
                            .map(str::to_owned)
                            .ok_or("omp model has an invalid thinking level")
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                _ => return Err("omp model has invalid thinking levels".to_owned()),
            };
            Ok(Model {
                provider: row
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                model: selector.to_owned(),
                label: label.map(str::to_owned),
                thinking,
                cost: price(row),
            })
        })
        .collect()
}

/// `cost.input`, US dollars per million input tokens, where it is a price.
fn price(model: &Value) -> Option<f64> {
    model
        .pointer("/cost/input")
        .and_then(Value::as_f64)
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

/// The model omp runs when told none: `modelRoles.default` in its `config.yml`, a
/// selector with its thinking level after a `:`, where that names one it lists.
fn omp_default(root: &Path, models: &[Model]) -> Option<String> {
    let named = crate::harness::omp_setting(root, "modelRoles", "default")?;
    let listed = |selector: &str| models.iter().any(|offer| offer.model == selector);
    if listed(&named) {
        return Some(named);
    }
    let (selector, _level) = named.rsplit_once(':')?;
    listed(selector).then(|| selector.to_owned())
}

/// One row of `pi --list-models`: the model as `provider/model`, and whether it thinks
/// where the table says.
#[derive(Debug, PartialEq)]
struct Row {
    model: String,
    thinks: Option<bool>,
}

/// The table `pi --list-models` prints: a heading whose first two columns are `provider`
/// and `model`, then one row per model, columns split on whitespace. `None` where no
/// heading was printed.
fn pi_rows(printed: &str) -> Option<Vec<Row>> {
    let mut thinking = None;
    let mut headed = false;
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    for line in printed.lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        if columns.len() < 2 {
            continue;
        }
        if columns[0].eq_ignore_ascii_case("provider") && columns[1].eq_ignore_ascii_case("model") {
            headed = true;
            thinking = columns
                .iter()
                .position(|column| column.eq_ignore_ascii_case("thinking"));
            continue;
        }
        let named = |word: &str| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        };
        if !headed || !named(columns[0]) {
            continue;
        }
        let model = format!("{}/{}", columns[0], columns[1]);
        if !seen.insert(model.clone()) {
            continue;
        }
        let thinks = thinking
            .and_then(|at| columns.get(at))
            .and_then(|said| match *said {
                "yes" => Some(true),
                "no" => Some(false),
                _ => None,
            });
        rows.push(Row { model, thinks });
    }
    headed.then_some(rows)
}

/// pi's catalog over its RPC mode: `get_available_models`, asked of a pi that loads its
/// extensions (providers may come from them) and nothing else, in a private folder.
async fn pi_catalog(binary: &Path, within: Duration) -> Result<Vec<Value>, String> {
    let folder = Private::new()?;
    let mut child = helper(binary)
        .args([
            "--mode",
            "rpc",
            "--no-session",
            "--no-tools",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
            "--approve",
        ])
        .current_dir(&folder.0)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("pi --mode rpc: {e}"))?;
    let asked = async {
        let mut stdin = child.stdin.take().ok_or("pi took no input")?;
        stdin
            .write_all(b"{\"id\":\"steery-models\",\"type\":\"get_available_models\"}\n")
            .await
            .map_err(|e| format!("pi --mode rpc: {e}"))?;
        let stdout = child.stdout.take().ok_or("pi printed nothing")?;
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| format!("pi --mode rpc: {e}"))?
        {
            let Ok(answer) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if answer.get("type").and_then(Value::as_str) != Some("response")
                || answer.get("id").and_then(Value::as_str) != Some("steery-models")
            {
                continue;
            }
            if answer.get("success").and_then(Value::as_bool) != Some(true) {
                return Err(format!(
                    "pi refused get_available_models: {}",
                    answer
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("no reason given")
                ));
            }
            return answer
                .pointer("/data/models")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| "pi's get_available_models named no models".to_owned());
        }
        Err("pi ended without listing its models".to_owned())
    };
    let answer = tokio::time::timeout(within, asked)
        .await
        .map_err(|_| format!("pi did not list its models within {} s", within.as_secs()))?;
    let _ = child.kill().await;
    answer
}

/// The thinking levels pi offers for a model of its catalog: `off` alone where it does not
/// reason; otherwise every level its `thinkingLevelMap` does not turn off, `xhigh` and
/// `max` only where the map names them.
fn pi_levels(model: &Value) -> Vec<String> {
    if model.get("reasoning").and_then(Value::as_bool) != Some(true) {
        return vec!["off".into()];
    }
    let map = model.get("thinkingLevelMap");
    PI_THINKING
        .iter()
        .filter(|level| match map.and_then(|map| map.get(**level)) {
            Some(Value::Null) => false,
            Some(_) => true,
            None => !matches!(**level, "xhigh" | "max"),
        })
        .map(|level| (*level).to_owned())
        .collect()
}

/// pi's list: the table's models in its order, each with the catalog's levels and price,
/// else `off` alone where the table says it does not think, else none (it runs at pi's
/// own); then the catalog's models the table left out.
fn pi_joined(rows: Option<Vec<Row>>, defined: Vec<Value>) -> Result<Vec<Model>, String> {
    let mut catalog = Vec::new();
    for model in &defined {
        let (Some(provider), Some(id)) = (
            model.get("provider").and_then(Value::as_str),
            model.get("id").and_then(Value::as_str),
        ) else {
            return Err("pi's catalog has a model with no provider or id".into());
        };
        catalog.push((format!("{provider}/{id}"), pi_levels(model), price(model)));
    }
    let Some(rows) = rows else {
        if catalog.is_empty() {
            return Ok(Vec::new());
        }
        return Err(format!(
            "pi --list-models printed no table, while pi's RPC names {} models",
            catalog.len()
        ));
    };
    let offer = |model: String, thinking: Vec<String>, cost: Option<f64>| Model {
        provider: model
            .split_once('/')
            .map(|(provider, _)| provider.to_owned()),
        model,
        label: None,
        thinking,
        cost,
    };
    let mut models: Vec<Model> = rows
        .into_iter()
        .map(
            |row| match catalog.iter().find(|(model, _, _)| *model == row.model) {
                Some((_, levels, cost)) => offer(row.model, levels.clone(), *cost),
                None if row.thinks == Some(false) => offer(row.model, vec!["off".into()], None),
                None => offer(row.model, Vec::new(), None),
            },
        )
        .collect();
    for (model, levels, cost) in catalog {
        if !models.iter().any(|offer| offer.model == model) {
            models.push(offer(model, levels, cost));
        }
    }
    Ok(models)
}

/// A private folder of this driver's, removed when dropped, holding pi's settings for a
/// run that must reach nothing of the person's.
pub struct Private(pub PathBuf);

impl Private {
    pub fn new() -> Result<Self, String> {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let folder = std::env::temp_dir().join(format!(
            "steery-pi-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&folder)
            .and_then(|()| std::fs::create_dir(folder.join(".pi")))
            .and_then(|()| std::fs::write(folder.join(".pi/settings.json"), PI_SETTINGS))
            .map_err(|e| format!("{}: {e}", folder.display()))?;
        Ok(Self(folder))
    }
}

impl Drop for Private {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The files whose change changes what the harness lists. omp: its configuration and
/// model files and the `.env` files it takes provider keys from, in its agent folder, its
/// config folder and home; its stored sign-ins live in `agent.db`, which every
/// conversation writes. pi: its models, sign-ins and settings, its release, and its
/// extensions, which may bring providers.
fn watched(harness: Harness, home: &Path, root: &Path) -> Vec<PathBuf> {
    match harness {
        Harness::Omp => {
            let mut files: Vec<PathBuf> = [
                "config.yml",
                "config.yaml",
                "models.yml",
                "models.yaml",
                "models.json",
                ".env",
            ]
            .iter()
            .map(|name| root.join(name))
            .collect();
            if let Ok(base) = harness.base(home) {
                files.push(base.join(".env"));
            }
            files.push(home.join(".env"));
            files
        }
        Harness::Pi => [
            "models.json",
            "auth.json",
            "settings.json",
            "install/current-version",
            "extensions",
        ]
        .iter()
        .map(|name| root.join(name))
        .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// pi's table is read from its heading on, a row a provider could not be is no model,
    /// and the catalog's levels and price come with each; a model the table left out
    /// follows it.
    #[test]
    fn pi_lists_its_table_with_its_catalogs_levels() {
        let printed = "Some banner\n\
                       provider   model            context  thinking\n\
                       anthropic  claude-opus-5    200K     yes\n\
                       openai     gpt-6-mini       400K     no\n\
                       local      tiny             8K       ?\n\
                       anthropic  claude-opus-5    200K     yes\n";
        let rows = pi_rows(printed).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].thinks, Some(false));
        let defined = vec![
            json!({"provider": "anthropic", "id": "claude-opus-5", "reasoning": true,
                   "thinkingLevelMap": {"minimal": null, "xhigh": "max"}, "cost": {"input": 5.0}}),
            json!({"provider": "google", "id": "gemini-4", "reasoning": false}),
        ];
        let models = pi_joined(Some(rows), defined).unwrap();
        let names: Vec<&str> = models.iter().map(|m| m.model.as_str()).collect();
        assert_eq!(
            names,
            [
                "anthropic/claude-opus-5",
                "openai/gpt-6-mini",
                "local/tiny",
                "google/gemini-4"
            ]
        );
        assert_eq!(
            models[0].thinking,
            ["off", "low", "medium", "high", "xhigh"]
        );
        assert_eq!(models[0].cost, Some(5.0));
        assert_eq!(models[1].thinking, ["off"]);
        assert!(models[2].thinking.is_empty());
        assert_eq!(models[3].thinking, ["off"]);
        assert_eq!(models[3].provider.as_deref(), Some("google"));
    }

    /// No table while the catalog names models is an error, and the last list stands;
    /// neither naming any is an empty list.
    #[test]
    fn pi_without_a_table_lists_nothing_or_refuses() {
        assert!(pi_rows("no models\n").is_none());
        assert!(pi_joined(None, Vec::new()).unwrap().is_empty());
        assert!(pi_joined(None, vec![json!({"provider": "a", "id": "b"})]).is_err());
    }
}
