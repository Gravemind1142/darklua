mod module_definitions;

use module_definitions::BuildModuleDefinitions;

use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::{iter, mem};

use crate::frontend::DarkluaResult;
use crate::nodes::{
    Arguments, Block, DoStatement, Expression, FieldExpression, FunctionCall, LocalAssignStatement,
    Prefix, Statement, StringExpression,
};
use crate::process::{
    to_expression, DefaultVisitor, IdentifierTracker, NodeProcessor, NodeVisitor, ScopeVisitor,
};
use crate::rules::require::is_require_call;
use crate::rules::{
    Context, ContextBuilder, FlawlessRule, ReplaceReferencedTokens, RuleProcessResult,
};
use crate::utils::Timer;
use crate::{DarkluaError, Resources};

use super::BundleOptions;
use crate::rules::convert_require::{
    datamodel_identifier, get_parent_instance, InstancePath, InstancePathComponent,
    InstancePathRoot,
};
use crate::rules::require::RobloxRequireMode;

pub(crate) enum RequiredResource {
    Block(Block),
    Expression(Expression),
}

#[derive(Debug)]
struct RequireRobloxProcessor<'a, 'b, 'resources> {
    options: &'a BundleOptions,
    identifier_tracker: IdentifierTracker,
    roblox_require_mode: &'b RobloxRequireMode,
    module_definitions: BuildModuleDefinitions,
    source: PathBuf,
    module_cache: HashMap<PathBuf, Expression>,
    require_stack: Vec<PathBuf>,
    skip_module_paths: HashSet<PathBuf>,
    resources: &'resources Resources,
    errors: Vec<String>,
    current_block_clone: Block,
    root_block_clone: Block,
}

impl<'a, 'b, 'resources> RequireRobloxProcessor<'a, 'b, 'resources> {
    fn new<'context>(
        context: &'context Context<'b, 'resources, '_>,
        options: &'a BundleOptions,
        roblox_require_mode: &'b RobloxRequireMode,
        current_block_clone: Block,
    ) -> Self
    where
        'context: 'b,
        'context: 'resources,
    {
        Self {
            options,
            identifier_tracker: IdentifierTracker::new(),
            roblox_require_mode,
            module_definitions: BuildModuleDefinitions::new(options.modules_identifier()),
            source: context.current_path().to_path_buf(),
            module_cache: Default::default(),
            require_stack: Default::default(),
            skip_module_paths: Default::default(),
            resources: context.resources(),
            errors: Vec::new(),
            current_block_clone: current_block_clone.clone(),
            root_block_clone: current_block_clone,
        }
    }

    fn apply(self, block: &mut Block, context: &Context) -> RuleProcessResult {
        self.module_definitions.apply(block, context);
        match self.errors.len() {
            0 => Ok(()),
            1 => Err(self.errors.first().unwrap().to_string()),
            _ => Err(format!("- {}", self.errors.join("\n- "))),
        }
    }

    fn parse_expression_to_instance_path(&self, expression: &Expression) -> Option<InstancePath> {
        match expression {
            Expression::Identifier(id) => match id.get_name().as_str() {
                "script" => {
                    Some(InstancePath::from_script())
                }
                "game" => {
                    Some(InstancePath::from_root())
                }
                other => {
                    let resolved = self.resolve_identifier_to_instance_path(other);
                    if resolved.is_none() {
                        log::warn!(
                            "resolve_identifier_to_instance_path failed for Identifier({})",
                            other
                        );
                    }
                    resolved
                }
            },
            Expression::Field(field) => {
                let mut base = self.parse_prefix_to_instance_path(field.get_prefix())?;
                let name = field.get_field().get_name();
                if name == "Parent" {
                    base.parent();
                } else {
                    base.child(name);
                }
                Some(base)
            }
            Expression::Index(index) => {
                let mut base = self.parse_prefix_to_instance_path(index.get_prefix())?;
                let child_name = match index.get_index() {
                    Expression::String(s) => s.get_string_value()?.to_string(),
                    _ => {
                        log::warn!(
                            "parse_expression_to_instance_path: Index with non-string index: cannot handle this expression"
                        );
                        log::warn!("index expression kind not supported for instance path");
                        return None;
                    }
                };
                base.child(child_name);
                Some(base)
            }
            Expression::Call(call) => {
                self.parse_call_to_instance_path(call)
            }
            Expression::Parenthese(paren) => {
                self.parse_expression_to_instance_path(paren.inner_expression())
            }
            _ => {
                log::warn!(
                    "parse_expression_to_instance_path: unsupported expression kind for instance path resolution"
                );
                None
            }
        }
    }

    fn parse_prefix_to_instance_path(&self, prefix: &Prefix) -> Option<InstancePath> {
        match prefix {
            Prefix::Identifier(id) => match id.get_name().as_str() {
                "script" => {
                    Some(InstancePath::from_script())
                }
                "game" => {
                    Some(InstancePath::from_root())
                }
                other => {
                    let resolved = self.resolve_identifier_to_instance_path(other);
                    if resolved.is_none() {
                        log::warn!(
                            "resolve_identifier_to_instance_path failed for Identifier({})",
                            other
                        );
                    }
                    resolved
                }
            },
            Prefix::Field(field) => {
                let mut base = self.parse_prefix_to_instance_path(field.get_prefix())?;
                let name = field.get_field().get_name();
                if name == "Parent" {
                    base.parent();
                } else {
                    base.child(name);
                }
                Some(base)
            }
            Prefix::Index(index) => {
                let mut base = self.parse_prefix_to_instance_path(index.get_prefix())?;
                let child_name = match index.get_index() {
                    Expression::String(s) => s.get_string_value()?.to_string(),
                    _ => {
                        log::warn!(
                            "parse_prefix_to_instance_path: Index with non-string index: cannot handle this prefix"
                        );
                        return None;
                    }
                };
                base.child(child_name);
                Some(base)
            }
            Prefix::Call(call) => {
                self.parse_call_to_instance_path(call)
            }
            Prefix::Parenthese(paren) => {
                self.parse_expression_to_instance_path(paren.inner_expression())
            }
        }
    }

    fn parse_call_to_instance_path(&self, call: &FunctionCall) -> Option<InstancePath> {
        let method = call.get_method().map(|m| m.get_name().as_str());
        let mut base = self.parse_prefix_to_instance_path(call.get_prefix())?;
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
            Some("FindFirstAncestor") => {
                let ancestor = self.read_first_string_argument(call)?;
                base.ancestor(ancestor);
                Some(base)
            }
            _ => {
                None
            }
        }
    }

    fn read_first_string_argument(&self, call: &FunctionCall) -> Option<String> {
        match call.get_arguments() {
            Arguments::String(s) => {
                let value = s.get_string_value().map(|s| s.to_string());
                if value.is_none() {
                    log::warn!(
                        "read_first_string_argument: got String argument but could not get value"
                    );
                }
                value
            }
            Arguments::Tuple(tuple) if tuple.len() >= 1 => {
                let first = tuple.iter_values().next().unwrap();
                match first {
                    Expression::String(s) => {
                        let value = s.get_string_value().map(|s| s.to_string());
                        if value.is_none() {
                            log::warn!(
                                "read_first_string_argument: first tuple arg is String but could not get value"
                            );
                        }
                        value
                    }
                    _ => {
                        log::warn!(
                            "read_first_string_argument: first tuple arg is not a String expression"
                        );
                        None
                    }
                }
            }
            _ => {
                log::warn!("read_first_string_argument: unsupported arguments kind");
                None
            }
        }
    }

    fn resolve_identifier_to_instance_path(&self, name: &str) -> Option<InstancePath> {
        fn resolve_in_block(
            this: &RequireRobloxProcessor<'_, '_, '_>,
            name: &str,
            block: &Block,
        ) -> Option<InstancePath> {
            for statement in block.iter_statements() {
                if let Statement::LocalAssign(local) = statement {
                    for (var, value) in local.iter_variables().zip(local.iter_values()) {
                        if var.get_identifier().get_name() == name {
                            log::trace!(
                                "resolve_identifier_to_instance_path: found local `{}`; try resolving its value",
                                name
                            );
                            if let Some(path) = this.parse_expression_to_instance_path(value) {
                                log::trace!(
                                    "resolve_identifier_to_instance_path: successfully resolved `{}`",
                                    name
                                );
                                return Some(path);
                            } else {
                                // Additionally handle locals initialized from a require call by resolving that require
                                if let Expression::Call(call) = value {
                                    if let Some((_, _, abs_instance_path)) = this.require_call(call) {
                                        log::trace!(
                                            "resolve_identifier_to_instance_path: resolved `{}` from require call",
                                            name
                                        );
                                        return Some(abs_instance_path);
                                    }
                                }
                                log::warn!(
                                    "resolve_identifier_to_instance_path: failed to resolve value for `{}`",
                                    name
                                );
                            }
                        }
                    }
                }
            }
            None
        }

        if let Some(p) = resolve_in_block(self, name, &self.current_block_clone) {
            return Some(p);
        }
        if let Some(p) = resolve_in_block(self, name, &self.root_block_clone) {
            return Some(p);
        }
        log::warn!(
            "resolve_identifier_to_instance_path: no matching local found for `{}` in root scope",
            name
        );
        None
    }

    fn instance_path_to_game_string(&self, path: &InstancePath) -> String {
        let mut s = String::new();
        match path.root() {
            InstancePathRoot::Root => {
                s.push_str("game");
            }
            InstancePathRoot::Script => {
                s.push_str("script");
            }
        }
        for component in path.components() {
            match component {
                InstancePathComponent::Parent => s.push_str(".Parent"),
                InstancePathComponent::Child(name) => {
                    s.push('.');
                    s.push_str(name);
                }
                InstancePathComponent::Ancestor(name) => {
                    s.push_str(&format!(".FindFirstAncestor(\"{}\")", name));
                }
            }
        }
        s
    }

    // Convert an InstancePath to a Prefix starting from DataModel (game)
    fn instance_path_to_game_prefix(&self, path: &InstancePath) -> Prefix {
        use crate::rules::convert_require::RobloxIndexStyle;
        // Manually build a property-based path from `game`, avoiding GetService for the first component
        let mut components = path.components().iter();
        let mut prefix: Prefix = datamodel_identifier().into();

        if let Some(first) = components.next() {
            match first {
                InstancePathComponent::Child(service_name) => {
                    prefix = FieldExpression::new(prefix, service_name).into();
                }
                _ => {
                    // Fallback to default conversion if unexpected component ordering
                    log::warn!(
                        concat!(
                            "cannot rewrite require to DataModel root (game): ",
                            "first component is not a service child; falling back to {:?}-rooted path"
                        ),
                        match path.root() {
                            InstancePathRoot::Root => "game",
                            InstancePathRoot::Script => "script",
                        }
                    );
                    return path.convert(&RobloxIndexStyle::Property);
                }
            }
        }

        for component in components {
            match component {
                InstancePathComponent::Parent => {
                    prefix = get_parent_instance(prefix);
                }
                InstancePathComponent::Child(child_name) => {
                    prefix = RobloxIndexStyle::Property.index(prefix, child_name);
                }
                InstancePathComponent::Ancestor(ancestor_name) => {
                    prefix = FunctionCall::from_prefix(prefix)
                        .with_method("FindFirstAncestor")
                        .with_argument(StringExpression::from_value(ancestor_name))
                        .into();
                }
            }
        }

        prefix
    }

    fn require_call(&self, call: &FunctionCall) -> Option<(String, PathBuf, InstancePath)> {
        if !is_require_call(call, self) {
            return None;
        }
        log::trace!(
            "require_call: processing require in `{}`",
            self.source.display()
        );
        let instance_path = match call.get_arguments() {
            Arguments::Tuple(tuple) if tuple.len() == 1 => {
                log::trace!("require_call: arguments is Tuple(len=1) -> attempt to resolve path from single argument");
                let expr = tuple.iter_values().next().unwrap();
                self.parse_expression_to_instance_path(expr)
            }
            Arguments::Tuple(tuple) => {
                log::trace!(
                    "require_call: arguments is Tuple(len={}), expected len=1",
                    tuple.len()
                );
                None
            }
            Arguments::String(_) => {
                log::warn!(
                    "require_call: arguments is a single String (unsupported for instance path)"
                );
                None
            }
            _ => {
                log::warn!("require_call: unsupported arguments variant for require call");
                None
            }
        };
        if instance_path.is_none() {
            log::warn!(
                "could not resolve instance path for require call in `{}`",
                self.source.display()
            );
            log::warn!(
                "require_call: instance path resolution returned None (check earlier trace logs for why)"
            );
            return None;
        }
        let instance_path = instance_path?;

        // Use sourcemap to resolve to a file path
        let source_path = &self.source;
        let target_file = match self
            .roblox_require_mode
            .get_file_from_instance_path(source_path, &instance_path)
        {
            Some(p) => p,
            None => {
                log::warn!(
                    concat!(
                        "require_call: unable to resolve file from instance path via sourcemap; ",
                        "instance path = {:?}, source = `{}`; ",
                        "skipping exclusion handling and DataModel-root rewrite for this require"
                    ),
                    instance_path,
                    source_path.display()
                );
                return None;
            }
        };

        // Prefer full instance path from DataModel if available
        let abs_instance_path = match self
            .roblox_require_mode
            .get_instance_path_for_file(source_path, &target_file)
        {
            Some(p) => p,
            None => {
                log::warn!(
                    concat!(
                        "unable to compute absolute DataModel path for `{}` from `{}` using sourcemap; ",
                        "falling back to path inferred from code (may be script-rooted)"
                    ),
                    target_file.display(),
                    source_path.display()
                );
                instance_path.clone()
            }
        };

        let roblox_reference = self.instance_path_to_game_string(&abs_instance_path);

        Some((roblox_reference, target_file, abs_instance_path))
    }

    fn try_inline_call(&mut self, call: &FunctionCall) -> Option<Expression> {
        let (roblox_reference, require_path, abs_instance_path) = self.require_call(call)?;

        if self.options.is_excluded(&require_path) {
            log::info!(
                "exclude `{}` from bundle [from `{}`]",
                require_path.display(),
                self.source.display()
            );
            // Instead of skipping, rewrite the require argument to a DataModel-rooted path
            let rewrite_path = self
                .roblox_require_mode
                .get_absolute_instance_path_for_file(&require_path)
                .or_else(|| self.roblox_require_mode.get_instance_path_for_file(&self.source, &require_path))
                .unwrap_or(abs_instance_path);
            if matches!(rewrite_path.root(), InstancePathRoot::Script) {
                log::warn!(
                    concat!(
                        "exclude prevented inlining but could not compute absolute DataModel path for `{}`; ",
                        "falling back to script-rooted path (require may not use `game` root)"
                    ),
                    require_path.display()
                );
            }
            let new_prefix = self.instance_path_to_game_prefix(&rewrite_path);
            let mut new_call = call.clone();
            new_call.set_arguments(Arguments::default().with_argument(new_prefix));
            return Some(Expression::Call(Box::new(new_call)));
        }

        if self.skip_module_paths.contains(&require_path) {
            log::trace!(
                "skip `{}` because it previously errored",
                require_path.display()
            );
            return None;
        }

        match self.inline_require(&roblox_reference, &require_path, call) {
            Ok(expression) => Some(expression),
            Err(error) => {
                self.errors.push(error.to_string());
                self.skip_module_paths.insert(require_path);
                None
            }
        }
    }

    fn inline_require(
        &mut self,
        roblox_reference: &str,
        require_path: &Path,
        call: &FunctionCall,
    ) -> DarkluaResult<Expression> {
        if let Some(expression) = self.module_cache.get(require_path) {
            Ok(expression.clone())
        } else {
            if let Some(i) = self
                .require_stack
                .iter()
                .enumerate()
                .find(|(_, path)| *path == require_path)
                .map(|(i, _)| i)
            {
                let require_stack_paths: Vec<_> = self
                    .require_stack
                    .iter()
                    .skip(i)
                    .map(|path| path.display().to_string())
                    .chain(iter::once(require_path.display().to_string()))
                    .collect();

                return Err(DarkluaError::custom(format!(
                    "cyclic require detected with `{}`",
                    require_stack_paths.join("` > `")
                )));
            }

            self.require_stack.push(require_path.to_path_buf());
            let required_resource = self.require_resource(require_path);
            self.require_stack.pop();

            let module_value = self
                .module_definitions
                .build_module_from_resource(required_resource?, require_path, roblox_reference, call)?;

            self.module_cache
                .insert(require_path.to_path_buf(), module_value.clone());

            Ok(module_value)
        }
    }

    fn require_resource(&mut self, path: impl AsRef<Path>) -> DarkluaResult<RequiredResource> {
        let path = path.as_ref();
        log::trace!("look for resource `{}`", path.display());
        let content = self.resources.get(path).map_err(DarkluaError::from)?;

        match path.extension() {
            Some(extension) => match extension.to_string_lossy().to_ascii_lowercase().as_str() {
                "lua" | "luau" => {
                    let parser_timer = Timer::now();
                    let mut block =
                        self.options
                            .parser()
                            .parse(&content)
                            .map_err(|parser_error| {
                                DarkluaError::parser_error(path.to_path_buf(), parser_error)
                            })?;
                    log::debug!(
                        "parsed `{}` in {}",
                        path.display(),
                        parser_timer.duration_label()
                    );

                    if self.options.parser().is_preserving_tokens() {
                        log::trace!("replacing token references of {}", path.display());
                        let context = ContextBuilder::new(path, self.resources, &content).build();
                        let replace_tokens = ReplaceReferencedTokens::default();

                        let apply_replace_tokens_timer = Timer::now();

                        replace_tokens.flawless_process(&mut block, &context);

                        log::trace!(
                            "replaced token references for `{}` in {}",
                            path.display(),
                            apply_replace_tokens_timer.duration_label()
                        );
                    }

                    let current_source = mem::replace(&mut self.source, path.to_path_buf());
                    let previous_block_clone =
                        mem::replace(&mut self.current_block_clone, block.clone());
                    let previous_root_block_clone =
                        mem::replace(&mut self.root_block_clone, block.clone());

                    let apply_processor_timer = Timer::now();
                    DefaultVisitor::visit_block(&mut block, self);

                    log::debug!(
                        "processed `{}` into bundle in {}",
                        path.display(),
                        apply_processor_timer.duration_label()
                    );

                    self.source = current_source;
                    self.current_block_clone = previous_block_clone;
                    self.root_block_clone = previous_root_block_clone;

                    Ok(RequiredResource::Block(block))
                }
                "json" | "json5" => {
                    transcode("json", path, json5::from_str::<serde_json::Value>, &content)
                }
                "yml" | "yaml" => transcode(
                    "yaml",
                    path,
                    serde_yaml::from_str::<serde_yaml::Value>,
                    &content,
                ),
                "toml" => transcode("toml", path, toml::from_str::<toml::Value>, &content),
                "txt" => Ok(RequiredResource::Expression(
                    StringExpression::from_value(content).into(),
                )),
                _ => Err(DarkluaError::invalid_resource_extension(path)),
            },
            None => unreachable!("extension should be defined"),
        }
    }
}

fn transcode<'a, T, E>(
    label: &'static str,
    path: &Path,
    deserialize_value: impl Fn(&'a str) -> Result<T, E>,
    content: &'a str,
) -> Result<RequiredResource, DarkluaError>
where
    T: serde::Serialize,
    E: Into<DarkluaError>,
{
    log::trace!("transcode {} data to Lua from `{}`", label, path.display());
    let transcode_duration = Timer::now();
    let value = deserialize_value(content).map_err(E::into)?;
    let expression = to_expression(&value)
        .map(RequiredResource::Expression)
        .map_err(DarkluaError::from);
    log::debug!(
        "transcoded {} data to Lua from `{}` in {}",
        label,
        path.display(),
        transcode_duration.duration_label()
    );
    expression
}

impl Deref for RequireRobloxProcessor<'_, '_, '_> {
    type Target = IdentifierTracker;

    fn deref(&self) -> &Self::Target {
        &self.identifier_tracker
    }
}

impl DerefMut for RequireRobloxProcessor<'_, '_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.identifier_tracker
    }
}

impl NodeProcessor for RequireRobloxProcessor<'_, '_, '_> {
    fn process_scope(&mut self, block: &mut Block, _extra: Option<&mut Expression>) {
        self.current_block_clone = block.clone();
    }

    fn process_expression(&mut self, expression: &mut Expression) {
        if let Expression::Call(call) = expression {
            if let Some(replace_with) = self.try_inline_call(call) {
                *expression = replace_with;
            }
        }
    }

    fn process_prefix_expression(&mut self, prefix: &mut Prefix) {
        if let Prefix::Call(call) = prefix {
            if let Some(replace_with) = self.try_inline_call(call) {
                *prefix = replace_with.into();
            }
        }
    }

    fn process_statement(&mut self, statement: &mut Statement) {
        if let Statement::Call(call) = statement {
            if let Some(replace_with) = self.try_inline_call(call) {
                if let Expression::Call(replace_with) = replace_with {
                    *call = *replace_with;
                } else {
                    *statement = convert_expression_to_statement(replace_with);
                }
            }
        }
    }
}

fn convert_expression_to_statement(expression: Expression) -> Statement {
    DoStatement::new(
        Block::default()
            .with_statement(LocalAssignStatement::from_variable("_").with_value(expression)),
    )
    .into()
}

pub(crate) fn process_block(
    block: &mut Block,
    context: &Context,
    options: &BundleOptions,
    roblox_require_mode: &RobloxRequireMode,
) -> Result<(), String> {
    if options.parser().is_preserving_tokens() {
        log::trace!(
            "replacing token references of {}",
            context.current_path().display()
        );
        let replace_tokens = ReplaceReferencedTokens::default();

        let apply_replace_tokens_timer = Timer::now();

        replace_tokens.flawless_process(block, context);

        log::trace!(
            "replaced token references for `{}` in {}",
            context.current_path().display(),
            apply_replace_tokens_timer.duration_label()
        );
    }

    let mut processor =
        RequireRobloxProcessor::new(context, options, roblox_require_mode, block.clone());
    ScopeVisitor::visit_block(block, &mut processor);
    processor.apply(block, context)
}
