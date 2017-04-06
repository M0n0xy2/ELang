use std::collections::{HashMap, HashSet};
use ir;
use ast;
use ast::{Span, Spanned};
use ir::tyck;

#[derive(Debug, Clone)]
pub struct SyntaxError {
    pub msg: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SymbolTable {
    globals: HashMap<String, ir::Type>,
    locals: Vec<HashMap<String, (ir::LocalVarId, ir::Type)>>,
}

impl SymbolTable {
    fn new() -> Self {
        SymbolTable {
            globals: HashMap::new(),
            locals: Vec::new(),
        }
    }

    fn start_local_scope(&mut self) {
        self.locals.push(HashMap::new());
    }

    fn end_local_scope(&mut self) {
        self.locals.pop();
    }

    fn register_local(&mut self, name: String, ty: ir::Type, id: ir::LocalVarId) -> bool {
        // return false if already on scope
        self.locals
            .last_mut()
            .unwrap()
            .insert(name, (id, ty))
            .is_none()
    }

    fn register_global(&mut self, name: String, ty: ir::Type) -> bool {
        self.globals.insert(name, ty).is_none()
    }

    fn get_var(&self, name: &String) -> Option<(ir::Type, ir::Expression)> {
        for scope in self.locals.iter().rev() {
            if let Some(&(ref id, ref ty)) = scope.get(name) {
                return Some((ir::Type::LValue(Box::new(ty.clone())),
                             ir::Expression::LocalVarLoad(id.clone())));
            }
        }
        if let Some(ty) = self.globals.get(name) {
            Some((ty.clone(), ir::Expression::GlobalLoad(name.clone())))
        } else {
            None
        }
    }
}

pub fn build_translation_unit(tu: ast::TranslationUnit)
                              -> Result<ir::TranslationUnit, SyntaxError> {
    let mut symbol_table = SymbolTable::new();

    let mut declarations = Vec::with_capacity(tu.declarations.len());
    for decl in tu.declarations {
        declarations.push(build_declaration(decl, &mut symbol_table)?);
    }

    Ok(ir::TranslationUnit { declarations: declarations })
}

fn build_declaration(decl: Spanned<ast::Declaration>,
                     symbol_table: &mut SymbolTable)
                     -> Result<ir::Declaration, SyntaxError> {
    match decl.inner {
        ast::Declaration::ExternFunction {
            name,
            params,
            return_ty,
        } => {
            let return_ty = build_type(return_ty)?;

            let mut param_types = Vec::with_capacity(params.len());
            for ty in params {
                param_types.push(build_type(ty)?);
            }

            let ty = ir::FunctionType {
                return_ty: Box::new(return_ty),
                params_ty: param_types,
            };

            if !symbol_table.register_global(name.clone(), ir::Type::Function(ty.clone())) {
                return Err(SyntaxError {
                               msg: format!("'{}' function is already defined.", name),
                               span: decl.span,
                           });
            }

            Ok(ir::Declaration::ExternFunction { name: name, ty: ty })
        }
        ast::Declaration::Function {
            name,
            params,
            return_ty,
            stmt,
        } => {
            let return_ty = build_type(return_ty)?;

            let mut param_names = Vec::with_capacity(params.len());
            let mut param_types = Vec::with_capacity(params.len());
            for (name, ty) in params {
                param_names.push(name);
                param_types.push(build_type(ty)?);
            }

            let ty = ir::FunctionType {
                return_ty: Box::new(return_ty),
                params_ty: param_types,
            };

            if !symbol_table.register_global(name.clone(), ir::Type::Function(ty.clone())) {
                return Err(SyntaxError {
                               msg: format!("'{}' function is already defined.", name),
                               span: decl.span,
                           });
            }

            let mut function_builder = FunctionBuilder::new(name, ty.clone(), symbol_table);
            function_builder.symbol_table.start_local_scope();
            for (index, (name, ty)) in param_names.into_iter().zip(ty.params_ty).enumerate() {
                if !function_builder.register_param(name.inner.clone(), ty, Some(index)) {
                    return Err(SyntaxError {
                                   msg: format!("'{}' is already defined.", name.inner),
                                   span: name.span,
                               });
                }
            }

            build_compound_statement(&mut function_builder, stmt)?;

            if *function_builder.ty.return_ty == ir::Type::Unit {
                let value = function_builder.new_temp_value(ir::Type::Unit);
                function_builder.push_statement(
                    ir::Statement::Assign(
                        value.clone(),
                        ir::Expression::Literal(ir::Literal::Unit)
                    )
                );
                let useless_label = function_builder.new_label();
                function_builder.push_terminator_label(Some(ir::Terminator::Ret(value)), useless_label);
            }

            function_builder.symbol_table.end_local_scope();

            function_builder.to_function(decl.span)
        }
    }
}

fn build_compound_statement(fb: &mut FunctionBuilder,
                            stmt: Spanned<ast::CompoundStatement>)
                            -> Result<(), SyntaxError> {
    fb.symbol_table.start_local_scope();
    for s in stmt.inner.0 {
        build_statement(fb, s)?;
    }
    fb.symbol_table.end_local_scope();
    Ok(())
}

fn build_statement(fb: &mut FunctionBuilder,
                   stmt: Spanned<ast::Statement>)
                   -> Result<(), SyntaxError> {
    match stmt.inner {
        ast::Statement::Compound(c) => build_compound_statement(fb, c),
        ast::Statement::Let { name, ty, expr } => {
            let expr_value = build_expression(fb, expr)?;
            let expr_value = build_lvalue_to_rvalue(fb, expr_value);

            let ty = if let Some(ty) = ty {
                build_type(ty)?
            } else {
                expr_value.ty.clone()
            };

            if ty == expr_value.ty {
                if !fb.register_local_variable(name.clone(), ty.clone()) {
                    return Err(SyntaxError {
                                   msg: format!("'{}' is already defined in this scope.", name),
                                   span: stmt.span,
                               });
                }

                let (_, lval_expr) = fb.symbol_table.get_var(&name).unwrap(); //TODO optimize
                let lvalue = fb.new_temp_value(ir::Type::LValue(Box::new(ty)));
                fb.push_statement(ir::Statement::Assign(lvalue.clone(), lval_expr));
                fb.push_statement(ir::Statement::LValueSet(lvalue, expr_value));
                Ok(())
            } else {
                Err(SyntaxError {
                        msg: format!("Mismatching assignment types."),
                        span: stmt.span,
                    })
            }
        }
        ast::Statement::Loop { stmt } => {
            let continue_label = fb.new_label();
            fb.push_terminator_label(None, continue_label);
            let break_label = fb.new_label();

            let old_loop_info = fb.current_loop_info;
            fb.current_loop_info = Some((continue_label, break_label));
            build_compound_statement(fb, stmt)?;
            fb.current_loop_info = old_loop_info;

            fb.push_terminator_label(Some(ir::Terminator::Br(continue_label)), break_label);
            Ok(())
        }
        ast::Statement::While { cond, stmt } => {
            let error_span = cond.span;
            let continue_label = fb.new_label();
            fb.push_terminator_label(None, continue_label);
            fb.symbol_table.start_local_scope();
            let cond_value = build_expression(fb, cond)?;
            let cond_value = build_lvalue_to_rvalue(fb, cond_value);

            if cond_value.ty != ir::Type::Bool {
                return Err(SyntaxError {
                               msg: format!("Condition type must be bool."),
                               span: error_span,
                           });
            }

            let stmt_label = fb.new_label();
            let break_label = fb.new_label();
            fb.push_terminator_label(Some(ir::Terminator::BrCond(cond_value, stmt_label, break_label)), stmt_label);

            let old_loop_info = fb.current_loop_info;
            fb.current_loop_info = Some((continue_label, break_label));
            build_compound_statement(fb, stmt)?;
            fb.current_loop_info = old_loop_info;

            fb.push_terminator_label(Some(ir::Terminator::Br(continue_label)), break_label);
            fb.symbol_table.end_local_scope();
            Ok(())
        }
        ast::Statement::If {
            if_branch,
            elseif_branches,
            else_branch,
        } => {
            let branches = vec![if_branch].into_iter().chain(elseif_branches);
            let global_end_label = fb.new_label();

            fb.symbol_table.start_local_scope();
            for branch in branches {
                let error_span = branch.0.span;
                let cond_value = build_expression(fb, branch.0)?;
                let cond_value = build_lvalue_to_rvalue(fb, cond_value);

                if cond_value.ty != ir::Type::Bool {
                    return Err(SyntaxError {
                                   msg: format!("Condition type must be bool."),
                                   span: error_span,
                               });
                }

                let if_label = fb.new_label();
                let else_label = fb.new_label();

                fb.push_terminator_label(Some(ir::Terminator::BrCond(cond_value, if_label, else_label)), if_label);
                build_compound_statement(fb, branch.1)?;
                fb.push_terminator_label(Some(ir::Terminator::Br(global_end_label)), else_label);
            }
            if let Some(branch) = else_branch {
                build_compound_statement(fb, branch)?;
            }

            fb.push_terminator_label(None, global_end_label);
            fb.symbol_table.end_local_scope();
            Ok(())
        }
        ast::Statement::Break => {
            if let Some((_, id)) = fb.current_loop_info.clone() {
                fb.push_terminator(Some(ir::Terminator::Br(id)));
                Ok(())
            } else {
                Err(SyntaxError {
                        msg: format!("Break outside loop."),
                        span: stmt.span,
                    })
            }
        }
        ast::Statement::Continue => {
            if let Some((id, _)) = fb.current_loop_info.clone() {
                fb.push_terminator(Some(ir::Terminator::Br(id)));
                Ok(())
            } else {
                Err(SyntaxError {
                        msg: format!("Continue outside loop."),
                        span: stmt.span,
                    })
            }
        }
        ast::Statement::Return { expr } => {
            let (value, error_span) = if let Some(expr) = expr {
                let error_span = expr.span;
                let value = build_expression(fb, expr)?;
                let value = build_lvalue_to_rvalue(fb, value);
                (value, error_span)
            } else {
                let value = fb.new_temp_value(ir::Type::Unit);
                fb.push_statement(ir::Statement::Assign(value.clone(), ir::Expression::Literal(ir::Literal::Unit)));
                (value, stmt.span)
            };

            if value.ty == *fb.ty.return_ty {
                fb.push_terminator(Some(ir::Terminator::Ret(value)));
                Ok(())
            } else {
                Err(SyntaxError {
                        msg: format!("Mismatching return type."),
                        span: error_span,
                    })
            }
        }
        ast::Statement::Expression { expr } => {
            build_expression(fb, expr)?;
            Ok(())
        }
    }

}

fn build_expression(fb: &mut FunctionBuilder,
                    expr: Spanned<ast::Expression>)
                    -> Result<ir::Value, SyntaxError> {
    match expr.inner {
        ast::Expression::Assign(lhs, rhs) => {
            let lhs_value = build_expression(fb, *lhs)?;
            let rhs_value = build_expression(fb, *rhs)?;
            let rhs_value = build_lvalue_to_rvalue(fb, rhs_value);

            if let ir::Type::LValue(sub) = lhs_value.ty.clone() {
                if *sub == rhs_value.ty.clone() {
                    fb.push_statement(ir::Statement::LValueSet(lhs_value, rhs_value.clone()));
                    Ok(rhs_value)
                } else {
                    Err(SyntaxError {
                            msg: format!("Mismatching type in assignment."),
                            span: expr.span,
                        })
                }
            } else {
                Err(SyntaxError {
                        msg: format!("Can't assign to a non-lvalue."),
                        span: expr.span,
                    })
            }
        }
        ast::Expression::Subscript(array, index) => {
            let array_value = build_expression(fb, *array)?;
            let array_value = build_lvalue_to_rvalue(fb, array_value);
            let index_value = build_expression(fb, *index)?;
            let index_value = build_lvalue_to_rvalue(fb, index_value);

            if let ir::Type::Array(sub, _) = array_value.ty.clone() {
                if ir::Type::Int == index_value.ty {
                    let value = fb.new_temp_value(ir::Type::LValue(sub));
                    fb.push_statement(ir::Statement::Assign(value.clone(),
                                                           ir::Expression::ReadArray(array_value,
                                                                                     index_value)));
                    Ok(value)
                } else {
                    Err(SyntaxError {
                            msg: format!("Index must be of int type."),
                            span: expr.span,
                        })
                }
            } else {
                Err(SyntaxError {
                        msg: format!("Subscript to a non-array."),
                        span: expr.span,
                    })
            }
        }
        ast::Expression::BinOp(code, lhs, rhs) => {
            if code == ast::BinOpCode::LogicalAnd {
                let logical_result = fb.register_local_logical();
                let lhs_value = build_expression(fb, *lhs)?;
                let lhs_value = build_lvalue_to_rvalue(fb, lhs_value);

                let true_label = fb.new_label();
                let false_label = fb.new_label();
                let final_label = fb.new_label();

                fb.push_terminator_label(Some(ir::Terminator::BrCond(lhs_value, true_label, false_label)), true_label);

                let rhs_value = build_expression(fb, *rhs)?;
                let rhs_value = build_lvalue_to_rvalue(fb, rhs_value);
                let final_value_rhs = fb.new_temp_value(ir::Type::LValue(Box::new(ir::Type::Bool)));
                fb.push_statement(ir::Statement::Assign(final_value_rhs.clone(), ir::Expression::LocalVarLoad(logical_result.clone())));
                fb.push_statement(ir::Statement::LValueSet(final_value_rhs, rhs_value));

                fb.push_terminator_label(Some(ir::Terminator::Br(final_label)), false_label);

                let false_value = fb.new_temp_value(ir::Type::Bool);
                fb.push_statement(ir::Statement::Assign(false_value.clone(), ir::Expression::Literal(ir::Literal::Bool(false))));
                let final_value_false = fb.new_temp_value(ir::Type::LValue(Box::new(ir::Type::Bool)));
                fb.push_statement(ir::Statement::Assign(final_value_false.clone(), ir::Expression::LocalVarLoad(logical_result.clone())));
                fb.push_statement(ir::Statement::LValueSet(final_value_false, false_value));

                fb.push_terminator_label(Some(ir::Terminator::Br(final_label)), final_label);

                let return_lvalue = fb.new_temp_value(ir::Type::LValue(Box::new(ir::Type::Bool)));
                fb.push_statement(ir::Statement::Assign(return_lvalue.clone(), ir::Expression::LocalVarLoad(logical_result.clone())));
                let return_value = fb.new_temp_value(ir::Type::Bool);
                fb.push_statement(ir::Statement::Assign(return_value.clone(), ir::Expression::LValueLoad(return_lvalue)));

                Ok(return_value)
            } else if code == ast::BinOpCode::LogicalOr {
                let logical_result = fb.register_local_logical();
                let lhs_value = build_expression(fb, *lhs)?;
                let lhs_value = build_lvalue_to_rvalue(fb, lhs_value);

                let true_label = fb.new_label();
                let false_label = fb.new_label();
                let final_label = fb.new_label();

                fb.push_terminator_label(Some(ir::Terminator::BrCond(lhs_value, true_label, false_label)), true_label);

                let true_value = fb.new_temp_value(ir::Type::Bool);
                fb.push_statement(ir::Statement::Assign(true_value.clone(), ir::Expression::Literal(ir::Literal::Bool(true))));
                let final_value_true = fb.new_temp_value(ir::Type::LValue(Box::new(ir::Type::Bool)));
                fb.push_statement(ir::Statement::Assign(final_value_true.clone(), ir::Expression::LocalVarLoad(logical_result.clone())));
                fb.push_statement(ir::Statement::LValueSet(final_value_true, true_value));

                fb.push_terminator_label(Some(ir::Terminator::Br(final_label)), false_label);

                let rhs_value = build_expression(fb, *rhs)?;
                let rhs_value = build_lvalue_to_rvalue(fb, rhs_value);
                let final_value_rhs = fb.new_temp_value(ir::Type::LValue(Box::new(ir::Type::Bool)));
                fb.push_statement(ir::Statement::Assign(final_value_rhs.clone(), ir::Expression::LocalVarLoad(logical_result.clone())));
                fb.push_statement(ir::Statement::LValueSet(final_value_rhs, rhs_value));

                fb.push_terminator_label(Some(ir::Terminator::Br(final_label)), final_label);

                let return_lvalue = fb.new_temp_value(ir::Type::LValue(Box::new(ir::Type::Bool)));
                fb.push_statement(ir::Statement::Assign(return_lvalue.clone(), ir::Expression::LocalVarLoad(logical_result.clone())));
                let return_value = fb.new_temp_value(ir::Type::Bool);
                fb.push_statement(ir::Statement::Assign(return_value.clone(), ir::Expression::LValueLoad(return_lvalue)));

                Ok(return_value)
            } else {
                let lhs_value = build_expression(fb, *lhs)?;
                let lhs_value = build_lvalue_to_rvalue(fb, lhs_value);
                let rhs_value = build_expression(fb, *rhs)?;
                let rhs_value = build_lvalue_to_rvalue(fb, rhs_value);

                if let Some((op, ty)) = tyck::binop_tyck(code, &lhs_value.ty, &rhs_value.ty) {
                    let value = fb.new_temp_value(ty);

                    fb.push_statement(ir::Statement::Assign(value.clone(),
                                                           ir::Expression::BinOp(op,
                                                                                 lhs_value,
                                                                                 rhs_value)));
                    Ok(value)
                } else {
                    Err(SyntaxError {
                            msg: format!("Operation mismatching for those types."),
                            span: expr.span,
                        })
                }
            }
        }
        ast::Expression::UnOp(code, sub) => {
            let mut sub_value = build_expression(fb, *sub)?;
            if code != ast::UnOpCode::AddressOf {
                sub_value = build_lvalue_to_rvalue(fb, sub_value);
            }

            if let Some((op, ty)) = tyck::unop_tyck(code, &sub_value.ty) {
                let value = fb.new_temp_value(ty);
                fb.push_statement(ir::Statement::Assign(value.clone(),
                                                       ir::Expression::UnOp(op, sub_value)));
                Ok(value)
            } else {
                Err(SyntaxError {
                        msg: format!("Operation mismatching for those types."),
                        span: expr.span,
                    })
            }
        }
        ast::Expression::FuncCall(func, params) => {
            let func_value = build_expression(fb, *func)?;
            let func_value = build_lvalue_to_rvalue(fb, func_value);

            if let ir::Type::Function(func_ty) = func_value.ty.clone() {
                let mut param_ty = Vec::new();
                let mut param_values = Vec::new();
                for param in params {
                    let param = build_expression(fb, param)?;
                    let param = build_lvalue_to_rvalue(fb, param);

                    param_ty.push(param.ty.clone());
                    param_values.push(param);
                }

                if param_ty == func_ty.params_ty {
                    let value = fb.new_temp_value(*func_ty.return_ty);
                    fb.push_statement(ir::Statement::Assign(value.clone(),
                                                           ir::Expression::FuncCall(func_value,
                                                                                    param_values)));
                    Ok(value)
                } else {
                    Err(SyntaxError {
                            msg: format!("Mismatching params."),
                            span: expr.span,
                        })
                }
            } else {
                Err(SyntaxError {
                        msg: format!("Not callable."),
                        span: expr.span,
                    })
            }
        }
        ast::Expression::Cast(sub_expr, target_ty) => {
            let expr_value = build_expression(fb, *sub_expr)?;
            let expr_value = build_lvalue_to_rvalue(fb, expr_value);
            let target_ty = build_type(target_ty)?;

            if let Some(code) = tyck::cast_tyck(&expr_value.ty, &target_ty) {
                let value = fb.new_temp_value(target_ty);
                fb.push_statement(ir::Statement::Assign(value.clone(), ir::Expression::CastOp(code, expr_value)));

                Ok(value)
            } else {
                Err(SyntaxError {
                    msg: format!("Unknown cast."),
                    span: expr.span,
                })
            }
        }
        ast::Expression::Paren(expr) => build_expression(fb, *expr),
        ast::Expression::Identifier(id) => {
            if let Some((ty, expr)) = fb.symbol_table.get_var(&id) {
                let value = fb.new_temp_value(ty);
                fb.push_statement(ir::Statement::Assign(value.clone(), expr));
                Ok(value)
            } else {
                Err(SyntaxError {
                        msg: format!("'{}' is not defined here.", id),
                        span: expr.span,
                    })
            }
        }
        ast::Expression::Literal(lit) => {
            let lit = build_literal(lit, expr.span)?;
            let ty = match lit {
                ir::Literal::Int(_) => ir::Type::Int,
                ir::Literal::Double(_) => ir::Type::Double,
                ir::Literal::Bool(_) => ir::Type::Bool,
                ir::Literal::Char(_) => ir::Type::Char,
                ir::Literal::Unit => ir::Type::Unit,
            };
            let value = fb.new_temp_value(ty);
            fb.push_statement(ir::Statement::Assign(value.clone(), ir::Expression::Literal(lit)));
            Ok(value)
        }
        ast::Expression::ArrayFullLiteral(_) => {
            unimplemented!()
        }
        ast::Expression::ArrayDefaultLiteral(_, _) => {
            unimplemented!()
        }
    }
}

fn build_literal(lit: ast::Literal, span: Span) -> Result<ir::Literal, SyntaxError> {
    match lit {
        ast::Literal::Unit => Ok(ir::Literal::Unit),
        ast::Literal::Int(val) => Ok(ir::Literal::Int(val)),
        ast::Literal::Double(val) => Ok(ir::Literal::Double(val)),
        ast::Literal::Bool(val) => Ok(ir::Literal::Bool(val)),
        ast::Literal::Char(val) => {
            let mut output = String::with_capacity(val.len());
            let mut slash = false;
            for c in val.chars() {
                if slash {
                    output.push(match c {
                        '\'' | '\"' => c,
                        'a' => '\x07',
                        'b' => '\x08',
                        'f' => '\x0c',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        'v' => '\x0b',
                        '0' => '\0',
                        _ => return Err(SyntaxError { msg: format!("Invalid escape char '{}'.", c), span: span })
                    });
                    slash = false;
                } else {
                    if c == '\\' {
                        slash = true;
                    } else {
                        output.push(c);
                    }
                }
            }
            if output.len() > 1 {
                Err(SyntaxError { msg: format!("Multiple char in char literal."), span: span })
            } else if output.len() == 0 {
                Err(SyntaxError { msg: format!("Empty char literal."), span: span })
            } else {
                Ok(ir::Literal::Char(output.chars().next().unwrap() as u8))
            }
        }
    }
}

fn build_lvalue_to_rvalue(fb: &mut FunctionBuilder, value: ir::Value) -> ir::Value {
    if let ir::Type::LValue(sub) = value.ty.clone() {
        let new_value = fb.new_temp_value(*sub);
        fb.push_statement(ir::Statement::Assign(new_value.clone(), ir::Expression::LValueLoad(value)));
        new_value
    } else {
        value
    }
}

fn build_type(parse_ty: Spanned<ast::ParseType>) -> Result<ir::Type, SyntaxError> {
    match parse_ty.inner {
        ast::ParseType::Unit => Ok(ir::Type::Unit),
        ast::ParseType::Array(sub, size) => Ok(ir::Type::Array(Box::new(build_type(*sub)?), size)),
        ast::ParseType::Ptr(sub) => Ok(ir::Type::Ptr(Box::new(build_type(*sub)?))),
        ast::ParseType::Lit(lit) => {
            match lit.as_str() {
                "int" => Ok(ir::Type::Int),
                "double" => Ok(ir::Type::Double),
                "bool" => Ok(ir::Type::Bool),
                "char" => Ok(ir::Type::Char),
                other => {
                    Err(SyntaxError {
                            msg: format!("Unrecognized type '{}'.", other),
                            span: parse_ty.span,
                        })
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Item {
    Statement(ir::Statement),
    TerminatorAndLabel(Option<ir::Terminator>, ir::BasicBlockId), // None if fallthrough
}

#[derive(Debug)]
struct FunctionBuilder<'a> {
    name: String,
    ty: ir::FunctionType,
    symbol_table: &'a mut SymbolTable,
    locals: Vec<ir::LocalVar>,
    items: Vec<Item>,
    local_counter: usize,
    label_counter: usize,
    current_temp_id: usize,
    current_loop_info: Option<(ir::BasicBlockId, ir::BasicBlockId)>, // (continue, break)
}

impl<'a> FunctionBuilder<'a> {
    fn new(name: String, ty: ir::FunctionType, st: &'a mut SymbolTable) -> Self {
        FunctionBuilder {
            name: name,
            ty: ty,
            symbol_table: st,
            locals: Vec::new(),
            items: Vec::new(),
            local_counter: 0,
            label_counter: 1,
            current_temp_id: 0,
            current_loop_info: None,
        }
    }

    fn to_function(self, span: Span) -> Result<ir::Declaration, SyntaxError> {
        #[derive(Clone)]
        enum PanicTerminator {
            Real(ir::Terminator),
            Panic
        }

        struct TempBasicBlock {
            id: ir::BasicBlockId,
            stmts: Vec<ir::Statement>,
            terminator: PanicTerminator,
        }

        let mut basic_blocks = Vec::new();
        let mut current_id = ir::BasicBlockId(0);
        let mut current_stmts = Vec::new();

        for item in self.items {
            match item {
                Item::Statement(s) => current_stmts.push(s),
                Item::TerminatorAndLabel(terminator, label) => {
                    let terminator = if let Some(real_ter) = terminator.clone() {
                        real_ter
                    } else {
                        ir::Terminator::Br(label)
                    };
                    basic_blocks.push(TempBasicBlock {
                        id: current_id,
                        stmts: current_stmts,
                        terminator: PanicTerminator::Real(terminator),
                    });
                    current_id = label;
                    current_stmts = Vec::new();
                }
            }
        }

        basic_blocks.push(TempBasicBlock {
            id: current_id,
            stmts: current_stmts,
            terminator: PanicTerminator::Panic,
        });

        // remove check panic
        let mut preds: HashMap<ir::BasicBlockId, HashSet<ir::BasicBlockId>> = HashMap::new();
        for bb in basic_blocks.iter() {
            if let PanicTerminator::Real(ref terminator) = bb.terminator {
                match *terminator {
                    ir::Terminator::Br(id) => {
                        preds.entry(id).or_insert(HashSet::new()).insert(bb.id);
                    }
                    ir::Terminator::Ret(_) => {
                    }
                    ir::Terminator::BrCond(_, id1, id2) => {
                        preds.entry(id1).or_insert(HashSet::new()).insert(bb.id);
                        preds.entry(id2).or_insert(HashSet::new()).insert(bb.id);
                    }
                }
            }
        }

        let mut opened = Vec::new();
        opened.push(basic_blocks[basic_blocks.len()-1].id);
        let mut panic_preds = HashSet::<ir::BasicBlockId>::new();

        while opened.len() != 0 {
            let id = opened.pop().unwrap();
            for pred in preds.get(&id).unwrap_or(&HashSet::new()) {
                if !panic_preds.contains(pred) {
                    opened.push(*pred);
                }
            }
            panic_preds.insert(id);
        }

        if panic_preds.contains(&ir::BasicBlockId(0)) {
            return Err(SyntaxError {
                msg: format!("Not all paths return."),
                span: span,
            })
        } else {
            basic_blocks.pop();
        }

        basic_blocks.retain(|bb| preds.get(&bb.id).map(|s| s.len()).unwrap_or(0) != 0 || bb.id.0 == 0);

        let real_bbs = basic_blocks.into_iter().map(|bb| {
            if let PanicTerminator::Real(term) = bb.terminator {
                ir::BasicBlock {
                    id: bb.id,
                    stmts: bb.stmts,
                    terminator: term
                }
            } else {
                unreachable!()
            }
        }).collect();

        Ok(ir::Declaration::Function {
            name: self.name,
            ty: self.ty,
            locals: self.locals,
            bbs: real_bbs,
        })
    }

    fn new_temp_value(&mut self, ty: ir::Type) -> ir::Value {
        let id = self.current_temp_id;
        self.current_temp_id += 1;
        ir::Value { id: id, ty: ty }
    }

    fn new_label(&mut self) -> ir::BasicBlockId {
        let id = ir::BasicBlockId(self.label_counter);
        self.label_counter += 1;
        id
    }

    fn push_terminator_label(&mut self, terminator: Option<ir::Terminator>, id: ir::BasicBlockId) {
        self.items.push(Item::TerminatorAndLabel(terminator, id));
    }

    fn push_terminator(&mut self, terminator: Option<ir::Terminator>) {
        let label = self.new_label();
        self.push_terminator_label(terminator, label);
    }

    fn push_statement(&mut self, stmt: ir::Statement) {
        self.items.push(Item::Statement(stmt));
    }

    fn register_param(&mut self, name: String, ty: ir::Type, param_index: Option<usize>) -> bool {
        let res = self.symbol_table
            .register_local(name, ty.clone(), ir::LocalVarId(self.local_counter));
        self.locals
            .push(ir::LocalVar {
                      id: ir::LocalVarId(self.local_counter),
                      ty: ty,
                      param_index: param_index,
                  });
        self.local_counter += 1;
        res
    }

    fn register_local_variable(&mut self, name: String, ty: ir::Type) -> bool {
        self.register_param(name, ty, None)
    }

    fn register_local_logical(&mut self) -> ir::LocalVarId {
        let id = self.local_counter;
        self.locals
            .push(ir::LocalVar {
                      id: ir::LocalVarId(id),
                      ty: ir::Type::Bool,
                      param_index: None,
                  });
        self.local_counter += 1;
        ir::LocalVarId(id)
    }
}
