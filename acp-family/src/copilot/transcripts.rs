//! PLACEHOLDER (slice A replaces this file whole).

use std::path::{Path, PathBuf};

use steery_sdk::Error;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read};

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list(&self, _list: List) -> Listed {
        unimplemented!()
    }

    pub fn locate(&self, _locate: Locate) -> Result<Located, Error> {
        Err(Error::Unknown)
    }

    pub fn read(&self, _read: Read) -> Result<Events, Error> {
        Err(Error::Unknown)
    }
}
