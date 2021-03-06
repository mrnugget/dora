use crate::error::msg::SemError;
use crate::ty::BuiltinType;
use crate::typeck::expr::{check_lit_float, check_lit_int};
use crate::vm::{ConstData, ConstValue, VM};

use dora_parser::ast::Expr::*;
use dora_parser::ast::*;
use dora_parser::lexer::token::IntSuffix;

pub struct ConstCheck<'a, 'ast: 'a> {
    pub vm: &'a VM<'ast>,
    pub xconst: &'a ConstData,
    pub negative_expr_id: NodeId,
}

impl<'a, 'ast> ConstCheck<'a, 'ast> {
    pub fn check_expr(&mut self, expr: &Expr) -> (BuiltinType, ConstValue) {
        let (ty, lit) = match expr {
            &ExprLitChar(ref expr) => (BuiltinType::Char, ConstValue::Char(expr.value)),
            &ExprLitInt(ref expr) => {
                let (ty, val) =
                    check_lit_int(self.vm, self.xconst.file, expr, false, BuiltinType::Any);
                (ty, ConstValue::Int(val))
            }
            &ExprLitFloat(ref expr) => {
                let (ty, val) = check_lit_float(self.vm, self.xconst.file, expr, false);
                (ty, ConstValue::Float(val))
            }
            &ExprLitBool(ref expr) => (BuiltinType::Bool, ConstValue::Bool(expr.value)),

            &ExprUn(ref expr) if expr.op == UnOp::Neg && expr.opnd.is_lit_int() => {
                let lit_int = expr.opnd.to_lit_int().unwrap();

                if lit_int.suffix == IntSuffix::UInt8 {
                    let ty = BuiltinType::UInt8.name(self.vm);
                    let msg = SemError::UnOpType(expr.op.as_str().into(), ty);
                    self.vm.diag.lock().report(self.xconst.file, expr.pos, msg);
                }

                let (ty, val) = check_lit_int(
                    self.vm,
                    self.xconst.file,
                    expr.opnd.to_lit_int().unwrap(),
                    true,
                    BuiltinType::Any,
                );
                (ty, ConstValue::Int(val))
            }

            &ExprUn(ref expr) if expr.op == UnOp::Neg && expr.opnd.is_lit_float() => {
                let (ty, val) = check_lit_float(
                    self.vm,
                    self.xconst.file,
                    expr.opnd.to_lit_float().unwrap(),
                    true,
                );
                (ty, ConstValue::Float(val))
            }

            _ => {
                let msg = SemError::ConstValueExpected;
                self.vm
                    .diag
                    .lock()
                    .report(self.xconst.file, expr.pos(), msg);
                return (BuiltinType::Error, ConstValue::None);
            }
        };

        if !self.xconst.ty.allows(self.vm, ty) {
            let name = self.vm.interner.str(self.xconst.name).to_string();
            let const_ty = self.xconst.ty.name(self.vm);
            let ty = ty.name(self.vm);
            let msg = SemError::AssignType(name, const_ty, ty);
            self.vm
                .diag
                .lock()
                .report(self.xconst.file, expr.pos(), msg);
        }

        (ty, lit)
    }
}
