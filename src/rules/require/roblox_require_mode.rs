use serde::{Deserialize, Serialize};

use crate::frontend::DarkluaResult;
use crate::nodes::FunctionCall;
use crate::rules::Context;
use crate::DarkluaError;

use std::path::{Path, PathBuf};

// Reuse the Rojo sourcemap and instance path data structures from convert_require
use crate::rules::convert_require::{RojoSourcemap, InstancePath};

/// A require mode for handling Roblox-specific require patterns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RobloxRequireMode {
    #[serde(default)]
    rojo_sourcemap: Option<PathBuf>,
    #[serde(skip)]
    cached_sourcemap: Option<RojoSourcemap>,
}

impl Default for RobloxRequireMode {
    fn default() -> Self {
        Self {
            rojo_sourcemap: None,
            cached_sourcemap: None,
        }
    }
}

impl RobloxRequireMode {
    /// Creates a new Roblox require mode.
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn initialize(&mut self, context: &Context) -> Result<(), DarkluaError> {
        if let Some(ref rojo_sourcemap_path) = self
            .rojo_sourcemap
            .as_ref()
            .map(|p| context.project_location().join(p))
        {
            // track sourcemap as a dependency and cache parsed content
            context.add_file_dependency(rojo_sourcemap_path.clone());
            let parent = get_relative_parent_path(rojo_sourcemap_path);
            let content = context
                .resources()
                .get(rojo_sourcemap_path)
                .map_err(|err| DarkluaError::from(err).context("while initializing Roblox require mode"))?;
            let sourcemap = RojoSourcemap::parse(&content, parent).map_err(|err| {
                err.context(format!(
                    "unable to parse Rojo sourcemap at `{}`",
                    rojo_sourcemap_path.display()
                ))
            })?;
            self.cached_sourcemap = Some(sourcemap);
        } else {
            self.cached_sourcemap = None;
        }
        Ok(())
    }

    pub(crate) fn find_require(
        &self,
        _call: &FunctionCall,
        _context: &Context,
    ) -> DarkluaResult<Option<PathBuf>> {
        // Resolution is handled in bundling using the current AST block to compute instance paths.
        // This method is intentionally a stub here for bundling integration.
        Ok(None)
    }

    pub(crate) fn get_file_from_instance_path(
        &self,
        from_file: &Path,
        instance_path: &InstancePath,
    ) -> Option<PathBuf> {
        self.cached_sourcemap
            .as_ref()
            .and_then(|map| map.get_file_from_instance_path(from_file, instance_path))
    }

    pub(crate) fn get_instance_path_for_file(
        &self,
        from_file: &Path,
        target_file: &Path,
    ) -> Option<InstancePath> {
        self.cached_sourcemap
            .as_ref()
            .and_then(|map| map.get_instance_path(from_file, target_file))
    }

    pub(crate) fn generate_require(
        &self,
        _path: &Path,
        _current_mode: &crate::rules::RequireMode,
        _context: &Context<'_, '_, '_>,
    ) -> Result<Option<crate::nodes::Arguments>, crate::DarkluaError> {
        Err(DarkluaError::custom("unsupported target require mode")
            .context("roblox require mode cannot be used"))
    }
}

fn get_relative_parent_path(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) => {
            if parent == Path::new("") {
                Path::new(".")
            } else {
                parent
            }
        }
        None => Path::new(".."),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rules::ContextBuilder;
    use crate::Resources;

    const ROJO_SOURCEMAP: &str = r#"{
        "name": "Project",
        "className": "ModuleScript",
        "filePaths": ["src/init.lua", "default.project.json"],
        "children": [
            {
                "name": "value",
                "className": "ModuleScript",
                "filePaths": ["src/value.lua"]
            }
        ]
    }"#;

    fn setup_resources() -> Resources {
        let resources = Resources::from_memory();
        resources.write("src/init.lua", "return nil").unwrap();
        resources.write("src/value.lua", "return true").unwrap();
        resources
            .write("default.project.json", ROJO_SOURCEMAP)
            .unwrap();
        resources
    }

    fn new_context(resources: &Resources) -> Context {
        ContextBuilder::new("src/init.lua", resources, "")
            .with_project_location(".")
            .build()
    }

    #[test]
    fn initialize_parses_rojo_sourcemap_and_tracks_dependency() {
        let resources = setup_resources();
        let context = new_context(&resources);

        let mut mode = RobloxRequireMode {
            rojo_sourcemap: Some(PathBuf::from("default.project.json")),
            cached_sourcemap: None,
        };

        mode.initialize(&context).expect("initialize failed");

        // dependency should be tracked
        let deps: Vec<_> = context.clone().into_dependencies().collect();
        assert!(
            deps.iter()
                .any(|p| p.ends_with("default.project.json")),
            "expected rojo sourcemap to be tracked as dependency, got: {:?}",
            deps
        );

        assert!(mode.cached_sourcemap.is_some(), "sourcemap should be cached");
    }

    #[test]
    fn resolve_file_from_instance_path() {
        let resources = setup_resources();
        let context = new_context(&resources);

        let mut mode = RobloxRequireMode {
            rojo_sourcemap: Some(PathBuf::from("default.project.json")),
            cached_sourcemap: None,
        };
        mode.initialize(&context).expect("initialize failed");

        let mut path = InstancePath::from_script();
        path.child("value");

        let resolved = mode
            .get_file_from_instance_path(Path::new("src/init.lua"), &path)
            .expect("failed to resolve instance path to file");

        assert!(resolved.ends_with("src/value.lua"), "got: {resolved:?}");
    }

    #[test]
    fn resolve_instance_path_from_file() {
        let resources = setup_resources();
        let context = new_context(&resources);

        let mut mode = RobloxRequireMode {
            rojo_sourcemap: Some(PathBuf::from("default.project.json")),
            cached_sourcemap: None,
        };
        mode.initialize(&context).expect("initialize failed");

        let instance_path = mode
            .get_instance_path_for_file(Path::new("src/init.lua"), Path::new("src/value.lua"))
            .expect("expected an instance path for file");

        // Basic sanity: path should start from script and have one child component
        match instance_path.root() {
            crate::rules::convert_require::InstancePathRoot::Script => {}
            other => panic!("unexpected root: {other:?}"),
        }
        assert_eq!(instance_path.components().len(), 1);
    }
} 