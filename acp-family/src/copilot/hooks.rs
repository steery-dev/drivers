//! PLACEHOLDER (slice B replaces this file whole).

use steery_sdk::Error;
use steery_sdk::wire::harness::{Hook, HookAnswer, HookAnswered, HookMoment, HookRead};

use super::transcripts::Store;

pub fn table() -> Vec<Hook> {
    Vec::new()
}

pub fn read(_store: &Store, _read: HookRead) -> Result<HookMoment, Error> {
    Err(Error::Unknown)
}

pub fn answer(_answer: HookAnswer) -> Result<HookAnswered, Error> {
    Err(Error::Unknown)
}
