mod module_definitions;

use module_definitions::BuildModuleDefinitions;

use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::{iter, mem};

use crate::frontend::DarkluaResult;
use crate::nodes::{
    Block, DoStatement, Expression, FunctionCall, LocalAssignStatement, Prefix, Statement,
};
use crate::process::{
    IdentifierTracker, NodeProcessor, NodeVisitor, ScopeVisitor,
};
use crate::rules::{
    Context, FlawlessRule, ReplaceReferencedTokens, RuleProcessResult,
};
use crate::utils::Timer;
use crate::{DarkluaError, Resources};

use super::BundleOptions;
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
    module_cache: HashMap<String, Expression>,
    require_stack: Vec<String>,
    skip_module_references: HashSet<String>,
    resources: &'resources Resources,
    errors: Vec<String>,
}

impl<'a, 'b, 'resources> RequireRobloxProcessor<'a, 'b, 'resources> {
    fn new<'context>(
        context: &'context Context<'b, 'resources, '_>,
        options: &'a BundleOptions,
        roblox_require_mode: &'b RobloxRequireMode,
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
            skip_module_references: Default::default(),
            resources: context.resources(),
            errors: Vec::new(),
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

    fn require_call(&self, _call: &FunctionCall) -> Option<String> {
        // TODO: Detect Roblox-specific require calls and resolve an identifier/reference string
        // e.g. return Some("ReplicatedStorage.Packages.MyModule".to_string());
        None
    }

    fn try_inline_call(&mut self, call: &FunctionCall) -> Option<Expression> {
        let roblox_reference = self.require_call(call)?;

        if self.skip_module_references.contains(&roblox_reference) {
            log::trace!("skip `{}` because it previously errored", roblox_reference);
            return None;
        }

        match self.inline_require(&roblox_reference, call) {
            Ok(expression) => Some(expression),
            Err(error) => {
                self.errors.push(error.to_string());
                self
                    .skip_module_references
                    .insert(roblox_reference);
                None
            }
        }
    }

    fn inline_require(&mut self, roblox_reference: &str, call: &FunctionCall) -> DarkluaResult<Expression> {
        if let Some(expression) = self.module_cache.get(roblox_reference) {
            Ok(expression.clone())
        } else {
            if let Some(i) = self
                .require_stack
                .iter()
                .enumerate()
                .find(|(_, reference)| *reference == roblox_reference)
                .map(|(i, _)| i)
            {
                let require_stack_refs: Vec<_> = self
                    .require_stack
                    .iter()
                    .skip(i)
                    .cloned()
                    .chain(iter::once(roblox_reference.to_string()))
                    .collect();

                return Err(DarkluaError::custom(format!(
                    "cyclic require detected with `{}`",
                    require_stack_refs.join("` > `")
                )));
            }

            self.require_stack.push(roblox_reference.to_string());
            let required_resource = self.require_resource(roblox_reference);
            self.require_stack.pop();

            let module_value = self.module_definitions.build_module_from_resource(
                required_resource?,
                roblox_reference,
                call,
            )?;

            self
                .module_cache
                .insert(roblox_reference.to_string(), module_value.clone());

            Ok(module_value)
        }
    }

    fn require_resource(&mut self, _roblox_reference: &str) -> DarkluaResult<RequiredResource> {
        // TODO: Resolve Roblox reference to a resource and parse it into a Block or Expression
        // Similar to path mode, once content is obtained:
        // - If Lua/Luau source: parse to Block, run ReplaceReferencedTokens if preserving tokens,
        //   visit recursively with this processor, and return RequiredResource::Block(block)
        // - If data files (json/yaml/toml/txt): transcode to Expression
        Err(DarkluaError::custom("roblox resource resolution is not implemented"))
    }
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

    let mut processor = RequireRobloxProcessor::new(context, options, roblox_require_mode);
    ScopeVisitor::visit_block(block, &mut processor);
    processor.apply(block, context)
} 