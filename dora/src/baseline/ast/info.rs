use dora_parser::ast::visit::*;
use dora_parser::ast::Expr::*;
use dora_parser::ast::*;

use crate::baseline::ast::{Arg, CallSite, JitInfo};
use crate::cpu::*;
use crate::mem;
use crate::semck::specialize::{specialize_for_call_type, specialize_type};
use crate::ty::{BuiltinType, TypeList, TypeParamId};
use crate::vm::{CallType, Fct, FctId, FctKind, FctParent, FctSrc, Intrinsic, TraitId, VM};

pub(super) fn generate<'a, 'ast: 'a>(
    vm: &'a VM<'ast>,
    fct: &Fct<'ast>,
    src: &'a FctSrc,
    jit_info: &'a mut JitInfo<'ast>,
    cls_type_params: &TypeList,
    fct_type_params: &TypeList,
) {
    let start = if fct.has_self() { 1 } else { 0 };

    if let FctParent::Class(cls_id) = fct.parent {
        let cls = vm.classes.idx(cls_id);
        let cls = cls.read();
        assert_eq!(cls_type_params.len(), cls.type_params.len());
    } else {
        assert_eq!(cls_type_params.len(), 0);
    }

    assert_eq!(fct.type_params.len(), fct_type_params.len());

    for ty in cls_type_params.iter() {
        assert!(ty.is_concrete_type(vm));
    }

    for ty in fct_type_params.iter() {
        assert!(ty.is_concrete_type(vm));
    }

    let mut ig = InfoGenerator {
        vm,
        fct,
        ast: fct.ast,
        src,
        jit_info,

        stacksize: 0,

        param_reg_idx: start,
        param_freg_idx: 0,

        cls_type_params,
        fct_type_params,
    };

    ig.generate();
}

struct InfoGenerator<'a, 'ast: 'a> {
    vm: &'a VM<'ast>,
    fct: &'a Fct<'ast>,
    src: &'a FctSrc,
    ast: &'ast Function,
    jit_info: &'a mut JitInfo<'ast>,

    stacksize: i32,

    param_reg_idx: usize,
    param_freg_idx: usize,

    cls_type_params: &'a TypeList,
    fct_type_params: &'a TypeList,
}

impl<'a, 'ast> Visitor<'ast> for InfoGenerator<'a, 'ast> {
    fn visit_stmt(&mut self, s: &'ast Stmt) {
        visit::walk_stmt(self, s);
    }

    fn visit_expr(&mut self, e: &'ast Expr) {
        match *e {
            ExprCall(ref expr) => self.expr_call(expr),
            ExprDelegation(ref expr) => self.expr_delegation(expr),
            ExprBin(ref expr) => self.expr_bin(expr),
            ExprTypeParam(_) => unreachable!(),

            _ => visit::walk_expr(self, e),
        }
    }
}

impl<'a, 'ast> InfoGenerator<'a, 'ast> {
    fn generate(&mut self) {
        self.visit_fct(self.ast);

        self.jit_info.stacksize = mem::align_i32(self.stacksize, STACK_FRAME_ALIGNMENT as i32);
    }

    fn get_intrinsic(&self, id: NodeId) -> Option<Intrinsic> {
        let call_type = self.src.map_calls.get(id).unwrap();

        if let Some(intrinsic) = call_type.to_intrinsic() {
            return Some(intrinsic);
        }

        let fid = call_type.fct_id().unwrap();

        // the function we compile right now is never an intrinsic
        if self.fct.id == fid {
            return None;
        }

        let fct = self.vm.fcts.idx(fid);
        let fct = fct.read();

        match fct.kind {
            FctKind::Builtin(intr) => Some(intr),
            _ => None,
        }
    }

    fn expr_call(&mut self, expr: &'ast ExprCallType) {
        if let Some(_) = self.get_intrinsic(expr.id) {
            self.reserve_args_call(expr);
            return;
        }

        let call_type = self.src.map_calls.get(expr.id).unwrap().clone();

        let mut args = expr
            .args
            .iter()
            .map(|arg| Arg::Expr(arg, BuiltinType::Unit))
            .collect::<Vec<_>>();

        let callee_id = match *call_type {
            CallType::Ctor(_, fid, _) | CallType::CtorNew(_, fid, _) => {
                let ty = self.ty(expr.id);
                let arg = if call_type.is_ctor() {
                    Arg::Selfie(ty)
                } else {
                    Arg::SelfieNew(ty)
                };

                args.insert(0, arg);

                fid
            }

            CallType::Method(_, fct_id, _) => {
                let object = expr.object().unwrap();
                args.insert(0, Arg::Expr(object, BuiltinType::Unit));

                let fct = self.vm.fcts.idx(fct_id);
                let fct = fct.read();

                if fct.parent.is_trait() {
                    // This happens for calls like (T: SomeTrait).method()
                    // Find the exact method that is called
                    let trait_id = fct.trait_id();
                    let object_type = match *call_type {
                        CallType::Method(ty, _, _) => ty,
                        _ => unreachable!(),
                    };
                    let object_type = self.specialize_type(object_type);
                    self.find_trait_impl(fct_id, trait_id, object_type)
                } else {
                    fct_id
                }
            }

            CallType::Fct(fid, _, _) => fid,

            CallType::Expr(_, fid) => {
                let object = &expr.callee;
                let ty = self.ty(object.id());
                args.insert(0, Arg::Expr(object, ty));

                fid
            }

            CallType::TraitStatic(tp_id, trait_id, trait_fct_id) => {
                let list_id = match tp_id {
                    TypeParamId::Fct(list_id) => list_id,
                    TypeParamId::Class(_) => unimplemented!(),
                };

                let ty = self.fct_type_params[list_id.idx()];
                let cls_id = ty.cls_id(self.vm).expect("no cls_id for type");

                let cls = self.vm.classes.idx(cls_id);
                let cls = cls.read();

                let mut impl_fct_id: Option<FctId> = None;

                for &impl_id in &cls.impls {
                    let ximpl = self.vm.impls[impl_id].read();

                    if ximpl.trait_id != Some(trait_id) {
                        continue;
                    }

                    for &fid in &ximpl.methods {
                        let method = self.vm.fcts.idx(fid);
                        let method = method.read();

                        if method.impl_for == Some(trait_fct_id) {
                            impl_fct_id = Some(fid);
                            break;
                        }
                    }
                }

                impl_fct_id.expect("no impl_fct_id found")
            }

            CallType::Trait(_, _) => unimplemented!(),
            CallType::Intrinsic(_) => unreachable!(),
        };

        let callee = self.vm.fcts.idx(callee_id);
        let callee = callee.read();

        if let FctKind::Builtin(_) = callee.kind {
            self.reserve_args_call(expr);
            return;
        }

        self.universal_call(expr.id, args, Some(callee_id));
    }

    fn reserve_args_call(&mut self, expr: &'ast ExprCallType) {
        for arg in &expr.args {
            self.visit_expr(arg);
        }

        let call_type = self.src.map_calls.get(expr.id).unwrap();

        if call_type.is_method() {
            let object = expr.object().unwrap();
            self.visit_expr(object);
        } else if call_type.is_expr() {
            self.visit_expr(&expr.callee);
        }
    }

    fn find_trait_impl(&self, fct_id: FctId, trait_id: TraitId, object_type: BuiltinType) -> FctId {
        let cls_id = object_type.cls_id(self.vm).unwrap();
        let cls = self.vm.classes.idx(cls_id);
        let cls = cls.read();

        for &impl_id in &cls.impls {
            let ximpl = self.vm.impls[impl_id].read();

            if ximpl.trait_id() != trait_id {
                continue;
            }

            for &mtd_id in &ximpl.methods {
                let mtd = self.vm.fcts.idx(mtd_id);
                let mtd = mtd.read();

                if mtd.impl_for == Some(fct_id) {
                    return mtd_id;
                }
            }
        }

        panic!("no impl found for generic trait call")
    }

    fn expr_delegation(&mut self, expr: &'ast ExprDelegationType) {
        let mut args = expr
            .args
            .iter()
            .map(|arg| Arg::Expr(arg, BuiltinType::Unit))
            .collect::<Vec<_>>();

        let cls = self.ty(expr.id);
        args.insert(0, Arg::Selfie(cls));

        self.universal_call(expr.id, args, None);
    }

    fn universal_call(&mut self, id: NodeId, args: Vec<Arg<'ast>>, callee_id: Option<FctId>) {
        let call_type = self.src.map_calls.get(id).unwrap().clone();

        let callee_id = if let Some(callee_id) = callee_id {
            callee_id
        } else {
            call_type.fct_id().unwrap()
        };

        let csite = self.build_call_site(&*call_type, callee_id, args);

        // remember args
        self.jit_info.map_csites.insert_or_replace(id, csite);
    }

    fn build_call_site(
        &mut self,
        call_type: &CallType,
        callee_id: FctId,
        args: Vec<Arg<'ast>>,
    ) -> CallSite<'ast> {
        let callee = self.vm.fcts.idx(callee_id);
        let callee = callee.read();

        let (args, return_type, super_call) =
            self.determine_call_args_and_types(&*call_type, &*callee, args);
        let (cls_type_params, fct_type_params) = self.determine_call_type_params(&*call_type);

        for arg in &args {
            match *arg {
                Arg::Expr(ast, _) => {
                    self.visit_expr(ast);
                }

                _ => {}
            }
        }

        CallSite {
            callee: callee_id,
            args,
            cls_type_params,
            fct_type_params,
            super_call,
            return_type,
        }
    }

    fn determine_call_args_and_types(
        &mut self,
        call_type: &CallType,
        callee: &Fct<'ast>,
        args: Vec<Arg<'ast>>,
    ) -> (Vec<Arg<'ast>>, BuiltinType, bool) {
        let mut super_call = false;

        assert!(callee.params_with_self().len() == args.len());

        let args = args
            .iter()
            .enumerate()
            .map(|(ind, arg)| {
                let ty = callee.params_with_self()[ind];
                let ty = self.specialize_type(specialize_for_call_type(call_type, ty, self.vm));

                match *arg {
                    Arg::Expr(ast, _) => {
                        if ind == 0 && ast.is_super() {
                            super_call = true;
                        }

                        Arg::Expr(ast, ty)
                    }

                    Arg::Stack(offset, _) => Arg::Stack(offset, ty),
                    Arg::SelfieNew(cid) => Arg::SelfieNew(cid),
                    Arg::Selfie(cid) => Arg::Selfie(cid),
                }
            })
            .collect::<Vec<_>>();

        let return_type = self.specialize_type(specialize_for_call_type(
            call_type,
            callee.return_type,
            self.vm,
        ));

        (args, return_type, super_call)
    }

    fn determine_call_type_params(&mut self, call_type: &CallType) -> (TypeList, TypeList) {
        let cls_type_params;
        let fct_type_params;

        match *call_type {
            CallType::Ctor(_, _, ref type_params) | CallType::CtorNew(_, _, ref type_params) => {
                cls_type_params = type_params.clone();
                fct_type_params = TypeList::empty();
            }

            CallType::Method(ty, _, ref type_params) => {
                let ty = self.specialize_type(ty);

                cls_type_params = ty.type_params(self.vm);
                fct_type_params = type_params.clone();
            }

            CallType::Fct(_, ref cls_tps, ref fct_tps) => {
                cls_type_params = cls_tps.clone();
                fct_type_params = fct_tps.clone();
            }

            CallType::Expr(ty, _) => {
                let ty = self.specialize_type(ty);

                cls_type_params = ty.type_params(self.vm);
                fct_type_params = TypeList::empty();
            }

            CallType::Trait(_, _) => unimplemented!(),

            CallType::TraitStatic(_, _, _) => {
                cls_type_params = TypeList::empty();
                fct_type_params = TypeList::empty();
            }

            CallType::Intrinsic(_) => unreachable!(),
        }

        (cls_type_params, fct_type_params)
    }

    fn expr_assign(&mut self, e: &'ast ExprBinType) {
        let call_type = self.src.map_calls.get(e.id);

        if call_type.is_some() {
            let call_expr = e.lhs.to_call().unwrap();

            let object = &call_expr.callee;
            let index = &call_expr.args[0];
            let value = &e.rhs;

            if let Some(_) = self.get_intrinsic(e.id) {
                self.visit_expr(object);
                self.visit_expr(index);
                self.visit_expr(value);
            } else {
                let args = vec![
                    Arg::Expr(object, BuiltinType::Unit),
                    Arg::Expr(index, BuiltinType::Unit),
                    Arg::Expr(value, BuiltinType::Unit),
                ];

                self.universal_call(e.id, args, None);
            }
        } else if e.lhs.is_ident() {
            self.visit_expr(&e.rhs);
        } else {
            // e.lhs is a field
            let lhs = e.lhs.to_dot().unwrap();

            self.visit_expr(&lhs.lhs);
            self.visit_expr(&e.rhs);
        }
    }

    fn expr_bin(&mut self, expr: &'ast ExprBinType) {
        if expr.op.is_any_assign() {
            self.expr_assign(expr);
            return;
        }

        self.visit_expr(&expr.lhs);
        self.visit_expr(&expr.rhs);
    }

    fn reserve_stack_slot(&mut self, ty: BuiltinType) -> i32 {
        let (ty_size, ty_align) = if ty.is_nil() {
            (mem::ptr_width(), mem::ptr_width())
        } else {
            (ty.size(self.vm), ty.align(self.vm))
        };

        self.stacksize = mem::align_i32(self.stacksize, ty_align) + ty_size;

        -self.stacksize
    }

    fn ty(&self, id: NodeId) -> BuiltinType {
        let ty = self.src.ty(id);
        self.specialize_type(ty)
    }

    fn specialize_type(&self, ty: BuiltinType) -> BuiltinType {
        let result = specialize_type(self.vm, ty, &self.cls_type_params, &self.fct_type_params);
        assert!(result.is_concrete_type(self.vm));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::os;
    use crate::test;
    use crate::vm::*;

    fn info<F>(code: &'static str, f: F)
    where
        F: FnOnce(&FctSrc, &JitInfo),
    {
        os::init_page_size();

        test::parse(code, |vm| {
            let fid = vm.fct_by_name("f").unwrap();
            let fct = vm.fcts.idx(fid);
            let fct = fct.read();
            let src = fct.src();
            let mut src = src.write();
            let mut jit_info = JitInfo::new();
            let empty = TypeList::empty();

            generate(vm, &fct, &mut src, &mut jit_info, &empty, &empty);

            f(&src, &jit_info);
        });
    }
}
