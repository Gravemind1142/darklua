use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Interns file paths to compact `source_id`s and supports reverse lookup.
#[derive(Debug, Default, Clone)]
pub struct SourceRegistry {
    path_to_id: HashMap<PathBuf, u32>,
    id_to_path: Vec<PathBuf>,
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the id for a path, inserting it if missing.
    pub fn intern(&mut self, path: impl AsRef<Path>) -> u32 {
        let path = path.as_ref();
        if let Some(id) = self.path_to_id.get(path) {
            return *id;
        }
        let len = self.id_to_path.len();
        let id: u32 = if len <= u32::MAX as usize {
            len as u32
        } else {
            panic!("too many sources to index")
        };
        self.path_to_id.insert(path.to_path_buf(), id);
        self.id_to_path.push(path.to_path_buf());
        id
    }

    pub fn get_path(&self, id: u32) -> Option<&Path> {
        self.id_to_path
            .get(id as usize)
            .map(|p| p.as_path())
    }

    pub fn get_id(&self, path: impl AsRef<Path>) -> Option<u32> {
        self.path_to_id.get(path.as_ref()).copied()
    }

    pub fn len(&self) -> usize { self.id_to_path.len() }
    pub fn is_empty(&self) -> bool { self.id_to_path.is_empty() }
}


