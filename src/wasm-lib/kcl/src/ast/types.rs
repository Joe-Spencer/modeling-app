//! Data types for the AST.

use std::{
    collections::HashMap,
    ops::RangeInclusive,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use async_recursion::async_recursion;
use databake::*;
use parse_display::{Display, FromStr};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value as JValue;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, DocumentSymbol, FoldingRange, FoldingRangeKind, Range as LspRange, SymbolKind,
};

use self::execute::execute_pipe_body;
pub use crate::ast::types::{
    condition::{ElseIf, IfExpression},
    literal_value::LiteralValue,
    none::KclNone,
};
use crate::{
    docs::StdLibFn,
    errors::KclError,
    executor::{ExecState, ExecutorContext, KclValue, Metadata, SourceRange, TagIdentifier, UserVal},
    parser::PIPE_OPERATOR,
    std::kcl_stdlib::KclStdLibFn,
};

mod condition;
pub(crate) mod digest;
pub(crate) mod execute;
mod literal_value;
mod none;

use digest::Digest;

pub enum Definition<'a> {
    Variable(&'a VariableDeclarator),
    Import(&'a ImportStatement),
}

/// A KCL program top level, or function body.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct Program {
    pub start: usize,
    pub end: usize,
    pub body: Vec<BodyItem>,
    #[serde(default, skip_serializing_if = "NonCodeMeta::is_empty")]
    pub non_code_meta: NonCodeMeta,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl Program {
    /// Is the last body item an expression?
    pub fn ends_with_expr(&self) -> bool {
        let Some(ref last) = self.body.last() else {
            return false;
        };
        matches!(last, BodyItem::ExpressionStatement(_))
    }

    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        // Check if we are in the non code meta.
        if let Some(meta) = self.get_non_code_meta_for_position(pos) {
            for node in &meta.start {
                if node.contains(pos) {
                    // We only care about the shebang.
                    if let NonCodeValue::Shebang { value: _ } = &node.value {
                        let source_range: SourceRange = node.into();
                        return Some(Hover::Comment {
                            value: r#"The `#!` at the start of a script, known as a shebang, specifies the path to the interpreter that should execute the script. This line is not necessary for your `kcl` to run in the modeling-app. You can safely delete it. If you wish to learn more about what you _can_ do with a shebang, read this doc: [zoo.dev/docs/faq/shebang](https://zoo.dev/docs/faq/shebang)."#.to_string(),
                            range: source_range.to_lsp_range(code),
                        });
                    }
                }
            }
        }

        let value = self.get_expr_for_position(pos)?;

        value.get_hover_value_for_position(pos, code)
    }

    /// Check the provided Program for any lint findings.
    pub fn lint<'a, RuleT>(&'a self, rule: RuleT) -> Result<Vec<crate::lint::Discovered>>
    where
        RuleT: crate::lint::rule::Rule<'a>,
    {
        let v = Arc::new(Mutex::new(vec![]));
        crate::walk::walk(self, &|node: crate::walk::Node<'a>| {
            let mut findings = v.lock().map_err(|_| anyhow::anyhow!("mutex"))?;
            findings.append(&mut rule.check(node)?);
            Ok(true)
        })?;
        let x = v.lock().unwrap();
        Ok(x.clone())
    }

    pub fn lint_all(&self) -> Result<Vec<crate::lint::Discovered>> {
        let rules = vec![
            crate::lint::checks::lint_variables,
            crate::lint::checks::lint_object_properties,
            crate::lint::checks::lint_call_expressions,
        ];

        let mut findings = vec![];
        for rule in rules {
            findings.append(&mut self.lint(rule)?);
        }
        Ok(findings)
    }

    /// Walk the ast and get all the variables and tags as completion items.
    pub fn completion_items<'a>(&'a self) -> Result<Vec<CompletionItem>> {
        let completions = Arc::new(Mutex::new(vec![]));
        crate::walk::walk(self, &|node: crate::walk::Node<'a>| {
            let mut findings = completions.lock().map_err(|_| anyhow::anyhow!("mutex"))?;
            match node {
                crate::walk::Node::TagDeclarator(tag) => {
                    findings.push(tag.into());
                }
                crate::walk::Node::VariableDeclaration(variable) => {
                    findings.extend::<Vec<CompletionItem>>(variable.into());
                }
                _ => {}
            }
            Ok(true)
        })?;
        let x = completions.lock().unwrap();
        Ok(x.clone())
    }

    /// Returns the body item that includes the given character position.
    pub fn get_body_item_for_position(&self, pos: usize) -> Option<&BodyItem> {
        for item in &self.body {
            let source_range: SourceRange = item.into();
            if source_range.contains(pos) {
                return Some(item);
            }
        }

        None
    }

    /// Returns the body item that includes the given character position.
    pub fn get_mut_body_item_for_position(&mut self, pos: usize) -> Option<&mut BodyItem> {
        for item in &mut self.body {
            let source_range: SourceRange = item.clone().into();
            if source_range.contains(pos) {
                return Some(item);
            }
        }

        None
    }

    /// Returns an Expr that includes the given character position.
    /// This is a bit more recursive than `get_body_item_for_position`.
    pub fn get_expr_for_position(&self, pos: usize) -> Option<&Expr> {
        let item = self.get_body_item_for_position(pos)?;

        // Recurse over the item.
        match item {
            BodyItem::ImportStatement(_) => None,
            BodyItem::ExpressionStatement(expression_statement) => Some(&expression_statement.expression),
            BodyItem::VariableDeclaration(variable_declaration) => variable_declaration.get_expr_for_position(pos),
            BodyItem::ReturnStatement(return_statement) => Some(&return_statement.argument),
        }
    }

    /// Returns a non code meta that includes the given character position.
    pub fn get_non_code_meta_for_position(&self, pos: usize) -> Option<&NonCodeMeta> {
        // Check if its in the body.
        if self.non_code_meta.contains(pos) {
            return Some(&self.non_code_meta);
        }
        let item = self.get_body_item_for_position(pos)?;

        // Recurse over the item.
        let expr = match item {
            BodyItem::ImportStatement(_) => None,
            BodyItem::ExpressionStatement(expression_statement) => Some(&expression_statement.expression),
            BodyItem::VariableDeclaration(variable_declaration) => variable_declaration.get_expr_for_position(pos),
            BodyItem::ReturnStatement(return_statement) => Some(&return_statement.argument),
        };

        // Check if the expr's non code meta contains the position.
        if let Some(expr) = expr {
            if let Some(non_code_meta) = expr.get_non_code_meta() {
                if non_code_meta.contains(pos) {
                    return Some(non_code_meta);
                }
            }
        }

        None
    }

    /// Returns all the lsp symbols in the program.
    pub fn get_lsp_symbols<'a>(&'a self, code: &str) -> Result<Vec<DocumentSymbol>> {
        let symbols = Arc::new(Mutex::new(vec![]));
        crate::walk::walk(self, &|node: crate::walk::Node<'a>| {
            let mut findings = symbols.lock().map_err(|_| anyhow::anyhow!("mutex"))?;
            match node {
                crate::walk::Node::TagDeclarator(tag) => {
                    findings.extend::<Vec<DocumentSymbol>>(tag.get_lsp_symbols(code));
                }
                crate::walk::Node::VariableDeclaration(variable) => {
                    findings.extend::<Vec<DocumentSymbol>>(variable.get_lsp_symbols(code));
                }
                _ => {}
            }
            Ok(true)
        })?;
        let x = symbols.lock().unwrap();
        Ok(x.clone())
    }

    // Return all the lsp folding ranges in the program.
    pub fn get_lsp_folding_ranges(&self) -> Vec<FoldingRange> {
        let mut ranges = vec![];
        // We only care about the top level things in the program.
        for item in &self.body {
            match item {
                BodyItem::ImportStatement(_) => continue,
                BodyItem::ExpressionStatement(expression_statement) => {
                    if let Some(folding_range) = expression_statement.expression.get_lsp_folding_range() {
                        ranges.push(folding_range)
                    }
                }
                BodyItem::VariableDeclaration(variable_declaration) => {
                    if let Some(folding_range) = variable_declaration.get_lsp_folding_range() {
                        ranges.push(folding_range)
                    }
                }
                BodyItem::ReturnStatement(_return_statement) => continue,
            }
        }

        ranges
    }

    /// Rename the variable declaration at the given position.
    pub fn rename_symbol(&mut self, new_name: &str, pos: usize) {
        // The position must be within the variable declaration.
        let mut old_name = None;
        for item in &mut self.body {
            match item {
                BodyItem::ImportStatement(stmt) => {
                    if let Some(var_old_name) = stmt.rename_symbol(new_name, pos) {
                        old_name = Some(var_old_name);
                        break;
                    }
                }
                BodyItem::ExpressionStatement(_expression_statement) => {
                    continue;
                }
                BodyItem::VariableDeclaration(ref mut variable_declaration) => {
                    if let Some(var_old_name) = variable_declaration.rename_symbol(new_name, pos) {
                        old_name = Some(var_old_name);
                        break;
                    }
                }
                BodyItem::ReturnStatement(_return_statement) => continue,
            }
        }

        if let Some(old_name) = old_name {
            // Now rename all the identifiers in the rest of the program.
            self.rename_identifiers(&old_name, new_name);
        } else {
            // Okay so this was not a top level variable declaration.
            // But it might be a variable declaration inside a function or function params.
            // So we need to check that.
            let Some(ref mut item) = self.get_mut_body_item_for_position(pos) else {
                return;
            };

            // Recurse over the item.
            let mut value = match item {
                BodyItem::ImportStatement(_) => None, // TODO
                BodyItem::ExpressionStatement(ref mut expression_statement) => {
                    Some(&mut expression_statement.expression)
                }
                BodyItem::VariableDeclaration(ref mut variable_declaration) => {
                    variable_declaration.get_mut_expr_for_position(pos)
                }
                BodyItem::ReturnStatement(ref mut return_statement) => Some(&mut return_statement.argument),
            };

            // Check if we have a function expression.
            if let Some(Expr::FunctionExpression(ref mut function_expression)) = &mut value {
                // Check if the params to the function expression contain the position.
                for param in &mut function_expression.params {
                    let param_source_range: SourceRange = (&param.identifier).into();
                    if param_source_range.contains(pos) {
                        let old_name = param.identifier.name.clone();
                        // Rename the param.
                        param.identifier.rename(&old_name, new_name);
                        // Now rename all the identifiers in the rest of the program.
                        function_expression.body.rename_identifiers(&old_name, new_name);
                        return;
                    }
                }
            }
        }
    }

    /// Rename all identifiers that have the old name to the new given name.
    fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        for item in &mut self.body {
            match item {
                BodyItem::ImportStatement(ref mut stmt) => {
                    stmt.rename_identifiers(old_name, new_name);
                }
                BodyItem::ExpressionStatement(ref mut expression_statement) => {
                    expression_statement.expression.rename_identifiers(old_name, new_name);
                }
                BodyItem::VariableDeclaration(ref mut variable_declaration) => {
                    variable_declaration.rename_identifiers(old_name, new_name);
                }
                BodyItem::ReturnStatement(ref mut return_statement) => {
                    return_statement.argument.rename_identifiers(old_name, new_name);
                }
            }
        }
    }

    /// Replace a variable declaration with the given name with a new one.
    pub fn replace_variable(&mut self, name: &str, declarator: VariableDeclarator) {
        for item in &mut self.body {
            match item {
                BodyItem::ImportStatement(_) => {
                    continue;
                }
                BodyItem::ExpressionStatement(_expression_statement) => {
                    continue;
                }
                BodyItem::VariableDeclaration(ref mut variable_declaration) => {
                    for declaration in &mut variable_declaration.declarations {
                        if declaration.id.name == name {
                            *declaration = declarator;
                            return;
                        }
                    }
                }
                BodyItem::ReturnStatement(_return_statement) => continue,
            }
        }
    }

    /// Replace a value with the new value, use the source range for matching the exact value.
    pub fn replace_value(&mut self, source_range: SourceRange, new_value: Expr) {
        for item in &mut self.body {
            match item {
                BodyItem::ImportStatement(_) => {} // TODO
                BodyItem::ExpressionStatement(ref mut expression_statement) => expression_statement
                    .expression
                    .replace_value(source_range, new_value.clone()),
                BodyItem::VariableDeclaration(ref mut variable_declaration) => {
                    variable_declaration.replace_value(source_range, new_value.clone())
                }
                BodyItem::ReturnStatement(ref mut return_statement) => {
                    return_statement.argument.replace_value(source_range, new_value.clone())
                }
            }
        }
    }

    /// Get the variable declaration with the given name.
    pub fn get_variable(&self, name: &str) -> Option<Definition<'_>> {
        for item in &self.body {
            match item {
                BodyItem::ImportStatement(stmt) => {
                    for import_item in &stmt.items {
                        if import_item.identifier() == name {
                            return Some(Definition::Import(stmt.as_ref()));
                        }
                    }
                }
                BodyItem::ExpressionStatement(_expression_statement) => {
                    continue;
                }
                BodyItem::VariableDeclaration(variable_declaration) => {
                    for declaration in &variable_declaration.declarations {
                        if declaration.id.name == name {
                            return Some(Definition::Variable(declaration));
                        }
                    }
                }
                BodyItem::ReturnStatement(_return_statement) => continue,
            }
        }

        None
    }
}

pub trait ValueMeta {
    fn start(&self) -> usize;

    fn end(&self) -> usize;
}

macro_rules! impl_value_meta {
    {$name:ident} => {
        impl crate::ast::types::ValueMeta for $name {
            fn start(&self) -> usize {
                self.start
            }

            fn end(&self) -> usize {
                self.end
            }
        }

        impl From<$name> for crate::executor::SourceRange {
            fn from(v: $name) -> Self {
                Self([v.start, v.end])
            }
        }

        impl From<&$name> for crate::executor::SourceRange {
            fn from(v: &$name) -> Self {
                Self([v.start, v.end])
            }
        }

        impl From<&Box<$name>> for crate::executor::SourceRange {
            fn from(v: &Box<$name>) -> Self {
                Self([v.start, v.end])
            }
        }
    };
}

pub(crate) use impl_value_meta;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub enum BodyItem {
    ImportStatement(Box<ImportStatement>),
    ExpressionStatement(ExpressionStatement),
    VariableDeclaration(Box<VariableDeclaration>),
    ReturnStatement(ReturnStatement),
}

impl BodyItem {
    pub fn start(&self) -> usize {
        match self {
            BodyItem::ImportStatement(stmt) => stmt.start(),
            BodyItem::ExpressionStatement(expression_statement) => expression_statement.start(),
            BodyItem::VariableDeclaration(variable_declaration) => variable_declaration.start(),
            BodyItem::ReturnStatement(return_statement) => return_statement.start(),
        }
    }

    pub fn end(&self) -> usize {
        match self {
            BodyItem::ImportStatement(stmt) => stmt.end(),
            BodyItem::ExpressionStatement(expression_statement) => expression_statement.end(),
            BodyItem::VariableDeclaration(variable_declaration) => variable_declaration.end(),
            BodyItem::ReturnStatement(return_statement) => return_statement.end(),
        }
    }
}

impl From<BodyItem> for SourceRange {
    fn from(item: BodyItem) -> Self {
        Self([item.start(), item.end()])
    }
}

impl From<&BodyItem> for SourceRange {
    fn from(item: &BodyItem) -> Self {
        Self([item.start(), item.end()])
    }
}

/// An expression can be evaluated to yield a single KCL value.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub enum Expr {
    Literal(Box<Literal>),
    Identifier(Box<Identifier>),
    TagDeclarator(Box<TagDeclarator>),
    BinaryExpression(Box<BinaryExpression>),
    FunctionExpression(Box<FunctionExpression>),
    CallExpression(Box<CallExpression>),
    PipeExpression(Box<PipeExpression>),
    PipeSubstitution(Box<PipeSubstitution>),
    ArrayExpression(Box<ArrayExpression>),
    ArrayRangeExpression(Box<ArrayRangeExpression>),
    ObjectExpression(Box<ObjectExpression>),
    MemberExpression(Box<MemberExpression>),
    UnaryExpression(Box<UnaryExpression>),
    IfExpression(Box<IfExpression>),
    None(KclNone),
}

impl Expr {
    pub fn get_lsp_folding_range(&self) -> Option<FoldingRange> {
        let recasted = self.recast(&FormatOptions::default(), 0, false);
        // If the code only has one line then we don't need to fold it.
        if recasted.lines().count() <= 1 {
            return None;
        }

        // This unwrap is safe because we know that the code has at least one line.
        let first_line = recasted.lines().next().unwrap().to_string();

        Some(FoldingRange {
            start_line: (self.start() + first_line.len()) as u32,
            start_character: None,
            end_line: self.end() as u32,
            end_character: None,
            kind: Some(FoldingRangeKind::Region),
            collapsed_text: Some(first_line),
        })
    }

    // Get the non code meta for the value.
    pub fn get_non_code_meta(&self) -> Option<&NonCodeMeta> {
        match self {
            Expr::BinaryExpression(_bin_exp) => None,
            Expr::ArrayExpression(_array_exp) => None,
            Expr::ArrayRangeExpression(_array_exp) => None,
            Expr::ObjectExpression(_obj_exp) => None,
            Expr::MemberExpression(_mem_exp) => None,
            Expr::Literal(_literal) => None,
            Expr::FunctionExpression(_func_exp) => None,
            Expr::CallExpression(_call_exp) => None,
            Expr::Identifier(_ident) => None,
            Expr::TagDeclarator(_tag) => None,
            Expr::PipeExpression(pipe_exp) => Some(&pipe_exp.non_code_meta),
            Expr::UnaryExpression(_unary_exp) => None,
            Expr::PipeSubstitution(_pipe_substitution) => None,
            Expr::IfExpression(_) => None,
            Expr::None(_none) => None,
        }
    }

    pub fn replace_value(&mut self, source_range: SourceRange, new_value: Expr) {
        if source_range == self.clone().into() {
            *self = new_value;
            return;
        }

        match self {
            Expr::BinaryExpression(ref mut bin_exp) => bin_exp.replace_value(source_range, new_value),
            Expr::ArrayExpression(ref mut array_exp) => array_exp.replace_value(source_range, new_value),
            Expr::ArrayRangeExpression(ref mut array_range) => array_range.replace_value(source_range, new_value),
            Expr::ObjectExpression(ref mut obj_exp) => obj_exp.replace_value(source_range, new_value),
            Expr::MemberExpression(_) => {}
            Expr::Literal(_) => {}
            Expr::FunctionExpression(ref mut func_exp) => func_exp.replace_value(source_range, new_value),
            Expr::CallExpression(ref mut call_exp) => call_exp.replace_value(source_range, new_value),
            Expr::Identifier(_) => {}
            Expr::TagDeclarator(_) => {}
            Expr::PipeExpression(ref mut pipe_exp) => pipe_exp.replace_value(source_range, new_value),
            Expr::UnaryExpression(ref mut unary_exp) => unary_exp.replace_value(source_range, new_value),
            Expr::IfExpression(_) => {}
            Expr::PipeSubstitution(_) => {}
            Expr::None(_) => {}
        }
    }

    pub fn start(&self) -> usize {
        match self {
            Expr::Literal(literal) => literal.start(),
            Expr::Identifier(identifier) => identifier.start(),
            Expr::TagDeclarator(tag) => tag.start(),
            Expr::BinaryExpression(binary_expression) => binary_expression.start(),
            Expr::FunctionExpression(function_expression) => function_expression.start(),
            Expr::CallExpression(call_expression) => call_expression.start(),
            Expr::PipeExpression(pipe_expression) => pipe_expression.start(),
            Expr::PipeSubstitution(pipe_substitution) => pipe_substitution.start(),
            Expr::ArrayExpression(array_expression) => array_expression.start(),
            Expr::ArrayRangeExpression(array_range) => array_range.start(),
            Expr::ObjectExpression(object_expression) => object_expression.start(),
            Expr::MemberExpression(member_expression) => member_expression.start(),
            Expr::UnaryExpression(unary_expression) => unary_expression.start(),
            Expr::IfExpression(expr) => expr.start(),
            Expr::None(none) => none.start,
        }
    }

    pub fn end(&self) -> usize {
        match self {
            Expr::Literal(literal) => literal.end(),
            Expr::Identifier(identifier) => identifier.end(),
            Expr::TagDeclarator(tag) => tag.end(),
            Expr::BinaryExpression(binary_expression) => binary_expression.end(),
            Expr::FunctionExpression(function_expression) => function_expression.end(),
            Expr::CallExpression(call_expression) => call_expression.end(),
            Expr::PipeExpression(pipe_expression) => pipe_expression.end(),
            Expr::PipeSubstitution(pipe_substitution) => pipe_substitution.end(),
            Expr::ArrayExpression(array_expression) => array_expression.end(),
            Expr::ArrayRangeExpression(array_range) => array_range.end(),
            Expr::ObjectExpression(object_expression) => object_expression.end(),
            Expr::MemberExpression(member_expression) => member_expression.end(),
            Expr::UnaryExpression(unary_expression) => unary_expression.end(),
            Expr::IfExpression(expr) => expr.end(),
            Expr::None(none) => none.end,
        }
    }

    /// Returns a hover value that includes the given character position.
    /// This is really recursive so keep that in mind.
    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        match self {
            Expr::BinaryExpression(binary_expression) => binary_expression.get_hover_value_for_position(pos, code),
            Expr::FunctionExpression(function_expression) => {
                function_expression.get_hover_value_for_position(pos, code)
            }
            Expr::CallExpression(call_expression) => call_expression.get_hover_value_for_position(pos, code),
            Expr::PipeExpression(pipe_expression) => pipe_expression.get_hover_value_for_position(pos, code),
            Expr::ArrayExpression(array_expression) => array_expression.get_hover_value_for_position(pos, code),
            Expr::ArrayRangeExpression(array_range) => array_range.get_hover_value_for_position(pos, code),
            Expr::ObjectExpression(object_expression) => object_expression.get_hover_value_for_position(pos, code),
            Expr::MemberExpression(member_expression) => member_expression.get_hover_value_for_position(pos, code),
            Expr::UnaryExpression(unary_expression) => unary_expression.get_hover_value_for_position(pos, code),
            Expr::IfExpression(expr) => expr.get_hover_value_for_position(pos, code),
            // TODO: LSP hover information for values/types. https://github.com/KittyCAD/modeling-app/issues/1126
            Expr::None(_) => None,
            Expr::Literal(_) => None,
            Expr::Identifier(_) => None,
            Expr::TagDeclarator(_) => None,
            // TODO: LSP hover information for symbols. https://github.com/KittyCAD/modeling-app/issues/1127
            Expr::PipeSubstitution(_) => None,
        }
    }

    /// Rename all identifiers that have the old name to the new given name.
    fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        match self {
            Expr::Literal(_literal) => {}
            Expr::Identifier(ref mut identifier) => identifier.rename(old_name, new_name),
            Expr::TagDeclarator(ref mut tag) => tag.rename(old_name, new_name),
            Expr::BinaryExpression(ref mut binary_expression) => {
                binary_expression.rename_identifiers(old_name, new_name)
            }
            Expr::FunctionExpression(_function_identifier) => {}
            Expr::CallExpression(ref mut call_expression) => call_expression.rename_identifiers(old_name, new_name),
            Expr::PipeExpression(ref mut pipe_expression) => pipe_expression.rename_identifiers(old_name, new_name),
            Expr::PipeSubstitution(_) => {}
            Expr::ArrayExpression(ref mut array_expression) => array_expression.rename_identifiers(old_name, new_name),
            Expr::ArrayRangeExpression(ref mut array_range) => array_range.rename_identifiers(old_name, new_name),
            Expr::ObjectExpression(ref mut object_expression) => {
                object_expression.rename_identifiers(old_name, new_name)
            }
            Expr::MemberExpression(ref mut member_expression) => {
                member_expression.rename_identifiers(old_name, new_name)
            }
            Expr::UnaryExpression(ref mut unary_expression) => unary_expression.rename_identifiers(old_name, new_name),
            Expr::IfExpression(ref mut expr) => expr.rename_identifiers(old_name, new_name),
            Expr::None(_) => {}
        }
    }

    /// Get the constraint level for an expression.
    pub fn get_constraint_level(&self) -> ConstraintLevel {
        match self {
            Expr::Literal(literal) => literal.get_constraint_level(),
            Expr::Identifier(identifier) => identifier.get_constraint_level(),
            Expr::TagDeclarator(tag) => tag.get_constraint_level(),
            Expr::BinaryExpression(binary_expression) => binary_expression.get_constraint_level(),

            Expr::FunctionExpression(function_identifier) => function_identifier.get_constraint_level(),
            Expr::CallExpression(call_expression) => call_expression.get_constraint_level(),
            Expr::PipeExpression(pipe_expression) => pipe_expression.get_constraint_level(),
            Expr::PipeSubstitution(pipe_substitution) => ConstraintLevel::Ignore {
                source_ranges: vec![pipe_substitution.into()],
            },
            Expr::ArrayExpression(array_expression) => array_expression.get_constraint_level(),
            Expr::ArrayRangeExpression(array_range) => array_range.get_constraint_level(),
            Expr::ObjectExpression(object_expression) => object_expression.get_constraint_level(),
            Expr::MemberExpression(member_expression) => member_expression.get_constraint_level(),
            Expr::UnaryExpression(unary_expression) => unary_expression.get_constraint_level(),
            Expr::IfExpression(expr) => expr.get_constraint_level(),
            Expr::None(none) => none.get_constraint_level(),
        }
    }
}

impl From<Expr> for SourceRange {
    fn from(value: Expr) -> Self {
        Self([value.start(), value.end()])
    }
}

impl From<&Expr> for SourceRange {
    fn from(value: &Expr) -> Self {
        Self([value.start(), value.end()])
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub enum BinaryPart {
    Literal(Box<Literal>),
    Identifier(Box<Identifier>),
    BinaryExpression(Box<BinaryExpression>),
    CallExpression(Box<CallExpression>),
    UnaryExpression(Box<UnaryExpression>),
    MemberExpression(Box<MemberExpression>),
    IfExpression(Box<IfExpression>),
}

impl From<BinaryPart> for SourceRange {
    fn from(value: BinaryPart) -> Self {
        Self([value.start(), value.end()])
    }
}

impl From<&BinaryPart> for SourceRange {
    fn from(value: &BinaryPart) -> Self {
        Self([value.start(), value.end()])
    }
}

impl BinaryPart {
    /// Get the constraint level.
    pub fn get_constraint_level(&self) -> ConstraintLevel {
        match self {
            BinaryPart::Literal(literal) => literal.get_constraint_level(),
            BinaryPart::Identifier(identifier) => identifier.get_constraint_level(),
            BinaryPart::BinaryExpression(binary_expression) => binary_expression.get_constraint_level(),
            BinaryPart::CallExpression(call_expression) => call_expression.get_constraint_level(),
            BinaryPart::UnaryExpression(unary_expression) => unary_expression.get_constraint_level(),
            BinaryPart::MemberExpression(member_expression) => member_expression.get_constraint_level(),
            BinaryPart::IfExpression(e) => e.get_constraint_level(),
        }
    }

    pub fn replace_value(&mut self, source_range: SourceRange, new_value: Expr) {
        match self {
            BinaryPart::Literal(_) => {}
            BinaryPart::Identifier(_) => {}
            BinaryPart::BinaryExpression(ref mut binary_expression) => {
                binary_expression.replace_value(source_range, new_value)
            }
            BinaryPart::CallExpression(ref mut call_expression) => {
                call_expression.replace_value(source_range, new_value)
            }
            BinaryPart::UnaryExpression(ref mut unary_expression) => {
                unary_expression.replace_value(source_range, new_value)
            }
            BinaryPart::MemberExpression(_) => {}
            BinaryPart::IfExpression(e) => e.replace_value(source_range, new_value),
        }
    }

    pub fn start(&self) -> usize {
        match self {
            BinaryPart::Literal(literal) => literal.start(),
            BinaryPart::Identifier(identifier) => identifier.start(),
            BinaryPart::BinaryExpression(binary_expression) => binary_expression.start(),
            BinaryPart::CallExpression(call_expression) => call_expression.start(),
            BinaryPart::UnaryExpression(unary_expression) => unary_expression.start(),
            BinaryPart::MemberExpression(member_expression) => member_expression.start(),
            BinaryPart::IfExpression(e) => e.start(),
        }
    }

    pub fn end(&self) -> usize {
        match self {
            BinaryPart::Literal(literal) => literal.end(),
            BinaryPart::Identifier(identifier) => identifier.end(),
            BinaryPart::BinaryExpression(binary_expression) => binary_expression.end(),
            BinaryPart::CallExpression(call_expression) => call_expression.end(),
            BinaryPart::UnaryExpression(unary_expression) => unary_expression.end(),
            BinaryPart::MemberExpression(member_expression) => member_expression.end(),
            BinaryPart::IfExpression(e) => e.end(),
        }
    }

    /// Returns a hover value that includes the given character position.
    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        match self {
            BinaryPart::Literal(_literal) => None,
            BinaryPart::Identifier(_identifier) => None,
            BinaryPart::BinaryExpression(binary_expression) => {
                binary_expression.get_hover_value_for_position(pos, code)
            }
            BinaryPart::CallExpression(call_expression) => call_expression.get_hover_value_for_position(pos, code),
            BinaryPart::UnaryExpression(unary_expression) => unary_expression.get_hover_value_for_position(pos, code),
            BinaryPart::IfExpression(e) => e.get_hover_value_for_position(pos, code),
            BinaryPart::MemberExpression(member_expression) => {
                member_expression.get_hover_value_for_position(pos, code)
            }
        }
    }

    /// Rename all identifiers that have the old name to the new given name.
    fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        match self {
            BinaryPart::Literal(_literal) => {}
            BinaryPart::Identifier(ref mut identifier) => identifier.rename(old_name, new_name),
            BinaryPart::BinaryExpression(ref mut binary_expression) => {
                binary_expression.rename_identifiers(old_name, new_name)
            }
            BinaryPart::CallExpression(ref mut call_expression) => {
                call_expression.rename_identifiers(old_name, new_name)
            }
            BinaryPart::UnaryExpression(ref mut unary_expression) => {
                unary_expression.rename_identifiers(old_name, new_name)
            }
            BinaryPart::MemberExpression(ref mut member_expression) => {
                member_expression.rename_identifiers(old_name, new_name)
            }
            BinaryPart::IfExpression(ref mut if_expression) => if_expression.rename_identifiers(old_name, new_name),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct NonCodeNode {
    pub start: usize,
    pub end: usize,
    pub value: NonCodeValue,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl From<NonCodeNode> for SourceRange {
    fn from(value: NonCodeNode) -> Self {
        Self([value.start, value.end])
    }
}

impl From<&NonCodeNode> for SourceRange {
    fn from(value: &NonCodeNode) -> Self {
        Self([value.start, value.end])
    }
}

impl NonCodeNode {
    pub fn contains(&self, pos: usize) -> bool {
        self.start <= pos && pos <= self.end
    }

    pub fn value(&self) -> String {
        match &self.value {
            NonCodeValue::Shebang { value } => value.clone(),
            NonCodeValue::InlineComment { value, style: _ } => value.clone(),
            NonCodeValue::BlockComment { value, style: _ } => value.clone(),
            NonCodeValue::NewLineBlockComment { value, style: _ } => value.clone(),
            NonCodeValue::NewLine => "\n\n".to_string(),
        }
    }

    pub fn format(&self, indentation: &str) -> String {
        match &self.value {
            NonCodeValue::Shebang { value } => format!("{}\n\n", value),
            NonCodeValue::InlineComment {
                value,
                style: CommentStyle::Line,
            } => format!(" // {}\n", value),
            NonCodeValue::InlineComment {
                value,
                style: CommentStyle::Block,
            } => format!(" /* {} */", value),
            NonCodeValue::BlockComment { value, style } => match style {
                CommentStyle::Block => format!("{}/* {} */", indentation, value),
                CommentStyle::Line => {
                    if value.trim().is_empty() {
                        format!("{}//\n", indentation)
                    } else {
                        format!("{}// {}\n", indentation, value.trim())
                    }
                }
            },
            NonCodeValue::NewLineBlockComment { value, style } => {
                let add_start_new_line = if self.start == 0 { "" } else { "\n\n" };
                match style {
                    CommentStyle::Block => format!("{}{}/* {} */\n", add_start_new_line, indentation, value),
                    CommentStyle::Line => {
                        if value.trim().is_empty() {
                            format!("{}{}//\n", add_start_new_line, indentation)
                        } else {
                            format!("{}{}// {}\n", add_start_new_line, indentation, value.trim())
                        }
                    }
                }
            }
            NonCodeValue::NewLine => "\n\n".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub enum CommentStyle {
    /// Like // foo
    Line,
    /// Like /* foo */
    Block,
}

impl CommentStyle {
    fn digestable_id(&self) -> [u8; 2] {
        match &self {
            CommentStyle::Line => *b"//",
            CommentStyle::Block => *b"/*",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum NonCodeValue {
    /// A shebang.
    /// This is a special type of comment that is at the top of the file.
    /// It looks like this:
    /// ```python,no_run
    /// #!/usr/bin/env python
    /// ```
    Shebang {
        value: String,
    },
    /// An inline comment.
    /// Here are examples:
    /// `1 + 1 // This is an inline comment`.
    /// `1 + 1 /* Here's another */`.
    InlineComment {
        value: String,
        style: CommentStyle,
    },
    /// A block comment.
    /// An example of this is the following:
    /// ```python,no_run
    /// /* This is a
    ///     block comment */
    /// 1 + 1
    /// ```
    /// Now this is important. The block comment is attached to the next line.
    /// This is always the case. Also the block comment doesn't have a new line above it.
    /// If it did it would be a `NewLineBlockComment`.
    BlockComment {
        value: String,
        style: CommentStyle,
    },
    /// A block comment that has a new line above it.
    /// The user explicitly added a new line above the block comment.
    NewLineBlockComment {
        value: String,
        style: CommentStyle,
    },
    // A new line like `\n\n` NOT a new line like `\n`.
    // This is also not a comment.
    NewLine,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct NonCodeMeta {
    pub non_code_nodes: HashMap<usize, Vec<NonCodeNode>>,
    pub start: Vec<NonCodeNode>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl NonCodeMeta {
    /// Does this contain anything?
    pub fn is_empty(&self) -> bool {
        self.non_code_nodes.is_empty() && self.start.is_empty()
    }

    /// How many non-code values does this have?
    pub fn non_code_nodes_len(&self) -> usize {
        self.non_code_nodes.values().map(|x| x.len()).sum()
    }
}

// implement Deserialize manually because we to force the keys of non_code_nodes to be usize
// and by default the ts type { [statementIndex: number]: NonCodeNode } serializes to a string i.e. "0", "1", etc.
impl<'de> Deserialize<'de> for NonCodeMeta {
    fn deserialize<D>(deserializer: D) -> Result<NonCodeMeta, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct NonCodeMetaHelper {
            non_code_nodes: HashMap<String, Vec<NonCodeNode>>,
            start: Vec<NonCodeNode>,
        }

        let helper = NonCodeMetaHelper::deserialize(deserializer)?;
        let non_code_nodes = helper
            .non_code_nodes
            .into_iter()
            .map(|(key, value)| Ok((key.parse().map_err(serde::de::Error::custom)?, value)))
            .collect::<Result<HashMap<_, _>, _>>()?;
        Ok(NonCodeMeta {
            non_code_nodes,
            start: helper.start,
            digest: None,
        })
    }
}

impl NonCodeMeta {
    pub fn insert(&mut self, i: usize, new: NonCodeNode) {
        self.non_code_nodes.entry(i).or_default().push(new);
    }

    pub fn contains(&self, pos: usize) -> bool {
        if self.start.iter().any(|node| node.contains(pos)) {
            return true;
        }

        self.non_code_nodes
            .iter()
            .any(|(_, nodes)| nodes.iter().any(|node| node.contains(pos)))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct ImportItem {
    /// Name of the item to import.
    pub name: Identifier,
    /// Rename the item using an identifier after "as".
    pub alias: Option<Identifier>,

    pub start: usize,
    pub end: usize,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(ImportItem);

impl ImportItem {
    pub fn identifier(&self) -> &str {
        match &self.alias {
            Some(alias) => &alias.name,
            None => &self.name.name,
        }
    }

    pub fn rename_symbol(&mut self, new_name: &str, pos: usize) -> Option<String> {
        match &mut self.alias {
            Some(alias) => {
                let alias_source_range = SourceRange::from(&*alias);
                if !alias_source_range.contains(pos) {
                    return None;
                }
                let old_name = std::mem::replace(&mut alias.name, new_name.to_owned());
                Some(old_name)
            }
            None => {
                let use_source_range = SourceRange::from(&*self);
                if use_source_range.contains(pos) {
                    self.alias = Some(Identifier::new(new_name));
                }
                // Return implicit name.
                return Some(self.identifier().to_owned());
            }
        }
    }

    pub fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        if let Some(alias) = &mut self.alias {
            alias.rename(old_name, new_name);
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct ImportStatement {
    pub start: usize,
    pub end: usize,
    pub items: Vec<ImportItem>,
    pub path: String,
    pub raw_path: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(ImportStatement);

impl ImportStatement {
    pub fn get_constraint_level(&self) -> ConstraintLevel {
        ConstraintLevel::Full {
            source_ranges: vec![self.into()],
        }
    }

    pub fn rename_symbol(&mut self, new_name: &str, pos: usize) -> Option<String> {
        for item in &mut self.items {
            let source_range = SourceRange::from(&*item);
            if source_range.contains(pos) {
                let old_name = item.rename_symbol(new_name, pos);
                if old_name.is_some() {
                    return old_name;
                }
            }
        }
        None
    }

    pub fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        for item in &mut self.items {
            item.rename_identifiers(old_name, new_name);
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct ExpressionStatement {
    pub start: usize,
    pub end: usize,
    pub expression: Expr,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(ExpressionStatement);

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct CallExpression {
    pub start: usize,
    pub end: usize,
    pub callee: Identifier,
    pub arguments: Vec<Expr>,
    pub optional: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(CallExpression);

impl From<CallExpression> for Expr {
    fn from(call_expression: CallExpression) -> Self {
        Expr::CallExpression(Box::new(call_expression))
    }
}

impl CallExpression {
    pub fn new(name: &str, arguments: Vec<Expr>) -> Result<Self, KclError> {
        Ok(Self {
            start: 0,
            end: 0,
            callee: Identifier::new(name),
            arguments,
            optional: false,
            digest: None,
        })
    }

    /// Is at least one argument the '%' i.e. the substitution operator?
    pub fn has_substitution_arg(&self) -> bool {
        self.arguments
            .iter()
            .any(|arg| matches!(arg, Expr::PipeSubstitution(_)))
    }

    pub fn as_source_ranges(&self) -> Vec<SourceRange> {
        vec![SourceRange([self.start, self.end])]
    }

    pub fn replace_value(&mut self, source_range: SourceRange, new_value: Expr) {
        for arg in &mut self.arguments {
            arg.replace_value(source_range, new_value.clone());
        }
    }

    /// Returns a hover value that includes the given character position.
    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        let callee_source_range: SourceRange = self.callee.clone().into();
        if callee_source_range.contains(pos) {
            return Some(Hover::Function {
                name: self.callee.name.clone(),
                range: callee_source_range.to_lsp_range(code),
            });
        }

        for (index, arg) in self.arguments.iter().enumerate() {
            let source_range: SourceRange = arg.into();
            if source_range.contains(pos) {
                return Some(Hover::Signature {
                    name: self.callee.name.clone(),
                    parameter_index: index as u32,
                    range: source_range.to_lsp_range(code),
                });
            }
        }

        None
    }

    /// Rename all identifiers that have the old name to the new given name.
    fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        self.callee.rename(old_name, new_name);

        for arg in &mut self.arguments {
            arg.rename_identifiers(old_name, new_name);
        }
    }

    /// Return the constraint level for this call expression.
    pub fn get_constraint_level(&self) -> ConstraintLevel {
        if self.arguments.is_empty() {
            return ConstraintLevel::Ignore {
                source_ranges: vec![self.into()],
            };
        }

        // Iterate over the arguments and get the constraint level for each one.
        let mut constraint_levels = ConstraintLevels::new();
        for arg in &self.arguments {
            constraint_levels.push(arg.get_constraint_level());
        }

        constraint_levels.get_constraint_level(self.into())
    }
}

/// A function declaration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ts_rs::TS, JsonSchema)]
#[ts(export)]
#[serde(tag = "type")]
pub enum Function {
    /// A stdlib function written in Rust (aka core lib).
    StdLib {
        /// The function.
        func: Box<dyn StdLibFn>,
    },
    /// A stdlib function written in KCL.
    StdLibKcl {
        /// The function.
        func: Box<dyn KclStdLibFn>,
    },
    /// A function that is defined in memory.
    #[default]
    InMemory,
}

impl PartialEq for Function {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Function::StdLib { func: func1 }, Function::StdLib { func: func2 }) => func1.name() == func2.name(),
            (Function::InMemory, Function::InMemory) => true,
            _ => false,
        }
    }
}

#[derive(
    Debug, Default, Clone, Copy, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, FromStr, Display, Bake,
)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
#[display(style = "snake_case")]
pub enum ItemVisibility {
    #[default]
    Default,
    Export,
}

impl ItemVisibility {
    fn digestable_id(&self) -> [u8; 1] {
        match self {
            ItemVisibility::Default => [0],
            ItemVisibility::Export => [1],
        }
    }

    fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct VariableDeclaration {
    pub start: usize,
    pub end: usize,
    pub declarations: Vec<VariableDeclarator>,
    #[serde(default, skip_serializing_if = "ItemVisibility::is_default")]
    pub visibility: ItemVisibility,
    pub kind: VariableKind, // Change to enum if there are specific values

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl From<&VariableDeclaration> for Vec<CompletionItem> {
    fn from(declaration: &VariableDeclaration) -> Self {
        let mut completions = vec![];
        for variable in &declaration.declarations {
            completions.push(CompletionItem {
                label: variable.id.name.to_string(),
                label_details: None,
                kind: Some(match declaration.kind {
                    VariableKind::Const => CompletionItemKind::CONSTANT,
                    VariableKind::Fn => CompletionItemKind::FUNCTION,
                }),
                detail: Some(declaration.kind.to_string()),
                documentation: None,
                deprecated: None,
                preselect: None,
                sort_text: None,
                filter_text: None,
                insert_text: None,
                insert_text_format: None,
                insert_text_mode: None,
                text_edit: None,
                additional_text_edits: None,
                command: None,
                commit_characters: None,
                data: None,
                tags: None,
            })
        }
        completions
    }
}

impl_value_meta!(VariableDeclaration);

impl VariableDeclaration {
    pub fn new(declarations: Vec<VariableDeclarator>, visibility: ItemVisibility, kind: VariableKind) -> Self {
        Self {
            start: 0,
            end: 0,
            declarations,
            visibility,
            kind,
            digest: None,
        }
    }
    pub fn get_lsp_folding_range(&self) -> Option<FoldingRange> {
        let recasted = self.recast(&FormatOptions::default(), 0);
        // If the recasted value only has one line, don't fold it.
        if recasted.lines().count() <= 1 {
            return None;
        }

        // This unwrap is safe because we know that the code has at least one line.
        let first_line = recasted.lines().next().unwrap().to_string();

        Some(FoldingRange {
            start_line: (self.start() + first_line.len()) as u32,
            start_character: None,
            end_line: self.end() as u32,
            end_character: None,
            kind: Some(FoldingRangeKind::Region),
            collapsed_text: Some(first_line),
        })
    }

    pub fn replace_value(&mut self, source_range: SourceRange, new_value: Expr) {
        for declaration in &mut self.declarations {
            declaration.init.replace_value(source_range, new_value.clone());
        }
    }

    /// Returns an Expr that includes the given character position.
    pub fn get_expr_for_position(&self, pos: usize) -> Option<&Expr> {
        for declaration in &self.declarations {
            let source_range: SourceRange = declaration.into();
            if source_range.contains(pos) {
                return Some(&declaration.init);
            }
        }

        None
    }

    /// Returns an Expr that includes the given character position.
    pub fn get_mut_expr_for_position(&mut self, pos: usize) -> Option<&mut Expr> {
        for declaration in &mut self.declarations {
            let source_range: SourceRange = declaration.clone().into();
            if source_range.contains(pos) {
                return Some(&mut declaration.init);
            }
        }

        None
    }

    /// Rename the variable declaration at the given position.
    /// This returns the old name of the variable, if it found one.
    pub fn rename_symbol(&mut self, new_name: &str, pos: usize) -> Option<String> {
        // The position must be within the variable declaration.
        let source_range: SourceRange = self.clone().into();
        if !source_range.contains(pos) {
            return None;
        }

        for declaration in &mut self.declarations {
            let declaration_source_range: SourceRange = declaration.id.clone().into();
            if declaration_source_range.contains(pos) {
                let old_name = declaration.id.name.clone();
                declaration.id.name = new_name.to_string();
                return Some(old_name);
            }
        }

        None
    }

    pub fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        for declaration in &mut self.declarations {
            // Skip the init for the variable with the new name since it is the one we are renaming.
            if declaration.id.name == new_name {
                continue;
            }

            declaration.init.rename_identifiers(old_name, new_name);
        }
    }

    pub fn get_lsp_symbols(&self, code: &str) -> Vec<DocumentSymbol> {
        let mut symbols = vec![];

        for declaration in &self.declarations {
            let source_range: SourceRange = declaration.into();
            let inner_source_range: SourceRange = declaration.id.clone().into();

            let mut symbol_kind = match self.kind {
                VariableKind::Fn => SymbolKind::FUNCTION,
                VariableKind::Const => SymbolKind::CONSTANT,
            };

            let children = match &declaration.init {
                Expr::FunctionExpression(function_expression) => {
                    symbol_kind = SymbolKind::FUNCTION;
                    let mut children = vec![];
                    for param in &function_expression.params {
                        let param_source_range: SourceRange = (&param.identifier).into();
                        #[allow(deprecated)]
                        children.push(DocumentSymbol {
                            name: param.identifier.name.clone(),
                            detail: None,
                            kind: SymbolKind::CONSTANT,
                            range: param_source_range.to_lsp_range(code),
                            selection_range: param_source_range.to_lsp_range(code),
                            children: None,
                            tags: None,
                            deprecated: None,
                        });
                    }
                    children
                }
                Expr::ObjectExpression(object_expression) => {
                    symbol_kind = SymbolKind::OBJECT;
                    let mut children = vec![];
                    for property in &object_expression.properties {
                        children.extend(property.get_lsp_symbols(code));
                    }
                    children
                }
                Expr::ArrayExpression(_) => {
                    symbol_kind = SymbolKind::ARRAY;
                    vec![]
                }
                _ => vec![],
            };

            #[allow(deprecated)]
            symbols.push(DocumentSymbol {
                name: declaration.id.name.clone(),
                detail: Some(self.kind.to_string()),
                kind: symbol_kind,
                range: source_range.to_lsp_range(code),
                selection_range: inner_source_range.to_lsp_range(code),
                children: Some(children),
                tags: None,
                deprecated: None,
            });
        }

        symbols
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, FromStr, Display, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
#[display(style = "snake_case")]
pub enum VariableKind {
    /// Declare a named constant.
    Const,
    /// Declare a function.
    Fn,
}

impl VariableKind {
    fn digestable_id(&self) -> [u8; 1] {
        match self {
            VariableKind::Const => [2],
            VariableKind::Fn => [3],
        }
    }

    pub fn to_completion_items() -> Result<Vec<CompletionItem>> {
        let mut settings = schemars::gen::SchemaSettings::openapi3();
        settings.inline_subschemas = true;
        let mut generator = schemars::gen::SchemaGenerator::new(settings);
        let schema = VariableKind::json_schema(&mut generator);
        let schemars::schema::Schema::Object(o) = &schema else {
            anyhow::bail!("expected object schema: {:#?}", schema);
        };
        let Some(subschemas) = &o.subschemas else {
            anyhow::bail!("expected subschemas: {:#?}", schema);
        };
        let Some(one_ofs) = &subschemas.one_of else {
            anyhow::bail!("expected one_of: {:#?}", schema);
        };

        // Iterate over all the VariableKinds and create a completion for each.
        let mut completions = vec![];
        for one_of in one_ofs {
            completions.push(crate::docs::completion_item_from_enum_schema(
                one_of,
                CompletionItemKind::KEYWORD,
            )?);
        }

        Ok(completions)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct VariableDeclarator {
    pub start: usize,
    pub end: usize,
    /// The identifier of the variable.
    pub id: Identifier,
    /// The value of the variable.
    pub init: Expr,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(VariableDeclarator);

impl VariableDeclarator {
    pub fn new(name: &str, init: Expr) -> Self {
        Self {
            start: 0,
            end: 0,
            id: Identifier::new(name),
            init,
            digest: None,
        }
    }

    pub fn get_constraint_level(&self) -> ConstraintLevel {
        self.init.get_constraint_level()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct Literal {
    pub start: usize,
    pub end: usize,
    pub value: LiteralValue,
    pub raw: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(Literal);

impl Literal {
    pub fn new(value: LiteralValue) -> Self {
        Self {
            start: 0,
            end: 0,
            raw: JValue::from(value.clone()).to_string(),
            value,
            digest: None,
        }
    }

    /// Get the constraint level for this literal.
    /// Literals are always not constrained.
    pub fn get_constraint_level(&self) -> ConstraintLevel {
        ConstraintLevel::None {
            source_ranges: vec![self.into()],
        }
    }
}

impl From<Literal> for KclValue {
    fn from(literal: Literal) -> Self {
        KclValue::UserVal(UserVal {
            value: JValue::from(literal.value.clone()),
            meta: vec![Metadata {
                source_range: literal.into(),
            }],
        })
    }
}

impl From<&Box<Literal>> for KclValue {
    fn from(literal: &Box<Literal>) -> Self {
        KclValue::UserVal(UserVal {
            value: JValue::from(literal.value.clone()),
            meta: vec![Metadata {
                source_range: literal.into(),
            }],
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake, Eq)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct Identifier {
    pub start: usize,
    pub end: usize,
    pub name: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(Identifier);

impl Identifier {
    pub fn new(name: &str) -> Self {
        Self {
            start: 0,
            end: 0,
            name: name.to_string(),
            digest: None,
        }
    }

    /// Get the constraint level for this identifier.
    /// Identifier are always fully constrained.
    pub fn get_constraint_level(&self) -> ConstraintLevel {
        ConstraintLevel::Full {
            source_ranges: vec![self.into()],
        }
    }

    /// Rename all identifiers that have the old name to the new given name.
    fn rename(&mut self, old_name: &str, new_name: &str) {
        if self.name == old_name {
            self.name = new_name.to_string();
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake, Eq)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct TagDeclarator {
    pub start: usize,
    pub end: usize,
    #[serde(rename = "value")]
    pub name: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(TagDeclarator);

impl From<Box<TagDeclarator>> for SourceRange {
    fn from(tag: Box<TagDeclarator>) -> Self {
        Self([tag.start, tag.end])
    }
}

impl From<Box<TagDeclarator>> for Vec<SourceRange> {
    fn from(tag: Box<TagDeclarator>) -> Self {
        vec![tag.into()]
    }
}

impl From<&Box<TagDeclarator>> for KclValue {
    fn from(tag: &Box<TagDeclarator>) -> Self {
        KclValue::TagDeclarator(tag.clone())
    }
}

impl From<&TagDeclarator> for KclValue {
    fn from(tag: &TagDeclarator) -> Self {
        KclValue::TagDeclarator(Box::new(tag.clone()))
    }
}

impl From<&TagDeclarator> for TagIdentifier {
    fn from(tag: &TagDeclarator) -> Self {
        TagIdentifier {
            value: tag.name.clone(),
            info: None,
            meta: vec![Metadata {
                source_range: tag.into(),
            }],
        }
    }
}

impl From<&TagDeclarator> for CompletionItem {
    fn from(tag: &TagDeclarator) -> Self {
        CompletionItem {
            label: tag.name.to_string(),
            label_details: None,
            kind: Some(CompletionItemKind::REFERENCE),
            detail: Some("tag (A reference to an entity you previously named)".to_string()),
            documentation: None,
            deprecated: None,
            preselect: None,
            sort_text: None,
            filter_text: None,
            insert_text: None,
            insert_text_format: None,
            insert_text_mode: None,
            text_edit: None,
            additional_text_edits: None,
            command: None,
            commit_characters: None,
            data: None,
            tags: None,
        }
    }
}

impl TagDeclarator {
    pub fn new(name: &str) -> Self {
        Self {
            start: 0,
            end: 0,
            name: name.to_string(),
            digest: None,
        }
    }

    /// Get the constraint level for this identifier.
    /// TagDeclarator are always fully constrained.
    pub fn get_constraint_level(&self) -> ConstraintLevel {
        ConstraintLevel::Full {
            source_ranges: vec![self.into()],
        }
    }

    /// Rename all identifiers that have the old name to the new given name.
    fn rename(&mut self, old_name: &str, new_name: &str) {
        if self.name == old_name {
            self.name = new_name.to_string();
        }
    }

    pub fn get_lsp_symbols(&self, code: &str) -> Vec<DocumentSymbol> {
        let source_range: SourceRange = self.into();

        vec![
            #[allow(deprecated)]
            DocumentSymbol {
                name: self.name.to_string(),
                detail: None,
                kind: SymbolKind::CONSTANT,
                range: source_range.to_lsp_range(code),
                selection_range: source_range.to_lsp_range(code),
                children: None,
                tags: None,
                deprecated: None,
            },
        ]
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct PipeSubstitution {
    pub start: usize,
    pub end: usize,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(PipeSubstitution);

impl PipeSubstitution {
    pub fn new() -> Self {
        Self {
            start: 0,
            end: 0,
            digest: None,
        }
    }
}

impl Default for PipeSubstitution {
    fn default() -> Self {
        Self::new()
    }
}

impl From<PipeSubstitution> for Expr {
    fn from(pipe_substitution: PipeSubstitution) -> Self {
        Expr::PipeSubstitution(Box::new(pipe_substitution))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(rename_all = "camelCase", tag = "type")]
pub struct ArrayExpression {
    pub start: usize,
    pub end: usize,
    pub elements: Vec<Expr>,
    #[serde(default, skip_serializing_if = "NonCodeMeta::is_empty")]
    pub non_code_meta: NonCodeMeta,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(ArrayExpression);

impl From<ArrayExpression> for Expr {
    fn from(array_expression: ArrayExpression) -> Self {
        Expr::ArrayExpression(Box::new(array_expression))
    }
}

impl ArrayExpression {
    pub fn new(elements: Vec<Expr>) -> Self {
        Self {
            start: 0,
            end: 0,
            elements,
            non_code_meta: Default::default(),
            digest: None,
        }
    }

    pub fn replace_value(&mut self, source_range: SourceRange, new_value: Expr) {
        for element in &mut self.elements {
            element.replace_value(source_range, new_value.clone());
        }
    }

    pub fn get_constraint_level(&self) -> ConstraintLevel {
        if self.elements.is_empty() {
            return ConstraintLevel::Ignore {
                source_ranges: vec![self.into()],
            };
        }

        let mut constraint_levels = ConstraintLevels::new();
        for element in &self.elements {
            constraint_levels.push(element.get_constraint_level());
        }

        constraint_levels.get_constraint_level(self.into())
    }

    /// Returns a hover value that includes the given character position.
    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        for element in &self.elements {
            let element_source_range: SourceRange = element.into();
            if element_source_range.contains(pos) {
                return element.get_hover_value_for_position(pos, code);
            }
        }

        None
    }

    /// Rename all identifiers that have the old name to the new given name.
    fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        for element in &mut self.elements {
            element.rename_identifiers(old_name, new_name);
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(rename_all = "camelCase", tag = "type")]
pub struct ArrayRangeExpression {
    pub start: usize,
    pub end: usize,
    pub start_element: Box<Expr>,
    pub end_element: Box<Expr>,
    /// Is the `end_element` included in the range?
    pub end_inclusive: bool,
    // TODO (maybe) comments on range components?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(ArrayRangeExpression);

impl From<ArrayRangeExpression> for Expr {
    fn from(array_expression: ArrayRangeExpression) -> Self {
        Expr::ArrayRangeExpression(Box::new(array_expression))
    }
}

impl ArrayRangeExpression {
    pub fn new(start_element: Box<Expr>, end_element: Box<Expr>) -> Self {
        Self {
            start: 0,
            end: 0,
            start_element,
            end_element,
            end_inclusive: true,
            digest: None,
        }
    }

    pub fn replace_value(&mut self, source_range: SourceRange, new_value: Expr) {
        self.start_element.replace_value(source_range, new_value.clone());
        self.end_element.replace_value(source_range, new_value.clone());
    }

    pub fn get_constraint_level(&self) -> ConstraintLevel {
        let mut constraint_levels = ConstraintLevels::new();
        constraint_levels.push(self.start_element.get_constraint_level());
        constraint_levels.push(self.end_element.get_constraint_level());

        constraint_levels.get_constraint_level(self.into())
    }

    /// Returns a hover value that includes the given character position.
    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        for element in [&*self.start_element, &*self.end_element] {
            let element_source_range: SourceRange = element.into();
            if element_source_range.contains(pos) {
                return element.get_hover_value_for_position(pos, code);
            }
        }

        None
    }

    /// Rename all identifiers that have the old name to the new given name.
    fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        self.start_element.rename_identifiers(old_name, new_name);
        self.end_element.rename_identifiers(old_name, new_name);
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(rename_all = "camelCase", tag = "type")]
pub struct ObjectExpression {
    pub start: usize,
    pub end: usize,
    pub properties: Vec<ObjectProperty>,
    #[serde(default, skip_serializing_if = "NonCodeMeta::is_empty")]
    pub non_code_meta: NonCodeMeta,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl ObjectExpression {
    pub fn new(properties: Vec<ObjectProperty>) -> Self {
        Self {
            start: 0,
            end: 0,
            properties,
            non_code_meta: Default::default(),
            digest: None,
        }
    }

    pub fn replace_value(&mut self, source_range: SourceRange, new_value: Expr) {
        for property in &mut self.properties {
            property.value.replace_value(source_range, new_value.clone());
        }
    }

    pub fn get_constraint_level(&self) -> ConstraintLevel {
        if self.properties.is_empty() {
            return ConstraintLevel::Ignore {
                source_ranges: vec![self.into()],
            };
        }

        let mut constraint_levels = ConstraintLevels::new();
        for property in &self.properties {
            constraint_levels.push(property.value.get_constraint_level());
        }

        constraint_levels.get_constraint_level(self.into())
    }

    /// Returns a hover value that includes the given character position.
    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        for property in &self.properties {
            let property_source_range: SourceRange = property.into();
            if property_source_range.contains(pos) {
                return property.get_hover_value_for_position(pos, code);
            }
        }

        None
    }

    /// Rename all identifiers that have the old name to the new given name.
    fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        for property in &mut self.properties {
            property.value.rename_identifiers(old_name, new_name);
        }
    }
}

impl_value_meta!(ObjectExpression);

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct ObjectProperty {
    pub start: usize,
    pub end: usize,
    pub key: Identifier,
    pub value: Expr,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(ObjectProperty);

impl ObjectProperty {
    pub fn get_lsp_symbols(&self, code: &str) -> Vec<DocumentSymbol> {
        let source_range: SourceRange = self.clone().into();
        let inner_source_range: SourceRange = self.key.clone().into();
        vec![
            #[allow(deprecated)]
            DocumentSymbol {
                name: self.key.name.to_string(),
                detail: None,
                kind: SymbolKind::PROPERTY,
                range: source_range.to_lsp_range(code),
                selection_range: inner_source_range.to_lsp_range(code),
                children: None,
                tags: None,
                deprecated: None,
            },
        ]
    }

    /// Returns a hover value that includes the given character position.
    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        let value_source_range: SourceRange = self.value.clone().into();
        if value_source_range.contains(pos) {
            return self.value.get_hover_value_for_position(pos, code);
        }

        None
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub enum MemberObject {
    MemberExpression(Box<MemberExpression>),
    Identifier(Box<Identifier>),
}

impl MemberObject {
    /// Returns a hover value that includes the given character position.
    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        match self {
            MemberObject::MemberExpression(member_expression) => {
                member_expression.get_hover_value_for_position(pos, code)
            }
            MemberObject::Identifier(_identifier) => None,
        }
    }

    pub fn start(&self) -> usize {
        match self {
            MemberObject::MemberExpression(member_expression) => member_expression.start,
            MemberObject::Identifier(identifier) => identifier.start,
        }
    }

    pub fn end(&self) -> usize {
        match self {
            MemberObject::MemberExpression(member_expression) => member_expression.end,
            MemberObject::Identifier(identifier) => identifier.end,
        }
    }
}

impl From<MemberObject> for SourceRange {
    fn from(obj: MemberObject) -> Self {
        Self([obj.start(), obj.end()])
    }
}

impl From<&MemberObject> for SourceRange {
    fn from(obj: &MemberObject) -> Self {
        Self([obj.start(), obj.end()])
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub enum LiteralIdentifier {
    Identifier(Box<Identifier>),
    Literal(Box<Literal>),
}

impl LiteralIdentifier {
    pub fn start(&self) -> usize {
        match self {
            LiteralIdentifier::Identifier(identifier) => identifier.start,
            LiteralIdentifier::Literal(literal) => literal.start,
        }
    }

    pub fn end(&self) -> usize {
        match self {
            LiteralIdentifier::Identifier(identifier) => identifier.end,
            LiteralIdentifier::Literal(literal) => literal.end,
        }
    }
}

impl From<LiteralIdentifier> for SourceRange {
    fn from(id: LiteralIdentifier) -> Self {
        Self([id.start(), id.end()])
    }
}

impl From<&LiteralIdentifier> for SourceRange {
    fn from(id: &LiteralIdentifier) -> Self {
        Self([id.start(), id.end()])
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct MemberExpression {
    pub start: usize,
    pub end: usize,
    pub object: MemberObject,
    pub property: LiteralIdentifier,
    pub computed: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(MemberExpression);

impl MemberExpression {
    /// Get the constraint level for a member expression.
    /// This is always fully constrained.
    pub fn get_constraint_level(&self) -> ConstraintLevel {
        ConstraintLevel::Full {
            source_ranges: vec![self.into()],
        }
    }

    /// Returns a hover value that includes the given character position.
    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        let object_source_range: SourceRange = self.object.clone().into();
        if object_source_range.contains(pos) {
            return self.object.get_hover_value_for_position(pos, code);
        }

        None
    }

    /// Rename all identifiers that have the old name to the new given name.
    fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        match &mut self.object {
            MemberObject::MemberExpression(ref mut member_expression) => {
                member_expression.rename_identifiers(old_name, new_name)
            }
            MemberObject::Identifier(ref mut identifier) => identifier.rename(old_name, new_name),
        }

        match &mut self.property {
            LiteralIdentifier::Identifier(ref mut identifier) => identifier.rename(old_name, new_name),
            LiteralIdentifier::Literal(_) => {}
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema)]
#[ts(export)]
pub struct ObjectKeyInfo {
    pub key: LiteralIdentifier,
    pub index: usize,
    pub computed: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct BinaryExpression {
    pub start: usize,
    pub end: usize,
    pub operator: BinaryOperator,
    pub left: BinaryPart,
    pub right: BinaryPart,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(BinaryExpression);

impl BinaryExpression {
    pub fn new(operator: BinaryOperator, left: BinaryPart, right: BinaryPart) -> Self {
        Self {
            start: left.start(),
            end: right.end(),
            operator,
            left,
            right,
            digest: None,
        }
    }

    pub fn replace_value(&mut self, source_range: SourceRange, new_value: Expr) {
        self.left.replace_value(source_range, new_value.clone());
        self.right.replace_value(source_range, new_value);
    }

    pub fn get_constraint_level(&self) -> ConstraintLevel {
        let left_constraint_level = self.left.get_constraint_level();
        let right_constraint_level = self.right.get_constraint_level();

        let mut constraint_levels = ConstraintLevels::new();
        constraint_levels.push(left_constraint_level);
        constraint_levels.push(right_constraint_level);
        constraint_levels.get_constraint_level(self.into())
    }

    pub fn precedence(&self) -> u8 {
        self.operator.precedence()
    }

    /// Returns a hover value that includes the given character position.
    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        let left_source_range: SourceRange = self.left.clone().into();
        let right_source_range: SourceRange = self.right.clone().into();

        if left_source_range.contains(pos) {
            return self.left.get_hover_value_for_position(pos, code);
        }

        if right_source_range.contains(pos) {
            return self.right.get_hover_value_for_position(pos, code);
        }

        None
    }

    /// Rename all identifiers that have the old name to the new given name.
    fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        self.left.rename_identifiers(old_name, new_name);
        self.right.rename_identifiers(old_name, new_name);
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, FromStr, Display, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
#[display(style = "snake_case")]
pub enum BinaryOperator {
    /// Add two numbers.
    #[serde(rename = "+")]
    #[display("+")]
    Add,
    /// Subtract two numbers.
    #[serde(rename = "-")]
    #[display("-")]
    Sub,
    /// Multiply two numbers.
    #[serde(rename = "*")]
    #[display("*")]
    Mul,
    /// Divide two numbers.
    #[serde(rename = "/")]
    #[display("/")]
    Div,
    /// Modulo two numbers.
    #[serde(rename = "%")]
    #[display("%")]
    Mod,
    /// Raise a number to a power.
    #[serde(rename = "^")]
    #[display("^")]
    Pow,
    /// Are two numbers equal?
    #[serde(rename = "==")]
    #[display("==")]
    Eq,
    /// Are two numbers not equal?
    #[serde(rename = "!=")]
    #[display("!=")]
    Neq,
    /// Is left greater than right
    #[serde(rename = ">")]
    #[display(">")]
    Gt,
    /// Is left greater than or equal to right
    #[serde(rename = ">=")]
    #[display(">=")]
    Gte,
    /// Is left less than right
    #[serde(rename = "<")]
    #[display("<")]
    Lt,
    /// Is left less than or equal to right
    #[serde(rename = "<=")]
    #[display("<=")]
    Lte,
}

/// Mathematical associativity.
/// Should a . b . c be read as (a . b) . c, or a . (b . c)
/// See <https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Operators/Operator_precedence#precedence_and_associativity> for more.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum Associativity {
    /// Read a . b . c as (a . b) . c
    Left,
    /// Read a . b . c as a . (b . c)
    Right,
}

impl Associativity {
    pub fn is_left(&self) -> bool {
        matches!(self, Self::Left)
    }
}

impl BinaryOperator {
    pub fn digestable_id(&self) -> [u8; 3] {
        match self {
            BinaryOperator::Add => *b"add",
            BinaryOperator::Sub => *b"sub",
            BinaryOperator::Mul => *b"mul",
            BinaryOperator::Div => *b"div",
            BinaryOperator::Mod => *b"mod",
            BinaryOperator::Pow => *b"pow",
            BinaryOperator::Eq => *b"eqq",
            BinaryOperator::Neq => *b"neq",
            BinaryOperator::Gt => *b"gtr",
            BinaryOperator::Gte => *b"gte",
            BinaryOperator::Lt => *b"ltr",
            BinaryOperator::Lte => *b"lte",
        }
    }

    /// Follow JS definitions of each operator.
    /// Taken from <https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Operators/Operator_precedence#table>
    pub fn precedence(&self) -> u8 {
        match &self {
            BinaryOperator::Add | BinaryOperator::Sub => 11,
            BinaryOperator::Mul | BinaryOperator::Div | BinaryOperator::Mod => 12,
            BinaryOperator::Pow => 13,
            Self::Gt | Self::Gte | Self::Lt | Self::Lte => 9,
            Self::Eq | Self::Neq => 8,
        }
    }

    /// Follow JS definitions of each operator.
    /// Taken from <https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Operators/Operator_precedence#table>
    pub fn associativity(&self) -> Associativity {
        match self {
            Self::Add | Self::Sub | Self::Mul | Self::Div | Self::Mod => Associativity::Left,
            Self::Pow => Associativity::Right,
            Self::Gt | Self::Gte | Self::Lt | Self::Lte | Self::Eq | Self::Neq => Associativity::Left, // I don't know if this is correct
        }
    }
}
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct UnaryExpression {
    pub start: usize,
    pub end: usize,
    pub operator: UnaryOperator,
    pub argument: BinaryPart,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(UnaryExpression);

impl UnaryExpression {
    pub fn new(operator: UnaryOperator, argument: BinaryPart) -> Self {
        Self {
            start: 0,
            end: argument.end(),
            operator,
            argument,
            digest: None,
        }
    }

    pub fn replace_value(&mut self, source_range: SourceRange, new_value: Expr) {
        self.argument.replace_value(source_range, new_value);
    }

    pub fn get_constraint_level(&self) -> ConstraintLevel {
        self.argument.get_constraint_level()
    }

    /// Returns a hover value that includes the given character position.
    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        let argument_source_range: SourceRange = self.argument.clone().into();
        if argument_source_range.contains(pos) {
            return self.argument.get_hover_value_for_position(pos, code);
        }

        None
    }

    /// Rename all identifiers that have the old name to the new given name.
    fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        self.argument.rename_identifiers(old_name, new_name);
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, FromStr, Display, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
#[display(style = "snake_case")]
pub enum UnaryOperator {
    /// Negate a number.
    #[serde(rename = "-")]
    #[display("-")]
    Neg,
    /// Negate a boolean.
    #[serde(rename = "!")]
    #[display("!")]
    Not,
}

impl UnaryOperator {
    pub fn digestable_id(&self) -> [u8; 3] {
        match self {
            UnaryOperator::Neg => *b"neg",
            UnaryOperator::Not => *b"not",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(rename_all = "camelCase", tag = "type")]
pub struct PipeExpression {
    pub start: usize,
    pub end: usize,
    // TODO: Only the first body expression can be any Value.
    // The rest will be CallExpression, and the AST type should reflect this.
    pub body: Vec<Expr>,
    #[serde(default, skip_serializing_if = "NonCodeMeta::is_empty")]
    pub non_code_meta: NonCodeMeta,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(PipeExpression);

impl From<PipeExpression> for Expr {
    fn from(pipe_expression: PipeExpression) -> Self {
        Expr::PipeExpression(Box::new(pipe_expression))
    }
}

impl PipeExpression {
    pub fn new(body: Vec<Expr>) -> Self {
        Self {
            start: 0,
            end: 0,
            body,
            non_code_meta: Default::default(),
            digest: None,
        }
    }

    pub fn replace_value(&mut self, source_range: SourceRange, new_value: Expr) {
        for value in &mut self.body {
            value.replace_value(source_range, new_value.clone());
        }
    }

    pub fn get_constraint_level(&self) -> ConstraintLevel {
        if self.body.is_empty() {
            return ConstraintLevel::Ignore {
                source_ranges: vec![self.into()],
            };
        }

        // Iterate over all body expressions.
        let mut constraint_levels = ConstraintLevels::new();
        for expression in &self.body {
            constraint_levels.push(expression.get_constraint_level());
        }

        constraint_levels.get_constraint_level(self.into())
    }

    /// Returns a hover value that includes the given character position.
    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        for b in &self.body {
            let b_source_range: SourceRange = b.into();
            if b_source_range.contains(pos) {
                return b.get_hover_value_for_position(pos, code);
            }
        }

        None
    }

    #[async_recursion]
    pub async fn get_result(&self, exec_state: &mut ExecState, ctx: &ExecutorContext) -> Result<KclValue, KclError> {
        execute_pipe_body(exec_state, &self.body, self.into(), ctx).await
    }

    /// Rename all identifiers that have the old name to the new given name.
    fn rename_identifiers(&mut self, old_name: &str, new_name: &str) {
        for statement in &mut self.body {
            statement.rename_identifiers(old_name, new_name);
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, Bake, FromStr, Display)]
#[databake(path = kcl_lib::ast::types)]
#[serde(tag = "type")]
#[display(style = "snake_case")]
pub enum FnArgPrimitive {
    /// A string type.
    String,
    /// A number type.
    Number,
    /// A boolean type.
    #[display("bool")]
    #[serde(rename = "bool")]
    Boolean,
    /// A tag.
    Tag,
    /// A sketch type.
    Sketch,
    /// A sketch surface type.
    SketchSurface,
    /// An solid type.
    Solid,
}

impl FnArgPrimitive {
    pub fn digestable_id(&self) -> &[u8] {
        match self {
            FnArgPrimitive::String => b"string",
            FnArgPrimitive::Number => b"number",
            FnArgPrimitive::Boolean => b"boolean",
            FnArgPrimitive::Tag => b"tag",
            FnArgPrimitive::Sketch => b"sketch",
            FnArgPrimitive::SketchSurface => b"sketch_surface",
            FnArgPrimitive::Solid => b"solid",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[serde(tag = "type")]
pub enum FnArgType {
    /// A primitive type.
    Primitive(FnArgPrimitive),
    // An array of a primitive type.
    Array(FnArgPrimitive),
    // An object type.
    Object {
        properties: Vec<Parameter>,
    },
}

/// Parameter of a KCL function.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct Parameter {
    /// The parameter's label or name.
    pub identifier: Identifier,
    /// The type of the parameter.
    /// This is optional if the user defines a type.
    #[serde(skip)]
    pub type_: Option<FnArgType>,
    /// Is the parameter optional?
    pub optional: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct FunctionExpression {
    pub start: usize,
    pub end: usize,
    pub params: Vec<Parameter>,
    pub body: Program,
    #[serde(skip)]
    pub return_type: Option<FnArgType>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(FunctionExpression);

#[derive(Debug, PartialEq, Clone)]
pub struct RequiredParamAfterOptionalParam(pub Box<Parameter>);

impl std::fmt::Display for RequiredParamAfterOptionalParam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KCL functions must declare any optional parameters after all the required parameters. But your required parameter {} is _after_ an optional parameter. You must move it to before the optional parameters instead.", self.0.identifier.name)
    }
}

impl FunctionExpression {
    /// Function expressions don't really apply.
    pub fn get_constraint_level(&self) -> ConstraintLevel {
        ConstraintLevel::Ignore {
            source_ranges: vec![self.into()],
        }
    }

    pub fn required_and_optional_params(
        &self,
    ) -> Result<(&[Parameter], &[Parameter]), RequiredParamAfterOptionalParam> {
        let Self {
            start: _,
            end: _,
            params,
            body: _,
            digest: _,
            return_type: _,
        } = self;
        let mut found_optional = false;
        for param in params {
            if param.optional {
                found_optional = true;
            } else if found_optional {
                return Err(RequiredParamAfterOptionalParam(Box::new(param.clone())));
            }
        }
        let boundary = self.params.partition_point(|param| !param.optional);
        // SAFETY: split_at panics if the boundary is greater than the length.
        Ok(self.params.split_at(boundary))
    }

    /// Required parameters must be declared before optional parameters.
    /// This gets all the required parameters.
    pub fn required_params(&self) -> &[Parameter] {
        let end_of_required_params = self
            .params
            .iter()
            .position(|param| param.optional)
            // If there's no optional params, then all the params are required params.
            .unwrap_or(self.params.len());
        &self.params[..end_of_required_params]
    }

    /// Minimum and maximum number of arguments this function can take.
    pub fn number_of_args(&self) -> RangeInclusive<usize> {
        self.required_params().len()..=self.params.len()
    }

    pub fn replace_value(&mut self, source_range: SourceRange, new_value: Expr) {
        self.body.replace_value(source_range, new_value);
    }

    /// Returns a hover value that includes the given character position.
    pub fn get_hover_value_for_position(&self, pos: usize, code: &str) -> Option<Hover> {
        if let Some(value) = self.body.get_expr_for_position(pos) {
            return value.get_hover_value_for_position(pos, code);
        }

        None
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema, Bake)]
#[databake(path = kcl_lib::ast::types)]
#[ts(export)]
#[serde(tag = "type")]
pub struct ReturnStatement {
    pub start: usize,
    pub end: usize,
    pub argument: Expr,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub digest: Option<Digest>,
}

impl_value_meta!(ReturnStatement);

/// Describes information about a hover.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Hover {
    Function {
        name: String,
        range: LspRange,
    },
    Signature {
        name: String,
        parameter_index: u32,
        range: LspRange,
    },
    Comment {
        value: String,
        range: LspRange,
    },
}

/// Format options.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema)]
#[cfg_attr(feature = "pyo3", pyo3::pyclass)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct FormatOptions {
    /// Size of a tab in spaces.
    pub tab_size: usize,
    /// Prefer tabs over spaces.
    pub use_tabs: bool,
    /// How to handle the final newline in the file.
    /// If true, ensure file ends with a newline.
    /// If false, ensure file does not end with a newline.
    pub insert_final_newline: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatOptions {
    /// Define the default format options.
    /// We use 2 spaces for indentation.
    pub fn new() -> Self {
        Self {
            tab_size: 2,
            use_tabs: false,
            insert_final_newline: true,
        }
    }

    /// Get the indentation string for the given level.
    pub fn get_indentation(&self, level: usize) -> String {
        if self.use_tabs {
            "\t".repeat(level)
        } else {
            " ".repeat(level * self.tab_size)
        }
    }

    /// Get the indentation string for the given level.
    /// But offset the pipe operator (and a space) by one level.
    pub fn get_indentation_offset_pipe(&self, level: usize) -> String {
        if self.use_tabs {
            "\t".repeat(level + 1)
        } else {
            " ".repeat(level * self.tab_size) + " ".repeat(PIPE_OPERATOR.len() + 1).as_str()
        }
    }
}

/// The constraint level.
#[derive(Debug, Clone, Deserialize, Serialize, ts_rs::TS, JsonSchema, Display)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
#[display(style = "snake_case")]
pub enum ConstraintLevel {
    /// Ignore constraints.
    /// This is useful for stuff like pipe substitutions where we don't want it to
    /// factor into the overall constraint level.
    /// Like empty arrays or objects, etc.
    #[display("ignore")]
    Ignore { source_ranges: Vec<SourceRange> },
    /// No constraints.
    #[display("none")]
    None { source_ranges: Vec<SourceRange> },
    /// Partially constrained.
    #[display("partial")]
    Partial {
        source_ranges: Vec<SourceRange>,
        levels: ConstraintLevels,
    },
    /// Fully constrained.
    #[display("full")]
    Full { source_ranges: Vec<SourceRange> },
}

impl From<ConstraintLevel> for Vec<SourceRange> {
    fn from(constraint_level: ConstraintLevel) -> Self {
        match constraint_level {
            ConstraintLevel::Ignore { source_ranges } => source_ranges,
            ConstraintLevel::None { source_ranges } => source_ranges,
            ConstraintLevel::Partial {
                source_ranges,
                levels: _,
            } => source_ranges,
            ConstraintLevel::Full { source_ranges } => source_ranges,
        }
    }
}

impl PartialEq for ConstraintLevel {
    fn eq(&self, other: &Self) -> bool {
        // Just check the variant.
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

impl ConstraintLevel {
    pub fn update_source_ranges(&self, source_range: SourceRange) -> Self {
        match self {
            ConstraintLevel::Ignore { source_ranges: _ } => ConstraintLevel::Ignore {
                source_ranges: vec![source_range],
            },
            ConstraintLevel::None { source_ranges: _ } => ConstraintLevel::None {
                source_ranges: vec![source_range],
            },
            ConstraintLevel::Partial {
                source_ranges: _,
                levels,
            } => ConstraintLevel::Partial {
                source_ranges: vec![source_range],
                levels: levels.clone(),
            },
            ConstraintLevel::Full { source_ranges: _ } => ConstraintLevel::Full {
                source_ranges: vec![source_range],
            },
        }
    }
}

/// A vector of constraint levels.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, ts_rs::TS, JsonSchema)]
#[ts(export)]
pub struct ConstraintLevels(pub Vec<ConstraintLevel>);

impl Default for ConstraintLevels {
    fn default() -> Self {
        Self::new()
    }
}

impl ConstraintLevels {
    pub fn new() -> Self {
        Self(vec![])
    }

    pub fn push(&mut self, constraint_level: ConstraintLevel) {
        self.0.push(constraint_level);
    }

    /// Get the overall constraint level.
    pub fn get_constraint_level(&self, source_range: SourceRange) -> ConstraintLevel {
        if self.0.is_empty() {
            return ConstraintLevel::Ignore {
                source_ranges: vec![source_range],
            };
        }

        // Check if all the constraint levels are the same.
        if self
            .0
            .iter()
            .all(|level| *level == self.0[0] || matches!(level, ConstraintLevel::Ignore { .. }))
        {
            self.0[0].clone()
        } else {
            ConstraintLevel::Partial {
                source_ranges: vec![source_range],
                levels: self.clone(),
            }
        }
    }

    pub fn get_all_partial_or_full_source_ranges(&self) -> Vec<SourceRange> {
        let mut source_ranges = Vec::new();
        // Add to our source ranges anything that is not none or ignore.
        for level in &self.0 {
            match level {
                ConstraintLevel::None { source_ranges: _ } => {}
                ConstraintLevel::Ignore { source_ranges: _ } => {}
                ConstraintLevel::Partial {
                    source_ranges: _,
                    levels,
                } => {
                    source_ranges.extend(levels.get_all_partial_or_full_source_ranges());
                }
                ConstraintLevel::Full {
                    source_ranges: full_source_ranges,
                } => {
                    source_ranges.extend(full_source_ranges);
                }
            }
        }

        source_ranges
    }
}

pub(crate) fn human_friendly_type(j: &JValue) -> &'static str {
    match j {
        JValue::Null => "null",
        JValue::Bool(_) => "boolean (true/false value)",
        JValue::Number(_) => "number",
        JValue::String(_) => "string (text)",
        JValue::Array(_) => "array (list)",
        JValue::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    // We have this as a test so we can ensure it never panics with an unwrap in the server.
    #[test]
    fn test_variable_kind_to_completion() {
        let completions = VariableKind::to_completion_items().unwrap();
        assert!(!completions.is_empty());
    }

    #[test]
    fn test_get_lsp_folding_ranges() {
        let code = r#"const part001 = startSketchOn('XY')
  |> startProfileAt([0.0000000000, 5.0000000000], %)
    |> line([0.4900857016, -0.0240763666], %)

startSketchOn('XY')
  |> startProfileAt([0.0000000000, 5.0000000000], %)
    |> line([0.4900857016, -0.0240763666], %)

const part002 = "part002"
const things = [part001, 0.0]
let blah = 1
const foo = false
let baz = {a: 1, b: "thing"}

fn ghi = (x) => {
  return x
}

ghi("things")
"#;
        let tokens = crate::token::lexer(code).unwrap();
        let parser = crate::parser::Parser::new(tokens);
        let program = parser.ast().unwrap();
        let folding_ranges = program.get_lsp_folding_ranges();
        assert_eq!(folding_ranges.len(), 3);
        assert_eq!(folding_ranges[0].start_line, 29);
        assert_eq!(folding_ranges[0].end_line, 134);
        assert_eq!(
            folding_ranges[0].collapsed_text,
            Some("part001 = startSketchOn('XY')".to_string())
        );
        assert_eq!(folding_ranges[1].start_line, 155);
        assert_eq!(folding_ranges[1].end_line, 254);
        assert_eq!(
            folding_ranges[1].collapsed_text,
            Some("startSketchOn('XY')".to_string())
        );
        assert_eq!(folding_ranges[2].start_line, 390);
        assert_eq!(folding_ranges[2].end_line, 403);
        assert_eq!(folding_ranges[2].collapsed_text, Some("fn ghi = (x) => {".to_string()));
    }

    #[test]
    fn test_get_lsp_symbols() {
        let code = r#"const part001 = startSketchOn('XY')
  |> startProfileAt([0.0000000000, 5.0000000000], %)
    |> line([0.4900857016, -0.0240763666], %)

const part002 = "part002"
const things = [part001, 0.0]
let blah = 1
const foo = false
let baz = {a: 1, b: "thing"}

fn ghi = (x) => {
  return x
}
"#;
        let tokens = crate::token::lexer(code).unwrap();
        let parser = crate::parser::Parser::new(tokens);
        let program = parser.ast().unwrap();
        let symbols = program.get_lsp_symbols(code).unwrap();
        assert_eq!(symbols.len(), 7);
    }

    #[test]
    fn test_ast_get_non_code_node() {
        let some_program_string = r#"const r = 20 / pow(pi(), 1 / 3)
const h = 30

// st
const cylinder = startSketchOn('-XZ')
  |> startProfileAt([50, 0], %)
  |> arc({
       angle_end: 360,
       angle_start: 0,
       radius: r
     }, %)
  |> extrude(h, %)
"#;
        let tokens = crate::token::lexer(some_program_string).unwrap();
        let parser = crate::parser::Parser::new(tokens);
        let program = parser.ast().unwrap();

        let value = program.get_non_code_meta_for_position(50);

        assert!(value.is_some());
    }

    #[test]
    fn test_ast_get_non_code_node_pipe() {
        let some_program_string = r#"const r = 20 / pow(pi(), 1 / 3)
const h = 30

// st
const cylinder = startSketchOn('-XZ')
  |> startProfileAt([50, 0], %)
  // comment
  |> arc({
       angle_end: 360,
       angle_start: 0,
       radius: r
     }, %)
  |> extrude(h, %)
"#;
        let tokens = crate::token::lexer(some_program_string).unwrap();
        let parser = crate::parser::Parser::new(tokens);
        let program = parser.ast().unwrap();

        let value = program.get_non_code_meta_for_position(124);

        assert!(value.is_some());
    }

    #[test]
    fn test_ast_get_non_code_node_inline_comment() {
        let some_program_string = r#"const part001 = startSketchOn('XY')
  |> startProfileAt([0,0], %)
  |> xLine(5, %) // lin
"#;
        let tokens = crate::token::lexer(some_program_string).unwrap();
        let parser = crate::parser::Parser::new(tokens);
        let program = parser.ast().unwrap();

        let value = program.get_non_code_meta_for_position(86);

        assert!(value.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_parse_type_args_on_functions() {
        let some_program_string = r#"fn thing = (arg0: number, arg1: string, tag?: string) => {
    return arg0
}"#;
        let tokens = crate::token::lexer(some_program_string).unwrap();
        let parser = crate::parser::Parser::new(tokens);
        let program = parser.ast().unwrap();

        // Check the program output for the types of the parameters.
        let function = program.body.first().unwrap();
        let BodyItem::VariableDeclaration(var_decl) = function else {
            panic!("expected a variable declaration")
        };
        let Expr::FunctionExpression(ref func_expr) = var_decl.declarations.first().unwrap().init else {
            panic!("expected a function expression")
        };
        let params = &func_expr.params;
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].type_, Some(FnArgType::Primitive(FnArgPrimitive::Number)));
        assert_eq!(params[1].type_, Some(FnArgType::Primitive(FnArgPrimitive::String)));
        assert_eq!(params[2].type_, Some(FnArgType::Primitive(FnArgPrimitive::String)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_parse_type_args_array_on_functions() {
        let some_program_string = r#"fn thing = (arg0: number[], arg1: string[], tag?: string) => {
    return arg0
}"#;
        let tokens = crate::token::lexer(some_program_string).unwrap();
        let parser = crate::parser::Parser::new(tokens);
        let program = parser.ast().unwrap();

        // Check the program output for the types of the parameters.
        let function = program.body.first().unwrap();
        let BodyItem::VariableDeclaration(var_decl) = function else {
            panic!("expected a variable declaration")
        };
        let Expr::FunctionExpression(ref func_expr) = var_decl.declarations.first().unwrap().init else {
            panic!("expected a function expression")
        };
        let params = &func_expr.params;
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].type_, Some(FnArgType::Array(FnArgPrimitive::Number)));
        assert_eq!(params[1].type_, Some(FnArgType::Array(FnArgPrimitive::String)));
        assert_eq!(params[2].type_, Some(FnArgType::Primitive(FnArgPrimitive::String)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_parse_type_args_object_on_functions() {
        let some_program_string = r#"fn thing = (arg0: number[], arg1: {thing: number, things: string[], more?: string}, tag?: string) => {
    return arg0
}"#;
        let tokens = crate::token::lexer(some_program_string).unwrap();
        let parser = crate::parser::Parser::new(tokens);
        let program = parser.ast().unwrap();

        // Check the program output for the types of the parameters.
        let function = program.body.first().unwrap();
        let BodyItem::VariableDeclaration(var_decl) = function else {
            panic!("expected a variable declaration")
        };
        let Expr::FunctionExpression(ref func_expr) = var_decl.declarations.first().unwrap().init else {
            panic!("expected a function expression")
        };
        let params = &func_expr.params;
        assert_eq!(params.len(), 3);
        assert_eq!(params[0].type_, Some(FnArgType::Array(FnArgPrimitive::Number)));
        assert_eq!(
            params[1].type_,
            Some(FnArgType::Object {
                properties: vec![
                    Parameter {
                        identifier: Identifier {
                            start: 35,
                            end: 40,
                            name: "thing".to_owned(),
                            digest: None,
                        },
                        type_: Some(FnArgType::Primitive(FnArgPrimitive::Number)),
                        optional: false,
                        digest: None
                    },
                    Parameter {
                        identifier: Identifier {
                            start: 50,
                            end: 56,
                            name: "things".to_owned(),
                            digest: None,
                        },
                        type_: Some(FnArgType::Array(FnArgPrimitive::String)),
                        optional: false,
                        digest: None
                    },
                    Parameter {
                        identifier: Identifier {
                            start: 68,
                            end: 72,
                            name: "more".to_owned(),
                            digest: None
                        },
                        type_: Some(FnArgType::Primitive(FnArgPrimitive::String)),
                        optional: true,
                        digest: None
                    }
                ]
            })
        );
        assert_eq!(params[2].type_, Some(FnArgType::Primitive(FnArgPrimitive::String)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_parse_return_type_on_functions() {
        let some_program_string = r#"fn thing = () => {thing: number, things: string[], more?: string} {
    return 1
}"#;
        let tokens = crate::token::lexer(some_program_string).unwrap();
        let parser = crate::parser::Parser::new(tokens);
        let program = parser.ast().unwrap();

        // Check the program output for the types of the parameters.
        let function = program.body.first().unwrap();
        let BodyItem::VariableDeclaration(var_decl) = function else {
            panic!("expected a variable declaration")
        };
        let Expr::FunctionExpression(ref func_expr) = var_decl.declarations.first().unwrap().init else {
            panic!("expected a function expression")
        };
        let params = &func_expr.params;
        assert_eq!(params.len(), 0);
        assert_eq!(
            func_expr.return_type,
            Some(FnArgType::Object {
                properties: vec![
                    Parameter {
                        identifier: Identifier {
                            start: 18,
                            end: 23,
                            name: "thing".to_owned(),
                            digest: None
                        },
                        type_: Some(FnArgType::Primitive(FnArgPrimitive::Number)),
                        optional: false,
                        digest: None
                    },
                    Parameter {
                        identifier: Identifier {
                            start: 33,
                            end: 39,
                            name: "things".to_owned(),
                            digest: None
                        },
                        type_: Some(FnArgType::Array(FnArgPrimitive::String)),
                        optional: false,
                        digest: None
                    },
                    Parameter {
                        identifier: Identifier {
                            start: 51,
                            end: 55,
                            name: "more".to_owned(),
                            digest: None
                        },
                        type_: Some(FnArgType::Primitive(FnArgPrimitive::String)),
                        optional: true,
                        digest: None
                    }
                ]
            })
        );
    }

    #[test]
    fn required_params() {
        for (i, (test_name, expected, function_expr)) in [
            (
                "no params",
                (0..=0),
                FunctionExpression {
                    start: 0,
                    end: 0,
                    params: vec![],
                    body: Program {
                        start: 0,
                        end: 0,
                        body: Vec::new(),
                        non_code_meta: Default::default(),
                        digest: None,
                    },
                    return_type: None,
                    digest: None,
                },
            ),
            (
                "all required params",
                (1..=1),
                FunctionExpression {
                    start: 0,
                    end: 0,
                    params: vec![Parameter {
                        identifier: Identifier {
                            start: 0,
                            end: 0,
                            name: "foo".to_owned(),
                            digest: None,
                        },
                        type_: None,
                        optional: false,
                        digest: None,
                    }],
                    body: Program {
                        start: 0,
                        end: 0,
                        body: Vec::new(),
                        non_code_meta: Default::default(),
                        digest: None,
                    },
                    return_type: None,
                    digest: None,
                },
            ),
            (
                "all optional params",
                (0..=1),
                FunctionExpression {
                    start: 0,
                    end: 0,
                    params: vec![Parameter {
                        identifier: Identifier {
                            start: 0,
                            end: 0,
                            name: "foo".to_owned(),
                            digest: None,
                        },
                        type_: None,
                        optional: true,
                        digest: None,
                    }],
                    body: Program {
                        start: 0,
                        end: 0,
                        body: Vec::new(),
                        non_code_meta: Default::default(),
                        digest: None,
                    },
                    return_type: None,
                    digest: None,
                },
            ),
            (
                "mixed params",
                (1..=2),
                FunctionExpression {
                    start: 0,
                    end: 0,
                    params: vec![
                        Parameter {
                            identifier: Identifier {
                                start: 0,
                                end: 0,
                                name: "foo".to_owned(),
                                digest: None,
                            },
                            type_: None,
                            optional: false,
                            digest: None,
                        },
                        Parameter {
                            identifier: Identifier {
                                start: 0,
                                end: 0,
                                name: "bar".to_owned(),
                                digest: None,
                            },
                            type_: None,
                            optional: true,
                            digest: None,
                        },
                    ],
                    body: Program {
                        start: 0,
                        end: 0,
                        body: Vec::new(),
                        non_code_meta: Default::default(),
                        digest: None,
                    },
                    return_type: None,
                    digest: None,
                },
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let actual = function_expr.number_of_args();
            assert_eq!(expected, actual, "failed test #{i} '{test_name}'");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_parse_object_bool() {
        let some_program_string = r#"some_func({thing: true, other_thing: false})"#;
        let tokens = crate::token::lexer(some_program_string).unwrap();
        let parser = crate::parser::Parser::new(tokens);
        let program = parser.ast().unwrap();

        // We want to get the bool and verify it is a bool.

        let BodyItem::ExpressionStatement(ExpressionStatement {
            expression,
            start: _,
            end: _,
            digest: None,
        }) = program.body.first().unwrap()
        else {
            panic!("expected a function!");
        };

        let Expr::CallExpression(ce) = expression else {
            panic!("expected a function!");
        };

        assert!(!ce.arguments.is_empty());

        let Expr::ObjectExpression(oe) = ce.arguments.first().unwrap() else {
            panic!("expected a object!");
        };

        assert_eq!(oe.properties.len(), 2);

        let Expr::Literal(ref l) = oe.properties.first().unwrap().value else {
            panic!("expected a literal!");
        };

        assert_eq!(l.raw, "true");

        let Expr::Literal(ref l) = oe.properties.get(1).unwrap().value else {
            panic!("expected a literal!");
        };

        assert_eq!(l.raw, "false");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_parse_tag_named_std_lib() {
        let some_program_string = r#"startSketchOn('XY')
    |> startProfileAt([0, 0], %)
    |> line([5, 5], %, $xLine)
"#;
        let tokens = crate::token::lexer(some_program_string).unwrap();
        let parser = crate::parser::Parser::new(tokens);
        let result = parser.ast();

        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            r#"syntax: KclErrorDetails { source_ranges: [SourceRange([76, 82])], message: "Cannot assign a tag to a reserved keyword: xLine" }"#
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_parse_empty_tag() {
        let some_program_string = r#"startSketchOn('XY')
    |> startProfileAt([0, 0], %)
    |> line([5, 5], %, $)
"#;
        let tokens = crate::token::lexer(some_program_string).unwrap();
        let parser = crate::parser::Parser::new(tokens);
        let result = parser.ast();

        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            r#"syntax: KclErrorDetails { source_ranges: [SourceRange([57, 59])], message: "Unexpected token: |>" }"#
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_parse_digest() {
        let prog1_string = r#"startSketchOn('XY')
    |> startProfileAt([0, 0], %)
    |> line([5, 5], %)
"#;
        let prog1_tokens = crate::token::lexer(prog1_string).unwrap();
        let prog1_parser = crate::parser::Parser::new(prog1_tokens);
        let prog1_digest = prog1_parser.ast().unwrap().compute_digest();

        let prog2_string = r#"startSketchOn('XY')
    |> startProfileAt([0, 2], %)
    |> line([5, 5], %)
"#;
        let prog2_tokens = crate::token::lexer(prog2_string).unwrap();
        let prog2_parser = crate::parser::Parser::new(prog2_tokens);
        let prog2_digest = prog2_parser.ast().unwrap().compute_digest();

        assert!(prog1_digest != prog2_digest);

        let prog3_string = r#"startSketchOn('XY')
    |> startProfileAt([0, 0], %)
    |> line([5, 5], %)
"#;
        let prog3_tokens = crate::token::lexer(prog3_string).unwrap();
        let prog3_parser = crate::parser::Parser::new(prog3_tokens);
        let prog3_digest = prog3_parser.ast().unwrap().compute_digest();

        assert_eq!(prog1_digest, prog3_digest);
    }
}
