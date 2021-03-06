use crate::error::msg::SemError;
use crate::mem;
use crate::sym::TypeSym::{
    SymClass, SymClassTypeParam, SymEnum, SymFctTypeParam, SymStruct, SymTrait,
};
use crate::ty::{BuiltinType, TypeList};
use crate::typeck;
use crate::vm::{ensure_tuple, ClassId, FileId, NodeMap, VM};
use dora_parser::ast::Type::{TypeBasic, TypeLambda, TypeSelf, TypeTuple};
use dora_parser::ast::{
    Expr, ExprBlockType, Stmt, Type, TypeBasicType, TypeLambdaType, TypeTupleType,
};

mod abstractck;
mod clsdefck;
mod constdefck;
mod enumck;
mod extensiondefck;
mod fctdefck;
mod flowck;
mod globaldef;
mod globaldefck;
mod implck;
mod impldefck;
mod moduledefck;
mod nameck;
pub(crate) mod prelude;
mod returnck;
pub mod specialize;
mod structdefck;
mod superck;
mod traitdefck;
pub mod typeparamck;

macro_rules! return_on_error {
    ($vm: ident) => {{
        if $vm.diag.lock().has_errors() {
            return;
        }
    }};
}

pub fn check<'ast>(vm: &mut VM<'ast>) {
    let mut map_cls_defs = NodeMap::new(); // get ClassId from ast node
    let mut map_struct_defs = NodeMap::new(); // get StructId from ast node
    let mut map_trait_defs = NodeMap::new(); // get TraitId from ast node
    let mut map_impl_defs = NodeMap::new(); // get ImplId from ast node
    let mut map_module_defs = NodeMap::new(); // get ModuleId from ast node
    let mut map_global_defs = NodeMap::new(); // get GlobalId from ast node
    let mut map_const_defs = NodeMap::new(); // get ConstId from ast node
    let mut map_enum_defs = NodeMap::new(); // get EnumId from ast node
    let mut map_extension_defs = NodeMap::new(); // get ExtensionId from ast node

    // add user defined fcts and classes to vm
    // this check does not look into fct or class bodies
    globaldef::check(
        vm,
        &mut map_cls_defs,
        &mut map_struct_defs,
        &mut map_trait_defs,
        &mut map_impl_defs,
        &mut map_module_defs,
        &mut map_global_defs,
        &mut map_const_defs,
        &mut map_enum_defs,
        &mut map_extension_defs,
    );
    return_on_error!(vm);

    // define internal classes
    prelude::internal_classes(vm);

    // find all trait implementations for classes
    impldefck::check(vm, &vm.ast, &map_impl_defs);

    // checks class/struct/trait definitions/bodies
    clsdefck::check(vm, &vm.ast, &map_cls_defs);
    moduledefck::check(vm, &vm.ast, &map_module_defs);
    structdefck::check(vm, &vm.ast, &map_struct_defs);
    traitdefck::check(vm, &vm.ast, &map_trait_defs);
    globaldefck::check(vm, &vm.ast, &map_global_defs);
    constdefck::check(vm, &vm.ast, &map_const_defs);
    enumck::check(vm, &vm.ast, &map_enum_defs);
    extensiondefck::check(vm, &vm.ast, &map_extension_defs);
    return_on_error!(vm);

    // check super class definition of classes
    clsdefck::check_super_definition(vm, &vm.ast, &map_cls_defs);
    return_on_error!(vm);

    // check names/identifiers of local variables
    // and their usage (variable def/use, function calls) in function bodies
    nameck::check(vm);
    return_on_error!(vm);

    // check type definitions of params,
    // return types and local variables in functions
    fctdefck::check(vm);
    return_on_error!(vm);

    superck::check_override(vm);
    return_on_error!(vm);

    // check impl methods against trait definition
    implck::check(vm);
    return_on_error!(vm);

    // define internal functions & methods
    prelude::internal_functions(vm);
    prelude::known_methods(vm);

    // check types of expressions in functions
    typeck::check(vm);
    return_on_error!(vm);

    // are break and continue used in the right places?
    flowck::check(vm);

    // checks if function has a return value
    returnck::check(vm);

    // add size of super classes to field offsets
    superck::check(vm);
    return_on_error!(vm);

    abstractck::check(vm);

    // check for internal functions or classes
    internalck(vm);
    return_on_error!(vm);

    // initialize addresses for global variables
    init_global_addresses(vm);
}

fn internalck<'ast>(vm: &VM<'ast>) {
    for fct in vm.fcts.iter() {
        let fct = fct.read();

        if fct.in_class() {
            continue;
        }

        if fct.internal && !fct.internal_resolved && fct.kind.is_definition() {
            vm.diag
                .lock()
                .report(fct.file, fct.pos, SemError::UnresolvedInternal);
        }

        if fct.kind.is_definition() && !fct.in_trait() {
            vm.diag
                .lock()
                .report(fct.file, fct.pos, SemError::MissingFctBody);
        }
    }

    for cls in vm.classes.iter() {
        let cls = cls.read();

        if cls.internal && !cls.internal_resolved {
            vm.diag
                .lock()
                .report(cls.file, cls.pos, SemError::UnresolvedInternal);
        }

        for method in &cls.methods {
            let method = vm.fcts.idx(*method);
            let method = method.read();

            if method.internal && !method.internal_resolved && method.kind.is_definition() {
                vm.diag
                    .lock()
                    .report(method.file, method.pos, SemError::UnresolvedInternal);
            }

            if method.kind.is_definition() && !method.is_abstract {
                vm.diag
                    .lock()
                    .report(method.file, method.pos, SemError::MissingFctBody);
            }
        }
    }
}

fn init_global_addresses<'ast>(vm: &VM<'ast>) {
    let globals = vm.globals.lock();
    let mut size = 0;
    let mut offsets = Vec::with_capacity(globals.len());

    for glob in globals.iter() {
        let glob = glob.read();

        let initialized = size;
        size += BuiltinType::Bool.size(vm);

        let ty_size = glob.ty.size(vm);
        let ty_align = glob.ty.align(vm);

        let value = mem::align_i32(size, ty_align);
        offsets.push((initialized, value));
        size = value + ty_size;
    }

    let ptr = vm.gc.alloc_perm(size as usize);

    for (ind, glob) in globals.iter().enumerate() {
        let mut glob = glob.write();
        let (initialized, value) = offsets[ind];

        glob.address_init = ptr.offset(initialized as usize);
        glob.address_value = ptr.offset(value as usize);
    }
}

pub fn read_type<'ast>(vm: &VM<'ast>, file: FileId, t: &'ast Type) -> Option<BuiltinType> {
    match *t {
        TypeSelf(_) => Some(BuiltinType::This),
        TypeBasic(ref basic) => read_type_basic(vm, file, basic),
        TypeTuple(ref tuple) => read_type_tuple(vm, file, tuple),
        TypeLambda(ref lambda) => read_type_lambda(vm, file, lambda),
    }
}

fn read_type_basic<'ast>(
    vm: &VM<'ast>,
    file: FileId,
    basic: &'ast TypeBasicType,
) -> Option<BuiltinType> {
    let sym = vm.sym.lock().get_type(basic.name);

    if sym.is_none() {
        let name = vm.interner.str(basic.name).to_string();
        let msg = SemError::UnknownType(name);
        vm.diag.lock().report(file, basic.pos, msg);

        return None;
    }

    let sym = sym.unwrap();

    match sym {
        SymClass(cls_id) => read_type_class(vm, file, basic, cls_id),

        SymTrait(trait_id) => {
            if basic.params.len() > 0 {
                let msg = SemError::NoTypeParamsExpected;
                vm.diag.lock().report(file, basic.pos, msg);
            }

            Some(BuiltinType::Trait(trait_id))
        }

        SymStruct(struct_id) => {
            if basic.params.len() > 0 {
                let msg = SemError::NoTypeParamsExpected;
                vm.diag.lock().report(file, basic.pos, msg);
            }

            let list_id = vm.lists.lock().insert(TypeList::empty());
            Some(BuiltinType::Struct(struct_id, list_id))
        }

        SymEnum(enum_id) => {
            if basic.params.len() > 0 {
                let msg = SemError::NoTypeParamsExpected;
                vm.diag.lock().report(file, basic.pos, msg);
            }

            let list_id = vm.lists.lock().insert(TypeList::empty());
            Some(BuiltinType::Enum(enum_id, list_id))
        }

        SymClassTypeParam(cls_id, type_param_id) => {
            if basic.params.len() > 0 {
                let msg = SemError::NoTypeParamsExpected;
                vm.diag.lock().report(file, basic.pos, msg);
            }

            Some(BuiltinType::ClassTypeParam(cls_id, type_param_id))
        }

        SymFctTypeParam(fct_id, type_param_id) => {
            if basic.params.len() > 0 {
                let msg = SemError::NoTypeParamsExpected;
                vm.diag.lock().report(file, basic.pos, msg);
            }

            Some(BuiltinType::FctTypeParam(fct_id, type_param_id))
        }
    }
}

fn read_type_class<'ast>(
    vm: &VM<'ast>,
    file: FileId,
    basic: &'ast TypeBasicType,
    cls_id: ClassId,
) -> Option<BuiltinType> {
    let mut type_params = Vec::new();

    for param in &basic.params {
        let param = read_type(vm, file, param);

        if let Some(param) = param {
            type_params.push(param);
        } else {
            return None;
        }
    }

    let cls = vm.classes.idx(cls_id);
    let cls = cls.read();

    if cls.type_params.len() != type_params.len() {
        let msg = SemError::WrongNumberTypeParams(cls.type_params.len(), type_params.len());
        vm.diag.lock().report(file, basic.pos, msg);
        return None;
    }

    if type_params.len() == 0 {
        return Some(cls.ty);
    }

    for (tp, ty) in cls.type_params.iter().zip(type_params.iter()) {
        let cls_id = if let Some(cls_id) = ty.cls_id(vm) {
            cls_id
        } else {
            continue;
        };

        let cls = vm.classes.idx(cls_id);
        let cls = cls.read();

        for &trait_bound in &tp.trait_bounds {
            if !cls.implements_trait(vm, trait_bound) {
                let bound = vm.traits[trait_bound].read();
                let name = ty.name(vm);
                let trait_name = vm.interner.str(bound.name).to_string();
                let msg = SemError::TraitBoundNotSatisfied(name, trait_name);
                vm.diag.lock().report(file, basic.pos, msg);
            }
        }
    }

    let list = TypeList::with(type_params);
    let list_id = vm.lists.lock().insert(list);
    Some(BuiltinType::Class(cls.id, list_id))
}

fn read_type_tuple<'ast>(
    vm: &VM<'ast>,
    file: FileId,
    tuple: &'ast TypeTupleType,
) -> Option<BuiltinType> {
    if tuple.subtypes.len() == 0 {
        Some(BuiltinType::Unit)
    } else {
        let mut subtypes = Vec::new();

        for subtype in &tuple.subtypes {
            if let Some(ty) = read_type(vm, file, subtype) {
                subtypes.push(ty);
            } else {
                return None;
            }
        }

        let tuple_id = ensure_tuple(vm, subtypes);
        Some(BuiltinType::Tuple(tuple_id))
    }
}

fn read_type_lambda<'ast>(
    vm: &VM<'ast>,
    file: FileId,
    lambda: &'ast TypeLambdaType,
) -> Option<BuiltinType> {
    let mut params = vec![];

    for param in &lambda.params {
        if let Some(p) = read_type(vm, file, param) {
            params.push(p);
        } else {
            return None;
        }
    }

    let ret = if let Some(ret) = read_type(vm, file, &lambda.ret) {
        ret
    } else {
        return None;
    };

    let ty = vm.lambda_types.lock().insert(params, ret);
    let ty = BuiltinType::Lambda(ty);

    Some(ty)
}

pub fn always_returns(s: &Stmt) -> bool {
    returnck::returns_value(s).is_ok()
}

pub fn expr_always_returns(e: &Expr) -> bool {
    returnck::expr_returns_value(e).is_ok()
}

pub fn expr_block_always_returns(e: &ExprBlockType) -> bool {
    returnck::expr_block_returns_value(e).is_ok()
}

#[cfg(test)]
pub mod tests {
    use crate::error::msg::SemError;
    use crate::test;
    use crate::vm::VM;
    use dora_parser::lexer::position::Position;

    pub fn ok(code: &'static str) {
        test::parse_with_errors(code, |vm| {
            let diag = vm.diag.lock();
            let errors = diag.errors();

            println!("errors = {:?}", errors);
            println!();

            for e in errors {
                println!("{}", e.message(vm));
            }

            assert!(!diag.has_errors());
        });
    }

    pub fn ok_with_test<F, R>(code: &'static str, f: F) -> R
    where
        F: FnOnce(&VM) -> R,
    {
        test::parse_with_errors(code, |vm| {
            let diag = vm.diag.lock();
            let errors = diag.errors();

            println!("errors = {:?}", errors);

            for e in errors {
                println!("{}", e.message(vm));
            }

            assert!(!diag.has_errors());

            f(vm)
        })
    }

    pub fn err(code: &'static str, pos: Position, msg: SemError) {
        test::parse_with_errors(code, |vm| {
            let diag = vm.diag.lock();
            let errors = diag.errors();

            println!("errors = {:?}", errors);

            assert_eq!(1, errors.len(), "found {} errors instead", errors.len());
            assert_eq!(pos, errors[0].pos);
            assert_eq!(msg, errors[0].msg);
        });
    }

    pub fn errors(code: &'static str, vec: &[(Position, SemError)]) {
        test::parse_with_errors(code, |vm| {
            let diag = vm.diag.lock();
            let errors = diag.errors();

            println!("errors = {:?}", errors);
            assert_eq!(vec.len(), errors.len());

            for (ind, error) in errors.iter().enumerate() {
                assert_eq!(vec[ind].0, error.pos);
                assert_eq!(vec[ind].1, error.msg);
            }
        });
    }

    pub fn pos(line: u32, col: u32) -> Position {
        Position::new(line, col)
    }
}
