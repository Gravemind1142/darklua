use serde::{Deserialize, Serialize};

use crate::{
    frontend::DarkluaResult,
    nodes::{Arguments, Expression, FunctionCall, Prefix, Statement},
    rules::{convert_require::rojo_sourcemap::RojoSourcemap, Context},
    utils, DarkluaError,
};

use std::path::{Component, Path, PathBuf};

use super::{
    instance_path::{get_parent_instance, script_identifier, InstancePath},
    RequireMode, RobloxIndexStyle,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RobloxRequireMode {
    rojo_sourcemap: Option<PathBuf>,
    #[serde(default, deserialize_with = "crate::utils::string_or_struct")]
    indexing_style: RobloxIndexStyle,
    #[serde(skip)]
    cached_sourcemap: Option<RojoSourcemap>,
}

impl RobloxRequireMode {
    pub(crate) fn initialize(&mut self, context: &Context) -> DarkluaResult<()> {
        if let Some(ref rojo_sourcemap_path) = self
            .rojo_sourcemap
            .as_ref()
            .map(|rojo_sourcemap_path| context.project_location().join(rojo_sourcemap_path))
        {
            context.add_file_dependency(rojo_sourcemap_path.clone());

            let sourcemap_parent_location = get_relative_parent_path(rojo_sourcemap_path);
            let sourcemap = RojoSourcemap::parse(
                &context
                    .resources()
                    .get(rojo_sourcemap_path)
                    .map_err(|err| {
                        DarkluaError::from(err).context("while initializing Roblox require mode")
                    })?,
                sourcemap_parent_location,
            )
            .map_err(|err| {
                err.context(format!(
                    "unable to parse Rojo sourcemap at `{}`",
                    rojo_sourcemap_path.display()
                ))
            })?;
            self.cached_sourcemap = Some(sourcemap);
        }
        Ok(())
    }

    pub(crate) fn find_require(
        &self,
        call: &FunctionCall,
        context: &Context,
        current_block: &crate::nodes::Block,
    ) -> DarkluaResult<Option<PathBuf>> {
        let instance_path = match call.get_arguments() {
            Arguments::Tuple(tuple) if tuple.len() == 1 => {
                let expr = tuple.iter_values().next().unwrap();
                self.parse_expression_to_instance_path(expr, context, current_block)
            }
            _ => None,
        };

        let Some(instance_path) = instance_path else { return Ok(None) };

        if let Some(sourcemap) = &self.cached_sourcemap {
            let source_path = utils::normalize_path(context.current_path());
            if let Some(target_file) = sourcemap.get_file_from_instance_path(&source_path, &instance_path) {
                return Ok(Some(target_file));
            }
            log::debug!("unable to resolve Roblox instance path to file using sourcemap");
            Ok(None)
        } else {
            Err(DarkluaError::custom(
                "Roblox require conversion requires a Rojo sourcemap (missing `rojo_sourcemap`)",
            ))
        }
    }

    fn parse_expression_to_instance_path(&self, expression: &Expression, context: &Context, current_block: &crate::nodes::Block) -> Option<InstancePath> {
        match expression {
            Expression::Identifier(id) => match id.get_name().as_str() {
                "script" => Some(InstancePath::from_script()),
                "game" => Some(InstancePath::from_root()),
                other => {
                    self.resolve_identifier_to_instance_path(other, context, current_block)
                }
            },
            Expression::Field(field) => {
                let mut base = self.parse_prefix_to_instance_path(field.get_prefix(), context, current_block)?;
                let name = field.get_field().get_name();
                if name == "Parent" {
                    base.parent();
                } else {
                    base.child(name);
                }
                Some(base)
            }
            Expression::Index(index) => {
                let mut base = self.parse_prefix_to_instance_path(index.get_prefix(), context, current_block)?;
                let child_name = match index.get_index() {
                    Expression::String(s) => s.get_string_value()?.to_string(),
                    _ => return None,
                };
                base.child(child_name);
                Some(base)
            }
            Expression::Call(call) => self.parse_call_to_instance_path(call, context, current_block),
            Expression::Parenthese(paren) => self.parse_expression_to_instance_path(paren.inner_expression(), context, current_block),
            _ => None,
        }
    }

    fn parse_prefix_to_instance_path(&self, prefix: &Prefix, context: &Context, current_block: &crate::nodes::Block) -> Option<InstancePath> {
        match prefix {
            Prefix::Identifier(id) => match id.get_name().as_str() {
                "script" => Some(InstancePath::from_script()),
                "game" => Some(InstancePath::from_root()),
                other => self.resolve_identifier_to_instance_path(other, context, current_block),
            },
            Prefix::Field(field) => {
                let mut base = self.parse_prefix_to_instance_path(field.get_prefix(), context, current_block)?;
                let name = field.get_field().get_name();
                if name == "Parent" {
                    base.parent();
                } else {
                    base.child(name);
                }
                Some(base)
            }
            Prefix::Index(index) => {
                let mut base = self.parse_prefix_to_instance_path(index.get_prefix(), context, current_block)?;
                let child_name = match index.get_index() {
                    Expression::String(s) => s.get_string_value()?.to_string(),
                    _ => return None,
                };
                base.child(child_name);
                Some(base)
            }
            Prefix::Call(call) => self.parse_call_to_instance_path(call, context, current_block),
            Prefix::Parenthese(paren) => self.parse_expression_to_instance_path(paren.inner_expression(), context, current_block),
        }
    }

    fn parse_call_to_instance_path(&self, call: &FunctionCall, context: &Context, current_block: &crate::nodes::Block) -> Option<InstancePath> {
        let method = call.get_method().map(|m| m.get_name().as_str());
        let mut base = self.parse_prefix_to_instance_path(call.get_prefix(), context, current_block)?;
        match method {
            Some("GetService") => {
                let child = self.read_first_string_argument(call)?;
                base.child(child);
                Some(base)
            }
            Some("WaitForChild") | Some("FindFirstChild") => {
                let child = self.read_first_string_argument(call)?;
                base.child(child);
                Some(base)
            }
            _ => None,
        }
    }

    fn read_first_string_argument(&self, call: &FunctionCall) -> Option<String> {
        match call.get_arguments() {
            Arguments::String(s) => s.get_string_value().map(|s| s.to_string()),
            Arguments::Tuple(tuple) if tuple.len() >= 1 => match tuple.iter_values().next().unwrap() {
                Expression::String(s) => s.get_string_value().map(|s| s.to_string()),
                _ => None,
            },
            _ => None,
        }
    }

    fn resolve_identifier_to_instance_path(&self, name: &str, _context: &Context, current_block: &crate::nodes::Block) -> Option<InstancePath> {
        // find a local assignment defining this identifier and parse its value
        for statement in current_block.iter_statements() {
            if let Statement::LocalAssign(local) = statement {
                for (var, value) in local.iter_variables().zip(local.iter_values()) {
                    if var.get_identifier().get_name() == name {
                        if let Some(path) = self.parse_expression_to_instance_path(value, _context, current_block) {
                            return Some(path);
                        }
                    }
                }
            }
        }
        None
    }

    pub(crate) fn generate_require(
        &self,
        require_path: &Path,
        current: &RequireMode,
        context: &Context,
    ) -> DarkluaResult<Option<Arguments>> {
        let source_path = utils::normalize_path(context.current_path());
        log::trace!(
            "generate Roblox require for `{}` from `{}`",
            require_path.display(),
            source_path.display(),
        );

        if let Some((sourcemap, sourcemap_path)) = self
            .cached_sourcemap
            .as_ref()
            .zip(self.rojo_sourcemap.as_ref())
        {
            if let Some(require_relative_to_sourcemap) = get_relative_path(
                require_path,
                get_relative_parent_path(sourcemap_path),
                false,
            )? {
                log::trace!(
                    "  ⨽ use sourcemap at `{}` to find `{}`",
                    sourcemap_path.display(),
                    require_relative_to_sourcemap.display()
                );

                if let Some(instance_path) =
                    sourcemap.get_instance_path(&source_path, &require_relative_to_sourcemap)
                {
                    Ok(Some(Arguments::default().with_argument(
                        instance_path.convert(&self.indexing_style),
                    )))
                } else {
                    log::warn!(
                        "unable to find path `{}` in sourcemap (from `{}`)",
                        require_relative_to_sourcemap.display(),
                        source_path.display()
                    );
                    Ok(None)
                }
            } else {
                log::debug!(
                    "unable to get relative path from sourcemap for `{}`",
                    require_path.display()
                );
                Ok(None)
            }
        } else if let Some(relative_require_path) =
            get_relative_path(require_path, &source_path, true)?
        {
            log::trace!(
                "make require path relative to source: `{}`",
                relative_require_path.display()
            );

            let require_is_module_folder_name = match current {
                RequireMode::Path(path_mode) => path_mode.is_module_folder_name(&relative_require_path),
                RequireMode::Roblox(_roblox_mode) => {
                    // in Roblox mode, module folder is always `init`
                    matches!(relative_require_path.file_stem().and_then(std::ffi::OsStr::to_str), Some("init"))
                }
            };
            // if we are about to make a require to a path like `./x/y/z/init.lua`
            // we can pop the last component from the path
            let take_components = relative_require_path
                .components()
                .count()
                .saturating_sub(if require_is_module_folder_name { 1 } else { 0 });
            let mut path_components = relative_require_path.components().take(take_components);

            if let Some(first_component) = path_components.next() {
                let source_is_module_folder_name = match current {
                    RequireMode::Path(path_mode) => path_mode.is_module_folder_name(&source_path),
                    RequireMode::Roblox(_roblox_mode) => {
                        matches!(source_path.file_stem().and_then(std::ffi::OsStr::to_str), Some("init"))
                    }
                };

                let instance_path = path_components.try_fold(
                    match first_component {
                        Component::CurDir => {
                            if source_is_module_folder_name {
                                script_identifier().into()
                            } else {
                                get_parent_instance(script_identifier())
                            }
                        }
                        Component::ParentDir => {
                            if source_is_module_folder_name {
                                get_parent_instance(script_identifier())
                            } else {
                                get_parent_instance(get_parent_instance(script_identifier()))
                            }
                        }
                        Component::Normal(_) => {
                            return Err(DarkluaError::custom(format!(
                                concat!(
                                    "unable to convert path `{}`: the require path should be ",
                                    "relative and start with `.` or `..` (got `{}`)"
                                ),
                                require_path.display(),
                                relative_require_path.display(),
                            )))
                        }
                        Component::Prefix(_) | Component::RootDir => {
                            return Err(DarkluaError::custom(format!(
                                concat!(
                                    "unable to convert absolute path `{}`: ",
                                    "without a provided Rojo sourcemap, ",
                                    "darklua can only convert relative paths ",
                                    "(starting with `.` or `..`)"
                                ),
                                require_path.display(),
                            )))
                        }
                    },
                    |instance: Prefix, component| match component {
                        Component::CurDir => Ok(instance),
                        Component::ParentDir => Ok(get_parent_instance(instance)),
                        Component::Normal(name) => utils::convert_os_string(name)
                            .map(|child_name| self.indexing_style.index(instance, child_name)),
                        Component::Prefix(_) | Component::RootDir => {
                            Err(DarkluaError::custom(format!(
                                "unable to convert path `{}`: unexpected component in relative path `{}`",
                                require_path.display(),
                                relative_require_path.display(),
                            )))
                        },
                    },
                )?;

                Ok(Some(Arguments::default().with_argument(instance_path)))
            } else {
                Err(DarkluaError::custom(format!(
                    "unable to convert path `{}` from `{}` without a sourcemap: the relative path is empty `{}`",
                    require_path.display(),
                    source_path.display(),
                    relative_require_path.display(),
                )))
            }
        } else {
            Err(DarkluaError::custom(format!(
                concat!(
                    "unable to convert path `{}` from `{}` without a sourcemap: unable to ",
                    "make the require path relative to the source file"
                ),
                require_path.display(),
                source_path.display(),
            )))
        }
    }
}

fn get_relative_path(
    require_path: &Path,
    source_path: &Path,
    use_current_dir_prefix: bool,
) -> Result<Option<PathBuf>, DarkluaError> {
    Ok(
        pathdiff::diff_paths(require_path, get_relative_parent_path(source_path))
            .map(|path| {
                if use_current_dir_prefix && !path.starts_with(".") && !path.starts_with("..") {
                    Path::new(".").join(path)
                } else if !use_current_dir_prefix && path.starts_with(".") {
                    path.strip_prefix(".")
                        .map(Path::to_path_buf)
                        .ok()
                        .unwrap_or(path)
                } else {
                    path
                }
            })
            .map(utils::normalize_path_with_current_dir),
    )
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
