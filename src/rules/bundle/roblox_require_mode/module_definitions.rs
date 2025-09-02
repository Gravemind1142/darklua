use indexmap::IndexMap;
use std::path::PathBuf;

use crate::frontend::DarkluaResult;
use crate::nodes::{
    Arguments, AssignStatement, Block, DoStatement, Expression, FieldExpression, FunctionCall,
    FunctionExpression, FunctionName, FunctionStatement, Identifier, IfStatement, IndexExpression,
    LastStatement, LocalAssignStatement, Prefix, ReturnStatement, Statement, StringExpression,
    TableEntry, TableExpression, Token, TupleArguments, TupleArgumentsTokens, UnaryExpression,
    UnaryOperator,
};
use crate::process::utils::{generate_identifier, identifier_permutator, CharPermutator};
use crate::rules::bundle::RenameTypeDeclarationProcessor;
use crate::rules::{Context, FlawlessRule, LineMappingSegment, LineMappingSource, ShiftTokenLine};
use crate::utils::lines;
use crate::DarkluaError;

use super::RequiredResource;

#[derive(Debug)]
pub(crate) struct BuildModuleDefinitions {
    modules_identifier: String,
    module_definitions: IndexMap<String, ModuleDefinition>,
    module_name_permutator: CharPermutator,
    rename_type_declaration: RenameTypeDeclarationProcessor,
}

#[derive(Debug)]
struct ModuleDefinition {
    block: Block,
    path: PathBuf,
}

impl ModuleDefinition {
    fn new(block: Block, path: PathBuf) -> Self { Self { block, path } }
}

const BUNDLE_MODULES_VARIABLE_LOAD_FIELD: &str = "load";
const BUNDLE_MODULES_VARIABLE_CACHE_FIELD: &str = "cache";

impl BuildModuleDefinitions {
    pub(crate) fn new(modules_identifier: impl Into<String>) -> Self {
        let modules_identifier = modules_identifier.into();
        Self {
            modules_identifier: modules_identifier.clone(),
            module_definitions: Default::default(),
            module_name_permutator: identifier_permutator(),
            rename_type_declaration: RenameTypeDeclarationProcessor::new(
                modules_identifier,
                BUNDLE_MODULES_VARIABLE_LOAD_FIELD,
            ),
        }
    }

    pub(crate) fn build_module_from_resource(
        &mut self,
        required_resource: RequiredResource,
        source_path: &std::path::Path,
        roblox_reference: &str,
        call: &FunctionCall,
    ) -> DarkluaResult<Expression> {
        let mut block = match required_resource {
            RequiredResource::Block(mut block) => {
                if let Some(LastStatement::Return(return_statement)) = block.get_last_statement() {
                    if return_statement.len() != 1 {
                        return Err(DarkluaError::custom(format!(
                            "invalid Lua module at `{}`: module must return exactly one value",
                            roblox_reference
                        )));
                    }
                } else {
                    block.set_last_statement(ReturnStatement::one(Expression::nil()));
                };
                block
            }
            RequiredResource::Expression(expression) => {
                Block::default().with_last_statement(ReturnStatement::one(expression))
            }
        };

        let exported_types = self
            .rename_type_declaration
            .extract_exported_types(&mut block);

        let module_name = self.generate_module_name();

        self.module_definitions.insert(
            module_name.clone(),
            ModuleDefinition::new(block, source_path.to_path_buf()),
        );
        self.rename_type_declaration
            .insert_module_types(module_name.clone(), exported_types);

        let token_trivia_identifier = match call.get_prefix() {
            Prefix::Identifier(require_identifier) => require_identifier.get_token(),
            _ => None,
        };

        let load_field = if let Some(token_trivia_identifier) = token_trivia_identifier {
            let mut field_token = Token::from_content(BUNDLE_MODULES_VARIABLE_LOAD_FIELD);
            for trivia in token_trivia_identifier.iter_trailing_trivia() {
                field_token.push_trailing_trivia(trivia.clone());
            }
            Identifier::new(BUNDLE_MODULES_VARIABLE_LOAD_FIELD).with_token(field_token)
        } else {
            Identifier::new(BUNDLE_MODULES_VARIABLE_LOAD_FIELD)
        };

        let arguments = match call.get_arguments() {
            Arguments::Tuple(original_args) => {
                if let Some(original_tokens) = original_args.get_tokens() {
                    TupleArguments::default().with_tokens(TupleArgumentsTokens {
                        opening_parenthese: transfer_trivia(
                            Token::from_content("("),
                            &original_tokens.opening_parenthese,
                        ),
                        closing_parenthese: transfer_trivia(
                            Token::from_content(")"),
                            &original_tokens.closing_parenthese,
                        ),
                        commas: Vec::new(),
                    })
                } else {
                    TupleArguments::default()
                }
            }
            Arguments::String(string_expression) => {
                if let Some(string_token) = string_expression.get_token() {
                    TupleArguments::default().with_tokens(TupleArgumentsTokens {
                        opening_parenthese: Token::from_content("("),
                        closing_parenthese: transfer_trivia(Token::from_content(")"), string_token),
                        commas: Vec::new(),
                    })
                } else {
                    TupleArguments::default()
                }
            }
            Arguments::Table(_) => TupleArguments::default(),
        };

        let new_require_call = FunctionCall::from_prefix(FieldExpression::new(
            Identifier::from(&self.modules_identifier),
            load_field,
        ))
        .with_arguments(arguments.with_argument(StringExpression::from_value(module_name)))
        .into();

        Ok(new_require_call)
    }

    fn generate_module_name(&mut self) -> String {
        loop {
            let name = generate_identifier(&mut self.module_name_permutator);

            if name != BUNDLE_MODULES_VARIABLE_CACHE_FIELD
                && name != BUNDLE_MODULES_VARIABLE_LOAD_FIELD
            {
                break name;
            }
        }
    }

    pub(crate) fn apply(mut self, block: &mut Block, context: &Context) {
        if self.module_definitions.is_empty() {
            return;
        }


        self.rename_type_declaration.rename_types(block);

        let modules_identifier = Identifier::from(&self.modules_identifier);

        let mut shift_lines = self.rename_type_declaration.get_type_lines();
        for module in self.module_definitions.values_mut() {
            let inserted_lines = lines::block_total(&module.block);

            // record mapping for this module segment before shifting, using source file path via rojo sourcemap resolution earlier
            let original_first = lines::block_first(&module.block);
            let original_last = lines::block_total(&module.block);
            if original_last >= original_first && original_first != 0 {
                let bundle_start = (shift_lines + 1).max(1) as usize;
                let span = original_last.saturating_sub(original_first) + 1;
                let bundle_end = bundle_start + span.saturating_sub(1);
                context.add_line_mapping_segment(LineMappingSegment {
                    bundle_start,
                    bundle_end,
                    source: Some(LineMappingSource { path: module.path.clone(), shift: shift_lines }),
                });
            }

            ShiftTokenLine::new(shift_lines).flawless_process(&mut module.block, context);

            shift_lines += inserted_lines as isize;
        }

        // map root block lines before shifting
        let root_first = lines::block_first(block);
        let root_last = lines::block_total(block);
        if root_last >= root_first && root_first != 0 {
            let bundle_start = (shift_lines + 1).max(1) as usize;
            let span = root_last.saturating_sub(root_first) + 1;
            let bundle_end = bundle_start + span.saturating_sub(1);
            context.add_line_mapping_segment(LineMappingSegment {
                bundle_start,
                bundle_end,
                source: Some(LineMappingSource { path: context.current_path().to_path_buf(), shift: shift_lines }),
            });
        }

        ShiftTokenLine::new(shift_lines).flawless_process(block, context);

        let statements = self
            .module_definitions
            .drain(..)
            .map(|(module_name, module)| {
                let function_name =
                    FunctionName::from_name(modules_identifier.clone()).with_field(&module_name);
                FunctionStatement::new(function_name, module.block, Vec::new(), false)
            })
            .map(Statement::from)
            .collect();
        block.insert_statement(0, DoStatement::new(Block::new(statements, None)));

        let modules_table = self.build_modules_table();
        block.insert_statement(
            0,
            AssignStatement::from_variable(modules_identifier, modules_table),
        );
        block.insert_statement(
            0,
            LocalAssignStatement::from_variable(self.modules_identifier),
        );

        for statement in self
            .rename_type_declaration
            .extract_type_declarations()
            .into_iter()
            .rev()
        {
            block.insert_statement(0, statement);
        }
    }

    fn build_modules_table(&self) -> TableExpression {
        let module_content_entry = "c";
        let parameter_name = "m";
        let index_cache = IndexExpression::new(
            FieldExpression::new(
                Identifier::from(&self.modules_identifier),
                BUNDLE_MODULES_VARIABLE_CACHE_FIELD,
            ),
            Identifier::from(parameter_name),
        );
        let load_function = FunctionExpression::from_block(
            Block::default()
                .with_statement(IfStatement::create(
                    UnaryExpression::new(UnaryOperator::Not, index_cache.clone()),
                    AssignStatement::from_variable(
                        index_cache.clone(),
                        TableExpression::default().append_entry(
                            TableEntry::from_string_key_and_value(
                                module_content_entry,
                                FunctionCall::from_prefix(IndexExpression::new(
                                    Identifier::from(&self.modules_identifier),
                                    Identifier::from(parameter_name),
                                )),
                            ),
                        ),
                    ),
                ))
                .with_last_statement(ReturnStatement::one(FieldExpression::new(
                    index_cache,
                    module_content_entry,
                ))),
        )
        .with_parameter(parameter_name);

        TableExpression::default()
            .append_entry(TableEntry::from_string_key_and_value(
                BUNDLE_MODULES_VARIABLE_CACHE_FIELD,
                TableExpression::default(),
            ))
            .append_field(BUNDLE_MODULES_VARIABLE_LOAD_FIELD, load_function)
    }
}

fn transfer_trivia(mut receiving_token: Token, take_token: &Token) -> Token {
    for (content, kind) in take_token.iter_trailing_trivia().filter_map(|trivia| {
        trivia
            .try_read()
            .map(str::to_owned)
            .zip(Some(trivia.kind()))
    }) {
        receiving_token.push_trailing_trivia(kind.with_content(content));
    }
    receiving_token
}
