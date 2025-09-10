use std::path::Path;

use super::{
    configuration::Configuration,
    resources::Resources,
    utils::maybe_plural,
    work_cache::WorkCache,
    work_item::{WorkItem, WorkProgress, WorkStatus},
    DarkluaError, DarkluaResult, Options,
};

use crate::{
    nodes::Block,
    rules::{bundle::Bundler, ContextBuilder, FlawlessRule, Rule, RuleConfiguration},
    utils::{normalize_path, Timer},
    GeneratorParameters,
};

use crate::utils::source_registry::SourceRegistry;

use crate::process::set_instance_indexing_is_pure;
use crate::process::{clear_known_instance_aliases, set_known_instance_aliases};
use crate::rules::ReplaceReferencedTokens;
use crate::process::{DefaultVisitor, NodeProcessor, NodeVisitor};

struct InstanceAliasCollector(std::collections::HashSet<String>);

impl InstanceAliasCollector {
    fn new() -> Self { Self(Default::default()) }
    fn into_set(self) -> std::collections::HashSet<String> { self.0 }
}

impl NodeProcessor for InstanceAliasCollector {
    fn process_local_assign_statement(&mut self, assign: &mut crate::nodes::LocalAssignStatement) {
        // Detect aliases to instance paths (supports chaining through previously discovered aliases)
        use crate::nodes::{Arguments, Expression, FunctionCall, Prefix};

        fn is_string_literal(expression: &Expression) -> bool {
            matches!(expression, Expression::String(s) if s.get_string_value().is_some())
        }

        fn call_has_indexing_semantics(call: &FunctionCall) -> bool {
            if let Some(method) = call.get_method() {
                let name = method.get_name();
                let supported = matches!(
                    name.as_str(),
                    "WaitForChild" | "FindFirstChild" | "FindFirstAncestor" | "GetService"
                );
                if supported {
                    match call.get_arguments() {
                        Arguments::String(s) => s.get_string_value().is_some(),
                        Arguments::Tuple(tuple) if tuple.len() >= 1 => tuple
                            .iter_values()
                            .next()
                            .map(is_string_literal)
                            .unwrap_or(false),
                        _ => false,
                    }
                } else {
                    false
                }
            } else {
                false
            }
        }

        fn prefix_is_instance_path(prefix: &Prefix, known: &std::collections::HashSet<String>) -> bool {
            match prefix {
                Prefix::Identifier(id) => {
                    let name = id.get_name();
                    matches!(name.as_str(), "script" | "game") || known.contains(name.as_str())
                }
                Prefix::Field(field) => prefix_is_instance_path(field.get_prefix(), known),
                Prefix::Index(index) => {
                    prefix_is_instance_path(index.get_prefix(), known) && is_string_literal(index.get_index())
                }
                Prefix::Call(call) => call_has_indexing_semantics(call) && prefix_is_instance_path(call.get_prefix(), known),
                Prefix::Parenthese(paren) => expression_is_instance_path(paren.inner_expression(), known),
            }
        }

        fn expression_is_instance_path(
            expression: &Expression,
            known: &std::collections::HashSet<String>,
        ) -> bool {
            match expression {
                Expression::Identifier(id) => {
                    let name = id.get_name();
                    matches!(name.as_str(), "script" | "game") || known.contains(name.as_str())
                }
                Expression::Field(field) => prefix_is_instance_path(field.get_prefix(), known),
                Expression::Index(index) => {
                    prefix_is_instance_path(index.get_prefix(), known) && is_string_literal(index.get_index())
                }
                Expression::Call(call) => call_has_indexing_semantics(call) && prefix_is_instance_path(call.get_prefix(), known),
                Expression::Parenthese(p) => expression_is_instance_path(p.inner_expression(), known),
                _ => false,
            }
        }

        for (var, value) in assign.iter_variables().zip(assign.iter_values()) {
            if expression_is_instance_path(value, &self.0) {
                self.0.insert(var.get_identifier().get_name().to_string());
            }
        }
    }
}

const DEFAULT_CONFIG_PATHS: [&str; 2] = [".darklua.json", ".darklua.json5"];

#[derive(Debug)]
pub(crate) struct Worker<'a> {
    resources: &'a Resources,
    cache: WorkCache<'a>,
    configuration: Configuration,
    cached_bundler: Option<Bundler>,
    shared_registry: std::rc::Rc<std::cell::RefCell<SourceRegistry>>,
}

impl<'a> Worker<'a> {
    pub(crate) fn new(resources: &'a Resources) -> Self {
        Self {
            resources,
            cache: WorkCache::new(resources),
            configuration: Configuration::default(),
            cached_bundler: None,
            shared_registry: std::rc::Rc::new(std::cell::RefCell::new(SourceRegistry::new())),
        }
    }

    pub(crate) fn setup_worker(&mut self, options: &mut Options) -> DarkluaResult<()> {
        let configuration_setup_timer = Timer::now();

        if let Some(config) = options.take_configuration() {
            self.configuration = config;
            if let Some(config_path) = options.configuration_path() {
                log::warn!(
                    concat!(
                        "the provided options contained both a configuration object and ",
                        "a path to a configuration file (`{}`). the provided configuration ",
                        "takes precedence, so it is best to avoid confusion by providing ",
                        "only the configuration itself or a path to a configuration"
                    ),
                    config_path.display()
                );
            }
        } else if let Some(config) = options.configuration_path() {
            if self.resources.exists(config)? {
                self.configuration = self.read_configuration(config)?;
                log::info!("using configuration file `{}`", config.display());
            } else {
                return Err(DarkluaError::resource_not_found(config)
                    .context("expected to find configuration file as provided by the options"));
            }
        } else {
            let mut configuration_files = Vec::new();
            for path in DEFAULT_CONFIG_PATHS.iter().map(Path::new) {
                if self.resources.exists(path)? {
                    configuration_files.push(path);
                }
            }

            match configuration_files.len() {
                0 => {
                    log::info!("using default configuration");
                }
                1 => {
                    let configuration_file_path = configuration_files.first().unwrap();
                    self.configuration = self.read_configuration(configuration_file_path)?;
                    log::info!(
                        "using configuration file `{}`",
                        configuration_file_path.display()
                    );
                }
                _ => {
                    return Err(DarkluaError::multiple_configuration_found(
                        configuration_files.into_iter().map(Path::to_path_buf),
                    ))
                }
            }
        };

        if let Some(generator) = options.generator_override() {
            log::trace!(
                "override with {} generator",
                match generator {
                    GeneratorParameters::RetainLines => "`retain_lines`".to_owned(),
                    GeneratorParameters::RetainLinesCompact { max_empty_lines } =>
                        format!("retain_lines_compact (max_empty_lines={})", max_empty_lines),
                    GeneratorParameters::Dense { column_span } =>
                        format!("dense ({})", column_span),
                    GeneratorParameters::Readable { column_span } =>
                        format!("readable ({})", column_span),
                }
            );
            self.configuration.set_generator(generator.clone());
        }

        log::trace!(
            "configuration setup in {}",
            configuration_setup_timer.duration_label()
        );
        log::debug!(
            "using configuration: {}",
            json5::to_string(&self.configuration).unwrap_or_else(|err| {
                format!("? (unable to serialize configuration: {})", err)
            })
        );

        // Apply global evaluator behavior based on configuration
        set_instance_indexing_is_pure(self.configuration.instance_indexing_is_pure());

        Ok(())
    }

    pub(crate) fn configuration(&self) -> &Configuration {
        &self.configuration
    }

    pub(crate) fn advance_work(&mut self, work_item: &mut WorkItem) -> DarkluaResult<()> {
        match &work_item.status {
            WorkStatus::NotStarted => {
                let source_display = work_item.source().display();

                let content = self.resources.get(work_item.source())?;


                let parser = self.configuration.build_parser();

                log::debug!("beginning work on `{}`", source_display);

                let parser_timer = Timer::now();

                let mut block = {
                    // If sourcemaps are enabled for bundling, parse the entry file with the
                    // shared registry source_id so that sourcemap indices align.
                    let sourcemap_requested = self
                        .configuration
                        .bundle_config()
                        .and_then(|bundle| bundle.sourcemap())
                        .map_or(false, |sm| sm.enabled);
                    let use_shared_registry = sourcemap_requested && self.configuration.is_retain_lines();

                    if use_shared_registry {
                        let source_id = self.shared_registry.borrow_mut().intern(work_item.source());
                        parser
                            .parse_with_source_id(source_id, &content)
                            .map_err(|parser_error| {
                                DarkluaError::parser_error(work_item.source(), parser_error)
                            })?
                    } else {
                        parser
                            .parse(&content)
                            .map_err(|parser_error| {
                                DarkluaError::parser_error(work_item.source(), parser_error)
                            })?
                    }
                };

                let parser_time = parser_timer.duration_label();
                log::debug!("parsed `{}` in {}", source_display, parser_time);

                // If configured, precompute aliases to instance paths for this block
                if self.configuration.instance_indexing_is_pure() {
                    let mut collector = InstanceAliasCollector::new();
                    DefaultVisitor::visit_block(&mut block, &mut collector);
                    set_known_instance_aliases(collector.into_set());
                } else {
                    clear_known_instance_aliases();
                }

                self.bundle(work_item, &mut block, &content)?;

                work_item.status = WorkProgress::new(content, block).into();

                self.apply_rules(work_item)
            }
            WorkStatus::InProgress(_work_progress) => self.apply_rules(work_item),
            WorkStatus::Done(_) => Ok(()),
        }
    }

    fn read_configuration(&self, config: &Path) -> DarkluaResult<Configuration> {
        let config_content = self.resources.get(config)?;
        json5::from_str(&config_content)
            .map_err(|err| {
                DarkluaError::invalid_configuration_file(config).context(err.to_string())
            })
            .map(|configuration: Configuration| {
                configuration.with_location({
                    config.parent().unwrap_or_else(|| {
                        log::warn!(
                            "unexpected configuration path `{}` (unable to extract parent path)",
                            config.display()
                        );
                        config
                    })
                })
            })
    }

    fn apply_rules(&mut self, work_item: &mut WorkItem) -> DarkluaResult<()> {
        let work_progress = match &mut work_item.status {
            WorkStatus::InProgress(progress) => progress.as_mut(),
            _ => return Ok(()),
        };

        let progress = &mut work_progress.progress;

        let source_display = work_item.data.source().display();
        let normalized_source = normalize_path(work_item.data.source());

        progress.duration().start();

        for (index, rule) in self
            .configuration
            .rules()
            .enumerate()
            .skip(progress.next_rule())
        {
            let mut context_builder =
                self.create_rule_context(work_item.data.source(), &work_progress.content);
            log::trace!(
                "[{}] apply rule `{}`{}",
                source_display,
                rule.get_name(),
                if rule.has_properties() {
                    format!(" {:?}", rule.serialize_to_properties())
                } else {
                    "".to_owned()
                }
            );
            let mut required_content: Vec<_> = rule
                .require_content(&normalized_source, progress.block())
                .into_iter()
                .map(normalize_path)
                .filter(|path| {
                    if *path == normalized_source {
                        log::debug!("filtering out currently processing path");
                        false
                    } else {
                        true
                    }
                })
                .collect();
            required_content.sort();
            required_content.dedup();

            if !required_content.is_empty() {
                if required_content
                    .iter()
                    .all(|path| self.cache.contains(path))
                {
                    let parser = self.configuration.build_parser();
                    for path in required_content.iter() {
                        let block = self.cache.get_block(path, &parser)?;
                        context_builder.insert_block(path, block);
                    }
                } else {
                    progress.duration().pause();
                    log::trace!(
                        "queue work for `{}` at rule `{}` (#{}) because it requires:{}",
                        source_display,
                        rule.get_name(),
                        index,
                        if required_content.len() == 1 {
                            format!(" {}", required_content.first().unwrap().display())
                        } else {
                            format!(
                                "\n- {}",
                                required_content
                                    .iter()
                                    .map(|path| format!("- {}", path.display()))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            )
                        }
                    );

                    progress.set_next_rule(index);
                    progress.set_required_content(required_content);
                    return Ok(());
                }
            }

            let context = context_builder.build();
            let block = progress.mutate_block();
            let rule_timer = Timer::now();

            // Recompute instance aliases prior to running each rule to reflect any changes
            if self.configuration.instance_indexing_is_pure() {
                let mut collector = InstanceAliasCollector::new();
                DefaultVisitor::visit_block(block, &mut collector);
                set_known_instance_aliases(collector.into_set());
            }

            let source = work_item.data.source();

            let rule_result = rule.process(block, &context).map_err(|rule_error| {
                let error = DarkluaError::rule_error(source, rule, index, rule_error);

                log::trace!(
                    "[{}] rule `{}` errored: {}",
                    source_display,
                    rule.get_name(),
                    error
                );

                error
            });

            work_item
                .external_file_dependencies
                .extend(context.into_dependencies());

            rule_result?;

            let rule_duration = rule_timer.duration_label();
            log::trace!(
                "[{}] ⨽completed `{}` in {}",
                source_display,
                rule.get_name(),
                rule_duration
            );
        }

        // Final cleanup pass to remove variables that became unused after prior rules
        if self.configuration.instance_indexing_is_pure() {
            let cleanup_context = self
                .create_rule_context(work_item.data.source(), &work_progress.content)
                .build();
            let cleanup_rule = crate::rules::RemoveUnusedVariable::default();
            cleanup_rule.flawless_process(progress.mutate_block(), &cleanup_context);
            ReplaceReferencedTokens::default()
                .flawless_process(progress.mutate_block(), &cleanup_context);
        }

        let rule_time = progress.duration().duration_label();
        let total_rules = self.configuration.rules_len();
        log::debug!(
            "{} rule{} applied in {} for `{}`",
            total_rules,
            maybe_plural(total_rules),
            rule_time,
            source_display,
        );

        log::trace!("begin generating code for `{}`", source_display);

        if cfg!(test) || (cfg!(debug_assertions) && log::log_enabled!(log::Level::Trace)) {
            log::trace!(
                "generate AST debugging view at `{}`",
                work_item.data.output().display()
            );
            self.resources
                .write(work_item.data.output(), &format!("{:#?}", progress.block()))?;
        }

        let generator_timer = Timer::now();

        let lua_code = if self.configuration.is_retain_lines() {
            log::trace!("Retain lines mode enabled for `{}`", source_display);
            match self
                .configuration
                .bundle_config()
                .and_then(|bundle| bundle.sourcemap())
                .filter(|sm| sm.enabled)
            {
                Some(sm) => {
                    log::trace!(
                        "Sourcemap generation requested for `{}` (output path: {:?})",
                        source_display,
                        sm.output_path
                    );
                    use sourcemap::SourceMapBuilder;
                    use crate::generator::LuaGenerator;
                    // Initialize SourceMapBuilder with the same source registry order as the bundler
                    let mut builder = SourceMapBuilder::new(None);
                    if let Some(root) = &sm.source_root {
                        log::trace!("Setting sourcemap source root: {}", root);
                        builder.set_source_root(Some(root.as_str()));
                    }


                    // Optionally relativize source paths to avoid leaking absolute paths
                    let relative_base: Option<std::path::PathBuf> = if let Some(root) = &sm.relative_to {
                        // If the configured base is relative, resolve it against the configuration location when available
                        let candidate = std::path::PathBuf::from(root);
                        if candidate.is_absolute() {
                            Some(candidate)
                        } else if let Some(cfg_loc) = self.configuration.location() {
                            Some(cfg_loc.join(candidate))
                        } else {
                            Some(candidate)
                        }
                    } else if let Some(cfg_loc) = self.configuration.location() {
                        Some(cfg_loc.to_path_buf())
                    } else {
                        None
                    };


                    // Pre-register all known source paths so the sourcemap `sources` field
                    // contains every file that participated in the bundle, even if some
                    // do not end up with explicit mappings on certain lines.
                    {
                        use std::path::PathBuf;
                        let paths: Vec<PathBuf> = if let Some(bundler) = self.cached_bundler.as_ref() {
                            bundler
                                .options()
                                .source_paths_snapshot()
                                .into_iter()
                                .map(PathBuf::from)
                                .collect()
                        } else {
                            let reg = self.shared_registry.borrow();
                            (0..reg.len())
                                .filter_map(|i| reg.get_path(i as u32).map(|p| p.to_path_buf()))
                                .collect()
                        };

                        for p in paths {
                            // Normalize and relativize similar to how MappingRecorder does
                            let p_norm = crate::utils::normalize_path_with_current_dir(&p);
                            let src_name = if let Some(base) = &relative_base {
                                let base_norm = crate::utils::normalize_path_with_current_dir(base);
                                match p_norm.strip_prefix(&base_norm) {
                                    Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
                                    Err(_) => p_norm.to_string_lossy().replace('\\', "/"),
                                }
                            } else {
                                p_norm.to_string_lossy().replace('\\', "/")
                            };
                            builder.add_source(&src_name);
                        }
                    }

                    // Set the sourcemap "file". If an explicit override is provided, use it; otherwise
                    // compute from the generated output path, respecting the same relative base.
                    {
                        if let Some(override_file) = sm.file.as_ref() {
                            builder.set_file(Some(override_file.as_str()));
                        } else {
                            let out_path_norm = crate::utils::normalize_path_with_current_dir(work_item.data.output());
                            let file_path = if let Some(base) = relative_base.as_ref() {
                                let base = crate::utils::normalize_path_with_current_dir(base);
                                match out_path_norm.strip_prefix(&base) {
                                    Ok(rel) => rel.to_path_buf(),
                                    Err(_) => out_path_norm.clone(),
                                }
                            } else {
                                out_path_norm.clone()
                            };
                            let file_string = file_path.to_string_lossy().replace('\\', "/");
                            builder.set_file(Some(file_string.as_str()));
                        }
                    }


                    // Choose retain-lines variant with sourcemap support
                    let (code, map_opt) = if let Some(max_empty) = self.configuration.retain_lines_compact_max_empty() {
                        let mut gen = crate::generator::RetainLinesCompactLuaGenerator::new(&work_progress.content, max_empty)
                            .with_sourcemap(builder, self.shared_registry.clone(), relative_base.clone());
                        gen.write_block(progress.block());
                        gen.into_string_and_sourcemap()
                    } else {
                        let mut gen = crate::generator::TokenBasedLuaGenerator::new(&work_progress.content)
                            .with_sourcemap(builder, self.shared_registry.clone(), relative_base.clone());
                        gen.write_block(progress.block());
                        gen.into_string_and_sourcemap()
                    };

                    if let (Some(map), Some(path)) = (map_opt, sm.output_path.as_ref()) {
                        let mut out = Vec::new();
                        match map.to_writer(&mut out) {
                            Err(err) => {
                                log::warn!("failed to write sourcemap JSON: {}", err);
                                log::trace!("Sourcemap for `{}` was NOT written due to JSON serialization error", source_display);
                            }
                            Ok(_) => match String::from_utf8(out) {
                                Ok(json) => {
                                    // Resolve output path relative to configuration location when needed
                                    let target_path = if path.is_absolute() {
                                        path.clone()
                                    } else if let Some(base) = self.configuration.location() {
                                        base.join(path)
                                    } else {
                                        path.clone()
                                    };

                                    match self.resources.write(&target_path, &json) {
                                        Ok(_) => {
                                            let abs_for_log = target_path
                                                .canonicalize()
                                                .unwrap_or_else(|_| {
                                                    if target_path.is_absolute() {
                                                        target_path.clone()
                                                    } else {
                                                        std::env::current_dir()
                                                            .map(|cwd| cwd.join(&target_path))
                                                            .unwrap_or_else(|_| target_path.clone())
                                                    }
                                                });
                                            log::trace!(
                                                "Sourcemap for `{}` successfully written to `{}`",
                                                source_display,
                                                abs_for_log.display()
                                            );
                                        }
                                        Err(err) => {
                                            let abs_for_log = if target_path.is_absolute() {
                                                target_path.clone()
                                            } else {
                                                std::env::current_dir()
                                                    .map(|cwd| cwd.join(&target_path))
                                                    .unwrap_or_else(|_| target_path.clone())
                                            };
                                            log::warn!(
                                                "failed to write sourcemap to `{}`: {:?}",
                                                abs_for_log.display(),
                                                err
                                            );
                                        }
                                    }
                                },
                                Err(err) => {
                                    log::warn!("failed to convert sourcemap to UTF-8 string: {}", err);
                                }
                            },
                        }
                    }

                    code
                }
                None => {
                    log::trace!(
                        "Sourcemap generation NOT requested for `{}`: either no bundle config, no sourcemap config, or sourcemap not enabled",
                        source_display
                    );
                    self.configuration.generate_lua(progress.block(), &work_progress.content)
                }
            }
        } else {
            // Warn if sourcemaps were requested but the generator mode is incompatible
            let sourcemap_requested = self
                .configuration
                .bundle_config()
                .and_then(|bundle| bundle.sourcemap())
                .map_or(false, |sm| sm.enabled);

            if sourcemap_requested {
                log::warn!(
                    "sourcemap generation requested for `{}` but current generator mode does not support sourcemaps; enable the `retain_lines` or `retain_lines_compact` generator to produce sourcemaps",
                    source_display
                );
            } else {
                log::trace!(
                    "Retain lines mode NOT enabled for `{}`; skipping sourcemap generation",
                    source_display
                );
            }

            self.configuration.generate_lua(progress.block(), &work_progress.content)
        };

        let generator_time = generator_timer.duration_label();
        log::debug!(
            "generated code for `{}` in {}",
            source_display,
            generator_time,
        );

        self.resources.write(work_item.data.output(), &lua_code)?;

        self.cache
            .link_source_to_output(normalized_source, work_item.data.output());

        work_item.status = WorkStatus::done();
        Ok(())
    }

    fn create_rule_context<'block, 'src>(
        &self,
        source: &Path,
        original_code: &'src str,
    ) -> ContextBuilder<'block, 'a, 'src> {
        let builder = ContextBuilder::new(normalize_path(source), self.resources, original_code);
        if let Some(project_location) = self.configuration.location() {
            builder.with_project_location(project_location)
        } else {
            builder
        }
    }

    fn bundle(
        &mut self,
        work_item: &mut WorkItem,
        block: &mut Block,
        original_code: &str,
    ) -> DarkluaResult<()> {
        if self.cached_bundler.is_none() {
            if let Some(bundler) = self.configuration.bundle() {
                // Ensure bundler uses the shared registry so source_id indices are unified
                self.cached_bundler = Some(bundler.with_registry(self.shared_registry.clone()));
            }
        }
        let bundler = match self.cached_bundler.as_ref() {
            Some(bundler) => bundler,
            None => return Ok(()),
        };

        log::debug!("beginning bundling from `{}`", work_item.source().display());

        let bundle_timer = Timer::now();

        let context = self
            .create_rule_context(work_item.source(), original_code)
            .build();

        let rule_result = bundler.process(block, &context).map_err(|rule_error| {
            let error = DarkluaError::orphan_rule_error(work_item.source(), bundler, rule_error);

            log::trace!(
                "[{}] rule `{}` errored: {}",
                work_item.source().display(),
                bundler.get_name(),
                error
            );

            error
        });

        work_item
            .external_file_dependencies
            .extend(context.into_dependencies());

        rule_result?;

        let bundle_time = bundle_timer.duration_label();
        log::debug!(
            "bundled `{}` in {}",
            work_item.source().display(),
            bundle_time
        );

        Ok(())
    }
}
