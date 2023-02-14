use oxc_allocator::{Box, Vec};
use oxc_ast::{
    ast::*,
    context::{Context, StatementContext},
    Node,
};
use oxc_diagnostics::{Diagnostic, Result};

use super::declaration::{VariableDeclarationContext, VariableDeclarationParent};
use super::grammar::CoverGrammar;
use super::list::SwitchCases;
use crate::lexer::Kind;
use crate::list::NormalList;
use crate::Parser;

impl<'a> Parser<'a> {
    /// `https://tc39.es/ecma262/#prod-StatementList`
    /// `StatementList`[Yield, Await, Return] :
    ///     `StatementListItem`[?Yield, ?Await, ?Return]
    ///     `StatementList`[?Yield, ?Await, ?Return] `StatementListItem`[?Yield, ?Await, ?Return]
    pub fn parse_directives_and_statements(
        &mut self,
        is_top_level: bool,
    ) -> Result<(Vec<'a, Directive<'a>>, Vec<'a, Statement<'a>>)> {
        let mut directives = self.ast.new_vec();
        let mut statements = self.ast.new_vec();

        let mut expecting_diretives = true;
        while !self.at(Kind::Eof) {
            match self.cur_kind() {
                Kind::RCurly if !is_top_level => break,
                Kind::Import if !matches!(self.peek_kind(), Kind::Dot | Kind::LParen) => {
                    let stmt = self.parse_import_declaration()?;
                    statements.push(stmt);
                }
                Kind::Export => {
                    let stmt = self.parse_export_declaration()?;
                    statements.push(stmt);
                }
                Kind::At => {
                    self.eat_decorators()?;
                    continue;
                }
                _ => {
                    let stmt = self.parse_statement_list_item(StatementContext::StatementList)?;

                    // Section 11.2.1 Directive Prologue
                    // The only way to get a correct directive is to parse the statement first and check if it is a string literal.
                    // All other method are flawed, see test cases in [babel](https://github.com/babel/babel/blob/main/packages/babel-parser/test/fixtures/core/categorized/not-directive/input.js)
                    if expecting_diretives {
                        if let Statement::ExpressionStatement(expr) = &stmt {
                            if let Expression::StringLiteral(string) = &expr.expression {
                                let src = &self.source[string.node.start + 1..string.node.end - 1];
                                let directive =
                                    self.ast.directive(expr.node, (*string).clone(), src);
                                directives.push(directive);
                                continue;
                            }
                        }
                        expecting_diretives = false;
                    }

                    statements.push(stmt);
                }
            };
        }

        Ok((directives, statements))
    }

    /// `StatementListItem`[Yield, Await, Return] :
    ///     Statement[?Yield, ?Await, ?Return]
    ///     Declaration[?Yield, ?Await]
    pub fn parse_statement_list_item(
        &mut self,
        stmt_ctx: StatementContext,
    ) -> Result<Statement<'a>> {
        let node = self.start_node();
        if self.ts_enabled() {
            if self.at(Kind::Type)
                && self.peek_kind().is_identifier_name()
                && !self.peek_token().is_on_new_line
            {
                return self
                    .parse_ts_type_alias_declaration(false, node)
                    .map(Statement::Declaration);
            }
            if self.at(Kind::Abstract) && !self.peek_token().is_on_new_line {
                return self.parse_class_statement(stmt_ctx);
            }
            if self.is_at_ts_declaration_clause() {
                let peek_kind = self.peek_kind();
                // ignore "declare `${expr}" and "declare `template`"
                if peek_kind != Kind::TemplateHead && peek_kind != Kind::NoSubstitutionTemplate {
                    return self.parse_ts_declare_statement();
                }
            }

            if self.is_nth_at_ts_namespace_declaration(0) {
                if self.at(Kind::Namespace) || self.at(Kind::Module) {
                    return self.parse_ts_namespace_or_module_statement(/* declare */ false);
                }
                if self.at(Kind::Global) {
                    return self.parse_ts_global_statement();
                }
            }
            match self.cur_kind() {
                Kind::Const | Kind::Enum if self.is_at_enum_declaration() => {
                    return self.parse_ts_enum_declaration(false, node).map(Statement::Declaration);
                }
                Kind::Interface if self.is_at_interface_declaration() => {
                    return self
                        .parse_ts_interface_declaration(false, node)
                        .map(Statement::Declaration);
                }
                _ => (),
            }
        }

        match self.cur_kind() {
            Kind::LCurly => self.parse_block_statement(),
            Kind::Semicolon => Ok(self.parse_empty_statement()),
            Kind::If => self.parse_if_statement(),
            Kind::Do => self.parse_do_while_statement(),
            Kind::While => self.parse_while_statement(),
            Kind::For => self.parse_for_statement(),
            Kind::Break | Kind::Continue => self.parse_break_or_continue_statement(),
            Kind::With => self.parse_with_statement(),
            Kind::Switch => self.parse_switch_statement(),
            Kind::Throw => self.parse_throw_statement(),
            Kind::Try => self.parse_try_statement(),
            Kind::Debugger => self.parse_debugger_statement(),
            Kind::Class => self.parse_class_statement(stmt_ctx),
            Kind::Import if !matches!(self.peek_kind(), Kind::Dot | Kind::LParen) => {
                self.parse_import_declaration()
            }
            Kind::Export => self.parse_export_declaration(),
            // [+Return] ReturnStatement[?Yield, ?Await]
            // Error is checked in linter
            Kind::Return => self.parse_return_statement(),
            Kind::Var | Kind::Const => self.parse_variable_statement(stmt_ctx),
            Kind::Let if !self.cur_token().escaped => self.parse_let(stmt_ctx),
            Kind::At => {
                self.eat_decorators()?;
                self.parse_statement_list_item(stmt_ctx)
            }
            _ if self.at_function_with_async() => self.parse_function_declaration(stmt_ctx),
            _ => self.parse_expression_or_labeled_statment(),
        }
    }

    fn parse_expression_or_labeled_statment(&mut self) -> Result<Statement<'a>> {
        let node = self.start_node();
        let expr = self.parse_expression()?;
        if let Expression::Identifier(ident) = &expr {
            // Section 14.13 Labelled Statement
            // Avoids lookahead for a labeled statement, which is on a hot path
            if self.eat(Kind::Colon) {
                let label = LabelIdentifier { node: ident.node, name: ident.name.clone() };
                let body = self.parse_statement_list_item(StatementContext::Label)?;
                return Ok(self.ast.labeled_statement(self.end_node(node), label, body));
            }
        }
        self.parse_expression_statement(node, expr)
    }

    /// Section 14.2 Block Statement
    pub fn parse_block(&mut self) -> Result<Box<'a, BlockStatement<'a>>> {
        let node = self.start_node();
        self.expect(Kind::LCurly)?;
        let mut body = self.ast.new_vec();
        while !self.at(Kind::RCurly) && !self.at(Kind::Eof) {
            let stmt = self.parse_statement_list_item(StatementContext::StatementList)?;
            body.push(stmt);
        }
        self.expect(Kind::RCurly)?;
        Ok(self.ast.block(self.end_node(node), body))
    }

    pub fn parse_block_statement(&mut self) -> Result<Statement<'a>> {
        let block = self.parse_block()?;
        Ok(self.ast.block_statement(block))
    }

    /// Section 14.3.2 Variable Statement
    pub fn parse_variable_statement(
        &mut self,
        stmt_ctx: StatementContext,
    ) -> Result<Statement<'a>> {
        let decl = self.parse_variable_declaration(VariableDeclarationContext::new(
            VariableDeclarationParent::Statement,
        ))?;

        if stmt_ctx.is_single_statement() && decl.kind.is_lexical() {
            self.error(Diagnostic::LexicalDeclarationSingleStatement(decl.node.range()));
        }

        Ok(Statement::Declaration(Declaration::VariableDeclaration(decl)))
    }

    /// Section 14.4 Empty Statement
    fn parse_empty_statement(&mut self) -> Statement<'a> {
        let node = self.start_node();
        self.bump_any(); // bump `;`
        self.ast.empty_statement(self.end_node(node))
    }

    /// Section 14.5 Expression Statement
    pub fn parse_expression_statement(
        &mut self,
        node: Node,
        expression: Expression<'a>,
    ) -> Result<Statement<'a>> {
        self.asi()?;
        Ok(self.ast.expression_statement(self.end_node(node), expression))
    }

    /// Section 14.6 If Statement
    fn parse_if_statement(&mut self) -> Result<Statement<'a>> {
        let node = self.start_node();
        self.bump_any(); // bump `if`
        let test = self.parse_paren_expression()?;
        let consequent = self.parse_statement_list_item(StatementContext::If)?;
        let alternate = self
            .eat(Kind::Else)
            .then(|| self.parse_statement_list_item(StatementContext::If))
            .transpose()?;
        Ok(self.ast.if_statement(self.end_node(node), test, consequent, alternate))
    }

    /// Section 14.7.2 Do-While Statement
    fn parse_do_while_statement(&mut self) -> Result<Statement<'a>> {
        let node = self.start_node();
        self.bump_any(); // advance `do`
        let body = self.parse_statement_list_item(StatementContext::Do)?;
        self.expect(Kind::While)?;
        let test = self.parse_paren_expression()?;
        self.bump(Kind::Semicolon);
        Ok(self.ast.do_while_statement(self.end_node(node), body, test))
    }

    /// Section 14.7.3 While Statement
    fn parse_while_statement(&mut self) -> Result<Statement<'a>> {
        let node = self.start_node();
        self.bump_any(); // bump `while`
        let test = self.parse_paren_expression()?;
        let body = self.parse_statement_list_item(StatementContext::While)?;
        Ok(self.ast.while_statement(self.end_node(node), test, body))
    }

    /// Section 14.7.4 For Statement
    fn parse_for_statement(&mut self) -> Result<Statement<'a>> {
        let node = self.start_node();
        self.bump_any(); // bump `for`

        // [+Await]
        let r#await = self.ctx.has_await() && self.eat(Kind::Await);

        self.expect(Kind::LParen)?;

        // for (;..
        if self.at(Kind::Semicolon) {
            return self.parse_for_loop(node, None, r#await);
        }

        // for (let | for (const | for (var
        // disallow for (let in ..)
        if self.at(Kind::Const)
            || self.at(Kind::Var)
            || (self.at(Kind::Let) && self.peek_kind().is_after_let())
        {
            let init_declaration = self.without_context(Context::In, |p| {
                p.parse_variable_declaration(VariableDeclarationContext::new(
                    VariableDeclarationParent::For,
                ))
            })?;

            let kind = self.cur_kind();

            // for (.. a in) for (.. a of)
            if matches!(kind, Kind::In | Kind::Of) {
                let init = ForStatementLeft::VariableDeclaration(init_declaration);
                return self.parse_for_in_or_of_loop(node, r#await, init);
            }

            let init = Some(ForStatementInit::VariableDeclaration(init_declaration));
            return self.parse_for_loop(node, init, r#await);
        }

        let is_let_of = self.at(Kind::Let) && self.peek_at(Kind::Of);
        let is_async_of =
            self.at(Kind::Async) && !self.cur_token().escaped && self.peek_at(Kind::Of);
        let expression_node = self.start_node();

        let init_expression = self.without_context(Context::In, Parser::parse_expression)?;

        // for (a.b in ...), for ([a] in ..), for ({a} in ..)
        if self.at(Kind::In) || self.at(Kind::Of) {
            let target = AssignmentTarget::cover(init_expression, self)
                .map_err(|_| Diagnostic::UnexpectedToken(self.end_node(expression_node).range()))?;
            let for_stmt_left = ForStatementLeft::AssignmentTarget(target);

            if !r#await && is_async_of {
                self.error(Diagnostic::ForLoopAsyncOf(self.end_node(expression_node).range()));
            }

            if is_let_of {
                self.error(Diagnostic::UnexpectedKeyword(
                    "let",
                    self.end_node(expression_node).range(),
                ));
            }

            return self.parse_for_in_or_of_loop(node, r#await, for_stmt_left);
        }

        self.parse_for_loop(node, Some(ForStatementInit::Expression(init_expression)), r#await)
    }

    fn parse_for_loop(
        &mut self,
        node: Node,
        init: Option<ForStatementInit<'a>>,
        r#await: bool,
    ) -> Result<Statement<'a>> {
        self.expect(Kind::Semicolon)?;
        let test = (!self.at(Kind::Semicolon)).then(|| self.parse_expression()).transpose()?;
        self.expect(Kind::Semicolon)?;
        let update = (!self.at(Kind::RParen)).then(|| self.parse_expression()).transpose()?;
        self.expect(Kind::RParen)?;

        if r#await {
            self.error(Diagnostic::ForAwait(self.end_node(node).range()));
        }

        let body = self.parse_statement_list_item(StatementContext::For)?;

        Ok(self.ast.for_statement(self.end_node(node), init, test, update, body))
    }

    fn parse_for_in_or_of_loop(
        &mut self,
        node: Node,
        r#await: bool,
        left: ForStatementLeft<'a>,
    ) -> Result<Statement<'a>> {
        let is_for_in = self.at(Kind::In);
        self.bump_any(); // bump `in` or `of`
        let right = if is_for_in {
            self.parse_expression()
        } else {
            self.parse_assignment_expression_base()
        }?;
        self.expect(Kind::RParen)?;

        if r#await && is_for_in {
            self.error(Diagnostic::ForAwait(self.end_node(node).range()));
        }

        let body = self.parse_statement_list_item(StatementContext::For)?;
        let node = self.end_node(node);

        if is_for_in {
            Ok(self.ast.for_in_statement(node, left, right, body))
        } else {
            Ok(self.ast.for_of_statement(node, r#await, left, right, body))
        }
    }

    /// Section 14.8 Continue Statement
    /// Section 14.9 Break Statement
    fn parse_break_or_continue_statement(&mut self) -> Result<Statement<'a>> {
        let node = self.start_node();
        let kind = self.cur_kind();
        self.bump_any(); // bump `break` or `continue`
        let label =
            if self.can_insert_semicolon() { None } else { Some(self.parse_label_identifier()?) };
        self.asi()?;
        let end_node = self.end_node(node);
        match kind {
            Kind::Break => Ok(self.ast.break_statement(end_node, label)),
            Kind::Continue => Ok(self.ast.continue_statement(end_node, label)),
            _ => unreachable!(),
        }
    }

    /// Section 14.10 Return Statement
    /// `ReturnStatement`[Yield, Await] :
    ///   return ;
    ///   return [no `LineTerminator` here] Expression[+In, ?Yield, ?Await] ;
    fn parse_return_statement(&mut self) -> Result<Statement<'a>> {
        let node = self.start_node();
        self.bump_any(); // advance `return`
        let argument = if self.eat(Kind::Semicolon) || self.can_insert_semicolon() {
            None
        } else {
            let expr = self.with_context(Context::In, Parser::parse_expression)?;
            self.asi()?;
            Some(expr)
        };
        Ok(self.ast.return_statement(self.end_node(node), argument))
    }

    /// Section 14.11 With Statement
    fn parse_with_statement(&mut self) -> Result<Statement<'a>> {
        let node = self.start_node();
        self.bump_any(); // bump `with`
        let object = self.parse_paren_expression()?;
        let body = self.parse_statement_list_item(StatementContext::With)?;
        let node = self.end_node(node);
        Ok(self.ast.with_statement(node, object, body))
    }

    /// Section 14.12 Switch Statement
    fn parse_switch_statement(&mut self) -> Result<Statement<'a>> {
        let node = self.start_node();
        self.bump_any(); // advance `switch`
        let discriminant = self.parse_paren_expression()?;
        let cases = {
            let mut switch_cases = SwitchCases::new(self);
            switch_cases.parse(self)?;
            switch_cases.elements
        };
        Ok(self.ast.switch_statement(self.end_node(node), discriminant, cases))
    }

    pub fn parse_switch_case(&mut self) -> Result<SwitchCase<'a>> {
        let node = self.start_node();
        let test = match self.cur_kind() {
            Kind::Default => {
                self.bump_any();
                None
            }
            Kind::Case => {
                self.bump_any();
                let expression = self.parse_expression()?;
                Some(expression)
            }
            _ => return self.unexpected(),
        };
        self.expect(Kind::Colon)?;
        let mut consequent = self.ast.new_vec();
        while !matches!(self.cur_kind(), Kind::Case | Kind::Default | Kind::RCurly | Kind::Eof) {
            let stmt = self.parse_statement_list_item(StatementContext::StatementList)?;
            consequent.push(stmt);
        }
        Ok(self.ast.switch_case(self.end_node(node), test, consequent))
    }

    /// Section 14.14 Throw Statement
    fn parse_throw_statement(&mut self) -> Result<Statement<'a>> {
        let node = self.start_node();
        self.bump_any(); // advance `throw`
        if self.cur_token().is_on_new_line {
            self.error(Diagnostic::IllegalNewline(
                "throw",
                self.end_node(node).range(),
                self.cur_token().range(),
            ));
        }
        let argument = self.parse_expression()?;
        self.asi()?;
        Ok(self.ast.throw_statement(self.end_node(node), argument))
    }

    /// Section 14.15 Try Statement
    fn parse_try_statement(&mut self) -> Result<Statement<'a>> {
        let node = self.start_node();
        self.bump_any(); // bump `try`

        let block = self.parse_block()?;

        let handler = self.at(Kind::Catch).then(|| self.parse_catch_clause()).transpose()?;

        let finalizer = self.eat(Kind::Finally).then(|| self.parse_block()).transpose()?;

        #[allow(clippy::range_plus_one)]
        let range = self.prev_token_end..self.prev_token_end + 1;
        if handler.is_none() && finalizer.is_none() {
            self.error(Diagnostic::ExpectCatchFinally(range));
        }

        Ok(self.ast.try_statement(self.end_node(node), block, handler, finalizer))
    }

    fn parse_catch_clause(&mut self) -> Result<Box<'a, CatchClause<'a>>> {
        let node = self.start_node();
        self.bump_any(); // advance `catch`
        let param = if self.eat(Kind::LParen) {
            let pattern = self.parse_binding_pattern()?.0;
            self.expect(Kind::RParen)?;
            Some(pattern)
        } else {
            None
        };
        let body = self.parse_block()?;
        Ok(self.ast.catch_clause(self.end_node(node), param, body))
    }

    /// Section 14.16 Debugger Statement
    fn parse_debugger_statement(&mut self) -> Result<Statement<'a>> {
        let node = self.start_node();
        self.bump_any();
        self.asi()?;
        Ok(self.ast.debugger_statement(self.end_node(node)))
    }
}