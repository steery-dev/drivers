//! An Anthropic API key as a way to give a harness its sign-in (PROTOCOL.md §4.1, Needs):
//! the runtime keeps it and sets it as `ANTHROPIC_API_KEY` for the harness on every host.
//! Claude Code, omp and pi each read that variable.

use std::collections::BTreeMap;

use steery_sdk::wire::harness::{Field, Given, NeedLink, Way};
use steery_sdk::{Error, refusal};

/// The way's id in every plan.
pub const WAY: &str = "api-key";
/// The variable the key is given as.
pub const VARIABLE: &str = "ANTHROPIC_API_KEY";
/// The field the key is typed in.
const KEY: &str = "key";
/// How a Claude API key starts, as https://platform.claude.com/docs/en/manage-claude/authentication
/// says it: "Static `sk-ant-api...` secret". An Admin key (`sk-ant-admin01-`) or an OAuth
/// token (`sk-ant-oat01-`) is none.
const STARTS: &str = "sk-ant-api";
/// Where a key is made, as the same page names it.
const KEYS_PAGE: &str = "https://platform.claude.com/settings/keys";

/// The way, `about` its cost and what the harness does with it.
pub fn way(about: String) -> Way {
    Way {
        id: WAY.into(),
        label: "Use your Anthropic API key".into(),
        about: Some(about),
        link: Some(NeedLink {
            label: "Create a key".into(),
            url: KEYS_PAGE.into(),
            fills: None,
        }),
        fields: vec![Field {
            name: KEY.into(),
            label: "API key".into(),
            about: Some(format!("Starts with {STARTS}")),
            secret: true,
        }],
        ends_at: None,
    }
}

/// What `values` give by [`way`]: the key as [`VARIABLE`], where it reads as an Anthropic
/// API key. `line` says what the harness now does.
pub fn give(values: &BTreeMap<String, String>, line: &str) -> Result<Given, Error> {
    let key = values.get(KEY).map_or("", |key| key.trim());
    if key.is_empty() {
        return Err(Error::refused(refusal::REFUSED, "Paste your Anthropic API key"));
    }
    if !key.starts_with(STARTS) {
        return Err(Error::refused(
            refusal::REFUSED,
            format!("An Anthropic API key starts with {STARTS}"),
        ));
    }
    Ok(Given::Done {
        line: line.into(),
        env: BTreeMap::from([(VARIABLE.to_owned(), key.to_owned())]),
    })
}
