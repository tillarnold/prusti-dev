// © 2019-2020, ETH Zurich
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use crate::encoder::builtin_encoder::BuiltinFunctionKind;
use crate::encoder::errors::{
    ErrorCtxt, PanicCause, EncodingError, PositionlessEncodingError, WithSpan
};
use crate::encoder::Encoder;
use prusti_common::vir;
use prusti_common::config;
use rustc_hir::def_id::DefId;
use rustc_middle::{mir, ty};
use rustc_index::vec::{Idx, IndexVec};
use rustc_span::{Span, DUMMY_SP};
use rustc_ast::ast;
use log::{trace, debug};
use std::collections::HashMap;

pub static PRECONDITION_LABEL: &'static str = "pre";
pub static WAND_LHS_LABEL: &'static str = "lhs";

type Result<T> = std::result::Result<T, EncodingError>;
type PositionlessResult<T> = std::result::Result<T, PositionlessEncodingError>;

pub trait PlaceEncoder<'v, 'tcx: 'v> {

    fn encoder(&self) -> &Encoder<'v, 'tcx>;

    fn get_local_ty(&self, local: mir::Local) -> ty::Ty<'tcx>;

    fn get_local_span(&self, local: mir::Local) -> Span;

    fn encode_local_var_name(&self, local: mir::Local) -> String {
        format!("{:?}", local)
    }

    fn encode_local(&self, local: mir::Local) -> Result<vir::LocalVar> {
        let var_name = self.encode_local_var_name(local);
        let type_name = self
            .encoder()
            .encode_type_predicate_use(self.get_local_ty(local))
            .with_span(self.get_local_span(local))?;
        Ok(vir::LocalVar::new(var_name, vir::Type::TypedRef(type_name)))
    }

    /// Returns
    /// - `vir::Expr`: the expression of the projection;
    /// - `ty::Ty<'tcx>`: the type of the expression;
    /// - `Option<usize>`: optionally, the variant of the enum.
    fn encode_place(
        &self,
        place: &mir::Place<'tcx>,
    ) -> PositionlessResult<(vir::Expr, ty::Ty<'tcx>, Option<usize>)> {
        trace!("Encode place {:?}", place);
        let result = if place.projection.is_empty() {
            let local = place.local;
            (
                self.encode_local(local)?.into(),
                self.get_local_ty(local),
                None,
            )
        } else {
            self.encode_projection(place.projection.len(), *place, None)?
        };
        Ok(result)
    }

    /// - `encoded_base_place`: optionally, the already encoded place (otherwise
    ///   encoded by recursively calling self.encode_projection)
    /// Returns
    /// - `vir::Expr`: the place of the projection;
    /// - `ty::Ty<'tcx>`: the type of the place;
    /// - `Option<usize>`: optionally, the variant of the enum.
    fn encode_projection(
        &self,
        index: usize,
        place: mir::Place<'tcx>,
        encoded_base_place: Option<(vir::Expr, ty::Ty<'tcx>, Option<usize>)>,
    ) -> PositionlessResult<(vir::Expr, ty::Ty<'tcx>, Option<usize>)> {
        trace!("Encode projection {}: {:?}", index, place);

        assert!(index >= 1, "place: {:?} index: {}", place, index);

        let (encoded_base, base_ty, opt_variant_index) =
            if let Some(content) = encoded_base_place {
                content
            } else {
                if index == 1 {
                    let local = place.local;
                    (
                        self.encode_local(local).unwrap().into(),
                        self.get_local_ty(local),
                        None,
                    )
                } else {
                    self.encode_projection(index-1, place, None)?
                }
            };

        trace!("base_ty: {:?}", base_ty);

        let elem = place.projection[index-1];
        Ok(match elem {
            mir::ProjectionElem::Field(ref field, _) => {
                match base_ty.kind() {
                    ty::TyKind::Bool
                    | ty::TyKind::Int(_)
                    | ty::TyKind::Uint(_)
                    | ty::TyKind::RawPtr(_)
                    | ty::TyKind::Ref(_, _, _) => {
                        panic!("Type {:?} has no fields", base_ty)
                    }

                    ty::TyKind::Tuple(elems) => {
                        let field_name = format!("tuple_{}", field.index());
                        let field_ty = elems[field.index()].expect_ty();
                        let encoded_field = self.encoder().encode_raw_ref_field(field_name, field_ty);
                        let encoded_projection = encoded_base.field(encoded_field);
                        (encoded_projection, field_ty, None)
                    }

                    ty::TyKind::Adt(ref adt_def, ref subst) if !adt_def.is_box() => {
                        debug!("subst {:?}", subst);
                        let num_variants = adt_def.variants.len();
                        // FIXME: why this can be None?
                        let variant_index = opt_variant_index.unwrap_or_else(|| {
                            assert_eq!(num_variants, 1);
                            0
                        });
                        let tcx = self.encoder().env().tcx();
                        let variant_def = &adt_def.variants[variant_index.into()];
                        let encoded_variant = if num_variants != 1 {
                            encoded_base.variant(&variant_def.ident.as_str())
                        } else {
                            encoded_base
                        };
                        let field = &variant_def.fields[field.index()];
                        let field_ty = field.ty(tcx, subst);
                        let encoded_field = self
                            .encoder()
                            .encode_struct_field(&field.ident.as_str(), field_ty);
                        let encoded_projection = encoded_variant.field(encoded_field);
                        (encoded_projection, field_ty, None)
                    }

                    ty::TyKind::Closure(def_id, ref closure_subst) => {
                        debug!("def_id={:?} closure_subst {:?}", def_id, closure_subst);

                        let closure_subst = closure_subst.as_closure();
                        debug!("Closure subst: {:?}", closure_subst);

                        let tcx = self.encoder().env().tcx();
                        // let node_id = tcx.hir.as_local_node_id(def_id).unwrap();
                        // let field_ty = closure_subst
                        //     .upvar_tys(def_id, tcx)
                        //     .nth(field.index())
                        //     .unwrap();
                        let field_ty = closure_subst.upvar_tys().nth(field.index()).unwrap();

                        let field_name = format!("closure_{}", field.index());
                        let encoded_field = self.encoder().encode_raw_ref_field(field_name, field_ty);
                        let encoded_projection = encoded_base.field(encoded_field);

                        // let encoded_projection: vir::Expr = tcx.with_freevars(node_id, |freevars| {
                        //     let freevar = &freevars[field.index()];
                        //     let field_name = format!("closure_{}", field.index());
                        //     let encoded_field = self.encoder().encode_raw_ref_field(field_name, field_ty);
                        //     let res = encoded_base.field(encoded_field);
                        //     let var_name = tcx.hir.name(freevar.var_id()).to_string();
                        //     trace!("Field {:?} of closure corresponds to variable '{}', encoded as {}", field, var_name, res);
                        //     res
                        // });

                        let encoded_field_type = self.encoder().encode_type(field_ty);
                        // debug!("Rust closure projection {:?}", place_projection);
                        debug!("encoded_projection: {:?}", encoded_projection);

                        assert_eq!(encoded_projection.get_type(), &encoded_field_type);

                        (encoded_projection, field_ty, None)
                    }

                    ref x => unimplemented!("{:?}", x),
                }
            }

            mir::ProjectionElem::Deref => self.encode_deref(encoded_base, base_ty),

            mir::ProjectionElem::Downcast(ref adt_def, variant_index) => {
                debug!("Downcast projection {:?}, {:?}", adt_def, variant_index);
                (encoded_base, base_ty, Some(variant_index.into()))
            }

            x => unimplemented!("{:?}", x),
        })
    }

    fn encode_deref(
        &self,
        encoded_base: vir::Expr,
        base_ty: ty::Ty<'tcx>,
    ) -> (vir::Expr, ty::Ty<'tcx>, Option<usize>) {
        trace!("encode_deref {} {}", encoded_base, base_ty);
        assert!(
            self.can_be_dereferenced(base_ty),
            "Type {:?} can not be dereferenced",
            base_ty
        );
        match base_ty.kind() {
            ty::TyKind::RawPtr(ty::TypeAndMut { ty, .. })
            | ty::TyKind::Ref(_, ty, _) => {
                let access = if encoded_base.is_addr_of() {
                    // Simplify `*&<expr>` ==> `<expr>`
                    encoded_base.get_parent().unwrap()
                } else {
                    match encoded_base {
                        vir::Expr::AddrOf(box base_place, _, _) => base_place,
                        _ => {
                            let ref_field = self.encoder().encode_dereference_field(ty);
                            encoded_base.field(ref_field)
                        }
                    }
                };
                (access, ty, None)
            }
            ty::TyKind::Adt(ref adt_def, ref _subst) if adt_def.is_box() => {
                let access = if encoded_base.is_addr_of() {
                    encoded_base.get_parent().unwrap()
                } else {
                    let field_ty = base_ty.boxed_ty();
                    let ref_field = self.encoder().encode_dereference_field(field_ty);
                    encoded_base.field(ref_field)
                };
                (access, base_ty.boxed_ty(), None)
            }
            ref x => unimplemented!("{:?}", x),
        }
    }

    fn can_be_dereferenced(&self, base_ty: ty::Ty<'tcx>) -> bool {
        trace!("can_be_dereferenced {}", base_ty);
        match base_ty.kind() {
            ty::TyKind::RawPtr(..) | ty::TyKind::Ref(..) => true,

            ty::TyKind::Adt(ref adt_def, ..) if adt_def.is_box() => true,

            _ => false,
        }
    }

}

/// Place encoder used when we do not have access to MIR. For example, when
/// encoding calls to functions defined in other crates.
#[derive(Clone)]
pub struct FakeMirEncoder<'p, 'v: 'p, 'tcx: 'v> {
    encoder: &'p Encoder<'v, 'tcx>,
    tys: IndexVec<mir::Local, ty::Ty<'tcx>>,
}

impl<'p, 'v: 'p, 'tcx: 'v> FakeMirEncoder<'p, 'v, 'tcx> {
    pub fn new(
        encoder: &'p Encoder<'v, 'tcx>,
        arg_tys: Vec<ty::Ty<'tcx>>,
        return_ty: Option<ty::Ty<'tcx>>,
    ) -> Self {
        trace!("FakeMirEncoder constructor");
        let mut tys: IndexVec<mir::Local, ty::Ty<'tcx>> = IndexVec::new();
        if let Some(return_ty) = return_ty {
            tys.push(return_ty);
        } else {
            tys.push(encoder.env().tcx().mk_unit());
        }
        for arg_ty in arg_tys {
            tys.push(arg_ty);
        }
        Self {
            encoder,
            tys,
        }
    }
}

impl<'p, 'v: 'p, 'tcx: 'v> PlaceEncoder<'v, 'tcx> for FakeMirEncoder<'p, 'v, 'tcx> {

    fn encoder(&self) -> &Encoder<'v, 'tcx> {
        self.encoder
    }

    fn get_local_ty(&self, local: mir::Local) -> ty::Ty<'tcx> {
        self.tys[local]
    }

    fn get_local_span(&self, _local: mir::Local) -> Span {
        DUMMY_SP
    }
}

/// Common code used for `ProcedureEncoder` and `PureFunctionEncoder`
#[derive(Clone)]
pub struct MirEncoder<'p, 'v: 'p, 'tcx: 'v> {
    encoder: &'p Encoder<'v, 'tcx>,
    mir: &'p mir::Body<'tcx>,
    def_id: DefId,
}

impl<'p, 'v: 'p, 'tcx: 'v> PlaceEncoder<'v, 'tcx> for MirEncoder<'p, 'v, 'tcx> {

    fn encoder(&self) -> &Encoder<'v, 'tcx> {
        self.encoder
    }

    fn get_local_ty(&self, local: mir::Local) -> ty::Ty<'tcx> {
        self.mir.local_decls[local].ty
    }

    fn get_local_span(&self, local: mir::Local) -> Span {
        self.mir.local_decls[local].source_info.span
    }
}

impl<'p, 'v: 'p, 'tcx: 'v> MirEncoder<'p, 'v, 'tcx> {
    pub fn new(
        encoder: &'p Encoder<'v, 'tcx>,
        mir: &'p mir::Body<'tcx>,
        def_id: DefId,
    ) -> Self {
        trace!("MirEncoder constructor");
        MirEncoder {
            encoder,
            mir,
            def_id,
        }
    }

    pub fn is_reference(&self, base_ty: ty::Ty<'tcx>) -> bool {
        trace!("is_reference {}", base_ty);
        match base_ty.kind() {
            ty::TyKind::RawPtr(..) | ty::TyKind::Ref(..) => true,

            _ => false,
        }
    }

    pub fn eval_place(
        &self,
        place: &mir::Place<'tcx>,
    ) -> PositionlessResult<vir::Expr> {
        let (encoded_place, place_ty, _) = self.encode_place(place)?;
        Ok(self.encoder.encode_value_expr(encoded_place, place_ty))
    }

    /// Returns an `vir::Expr` that corresponds to the value of the operand
    pub fn encode_operand_expr(
        &self,
        operand: &mir::Operand<'tcx>,
    ) -> PositionlessResult<vir::Expr> {
        trace!("Encode operand expr {:?}", operand);
        Ok(match operand {
            &mir::Operand::Constant(box mir::Constant {
                literal: ty::Const { ty, val },
                ..
            }) => self.encoder.encode_const_expr(ty, val),
            &mir::Operand::Copy(ref place) | &mir::Operand::Move(ref place) => {
                let val_place = self.eval_place(&place)?;
                val_place.into()
            }
            // FIXME: Check whether the commented out code is necessary.
            // &mir::Operand::Constant(box mir::Constant {
            //     ty,
            //     literal: mir::Literal::Promoted { .. },
            //     ..
            // }) => {
            //     debug!("Incomplete encoding of promoted literal {:?}", operand);

            //     // Generate a function call that leaves the expression undefined.
            //     let encoded_type = self.encoder.encode_value_type(ty);
            //     let function_name =
            //         self.encoder
            //             .encode_builtin_function_use(BuiltinFunctionKind::Unreachable(
            //                 encoded_type.clone(),
            //             ));
            //     let pos = self.encoder.error_manager().register(
            //         // TODO: use a proper span
            //         self.mir.span,
            //         ErrorCtxt::PureFunctionCall,
            //     );
            //     vir::Expr::func_app(function_name, vec![], vec![], encoded_type, pos)
            // }
        })
    }

    pub fn get_operand_ty(&self, operand: &mir::Operand<'tcx>) -> ty::Ty<'tcx> {
        debug!("Get operand ty {:?}", operand);
        // match operand {
        //     &mir::Operand::Move(ref place) | &mir::Operand::Copy(ref place) => {
        //         let (_, ty, _) = self.encode_place(place);
        //         ty
        //     }
        //     &mir::Operand::Constant(box mir::Constant { ty, .. }) => ty,
        // }
        operand.ty(self.mir, self.encoder.env().tcx())
    }

    /// Returns an `vir::Type` that corresponds to the type of the value of the operand
    pub fn encode_operand_expr_type(&self, operand: &mir::Operand<'tcx>) -> vir::Type {
        trace!("Encode operand expr {:?}", operand);
        // match operand {
        //     &mir::Operand::Constant(box mir::Constant { ty, .. }) => {
        //         let ty = self.encoder.resolve_typaram(ty);
        //         self.encoder.encode_value_type(ty)
        //     }
        //     &mir::Operand::Copy(ref place) | &mir::Operand::Move(ref place) => {
        //         let (encoded_place, place_ty, _) = self.encode_place(place);
        //         let place_ty = self.encoder.resolve_typaram(place_ty);
        //         let value_field = self.encoder.encode_value_field(place_ty);
        //         let val_place = encoded_place.field(value_field);
        //         val_place.get_type().clone()
        //     }
        // }
        let ty = operand.ty(self.mir, self.encoder.env().tcx());
        self.encoder.encode_value_type(ty)
    }

    pub fn encode_bin_op_expr(
        &self,
        op: mir::BinOp,
        left: vir::Expr,
        right: vir::Expr,
        ty: ty::Ty<'tcx>,
    ) -> vir::Expr {
        let is_bool = ty.kind() == &ty::TyKind::Bool;
        match op {
            mir::BinOp::Eq => vir::Expr::eq_cmp(left, right),
            mir::BinOp::Ne => vir::Expr::ne_cmp(left, right),
            mir::BinOp::Gt => vir::Expr::gt_cmp(left, right),
            mir::BinOp::Ge => vir::Expr::ge_cmp(left, right),
            mir::BinOp::Lt => vir::Expr::lt_cmp(left, right),
            mir::BinOp::Le => vir::Expr::le_cmp(left, right),
            mir::BinOp::Add => vir::Expr::add(left, right),
            mir::BinOp::Sub => vir::Expr::sub(left, right),
            mir::BinOp::Rem => vir::Expr::rem(left, right),
            mir::BinOp::Div => vir::Expr::div(left, right),
            mir::BinOp::Mul => vir::Expr::mul(left, right),
            mir::BinOp::BitAnd if is_bool => vir::Expr::and(left, right),
            mir::BinOp::BitOr if is_bool => vir::Expr::or(left, right),
            mir::BinOp::BitXor if is_bool => vir::Expr::xor(left, right),
            x => unimplemented!("{:?}", x),
        }
    }

    pub fn encode_unary_op_expr(&self, op: mir::UnOp, expr: vir::Expr) -> vir::Expr {
        match op {
            mir::UnOp::Not => vir::Expr::not(expr),
            mir::UnOp::Neg => vir::Expr::minus(expr),
        }
    }

    /// Returns `true` is an overflow happened
    pub fn encode_bin_op_check(
        &self,
        op: mir::BinOp,
        left: vir::Expr,
        right: vir::Expr,
        ty: ty::Ty<'tcx>,
    ) -> vir::Expr {
        if !op.is_checkable() || !config::check_binary_operations() {
            false.into()
        } else {
            let result = self.encode_bin_op_expr(op, left.clone(), right.clone(), ty);

            match op {
                mir::BinOp::Add | mir::BinOp::Mul | mir::BinOp::Sub => match ty.kind() {
                    // Unsigned
                    ty::TyKind::Uint(ast::UintTy::U8) => vir::Expr::or(
                        vir::Expr::lt_cmp(result.clone(), std::u8::MIN.into()),
                        vir::Expr::gt_cmp(result, std::u8::MAX.into()),
                    ),
                    ty::TyKind::Uint(ast::UintTy::U16) => vir::Expr::or(
                        vir::Expr::lt_cmp(result.clone(), std::u16::MIN.into()),
                        vir::Expr::gt_cmp(result, std::u16::MAX.into()),
                    ),
                    ty::TyKind::Uint(ast::UintTy::U32) => vir::Expr::or(
                        vir::Expr::lt_cmp(result.clone(), std::u32::MIN.into()),
                        vir::Expr::gt_cmp(result, std::u32::MAX.into()),
                    ),
                    ty::TyKind::Uint(ast::UintTy::U64) => vir::Expr::or(
                        vir::Expr::lt_cmp(result.clone(), std::u64::MIN.into()),
                        vir::Expr::gt_cmp(result, std::u64::MAX.into()),
                    ),
                    ty::TyKind::Uint(ast::UintTy::U128) => vir::Expr::or(
                        vir::Expr::lt_cmp(result.clone(), std::u128::MIN.into()),
                        vir::Expr::gt_cmp(result, std::u128::MAX.into()),
                    ),
                    ty::TyKind::Uint(ast::UintTy::Usize) => vir::Expr::or(
                        vir::Expr::lt_cmp(result.clone(), std::usize::MIN.into()),
                        vir::Expr::gt_cmp(result, std::usize::MAX.into()),
                    ),
                    // Signed
                    ty::TyKind::Int(ast::IntTy::I8) => vir::Expr::or(
                        vir::Expr::lt_cmp(result.clone(), std::i8::MIN.into()),
                        vir::Expr::gt_cmp(result, std::i8::MAX.into()),
                    ),
                    ty::TyKind::Int(ast::IntTy::I16) => vir::Expr::or(
                        vir::Expr::lt_cmp(result.clone(), std::i16::MIN.into()),
                        vir::Expr::gt_cmp(result, std::i16::MIN.into()),
                    ),
                    ty::TyKind::Int(ast::IntTy::I32) => vir::Expr::or(
                        vir::Expr::lt_cmp(result.clone(), std::i32::MIN.into()),
                        vir::Expr::gt_cmp(result, std::i32::MAX.into()),
                    ),
                    ty::TyKind::Int(ast::IntTy::I64) => vir::Expr::or(
                        vir::Expr::lt_cmp(result.clone(), std::i64::MIN.into()),
                        vir::Expr::gt_cmp(result, std::i64::MAX.into()),
                    ),
                    ty::TyKind::Int(ast::IntTy::I128) => vir::Expr::or(
                        vir::Expr::lt_cmp(result.clone(), std::i128::MIN.into()),
                        vir::Expr::gt_cmp(result, std::i128::MAX.into()),
                    ),
                    ty::TyKind::Int(ast::IntTy::Isize) => vir::Expr::or(
                        vir::Expr::lt_cmp(result.clone(), std::isize::MIN.into()),
                        vir::Expr::gt_cmp(result, std::isize::MAX.into()),
                    ),

                    _ => {
                        debug!(
                            "Encoding of bin op check '{:?}' is incomplete for type {:?}",
                            op, ty
                        );
                        false.into()
                    }
                },

                mir::BinOp::Shl | mir::BinOp::Shr => {
                    debug!("Encoding of bin op check '{:?}' is incomplete", op);
                    false.into()
                }

                _ => unreachable!("{:?}", op),
            }
        }
    }

    pub fn encode_cast_expr(
        &self,
        operand: &mir::Operand<'tcx>,
        dst_ty: ty::Ty<'tcx>,
    ) -> PositionlessResult<vir::Expr> {
        let src_ty = self.get_operand_ty(operand);

        let encoded_val = match (src_ty.kind(), dst_ty.kind()) {
            (ty::TyKind::Int(ast::IntTy::I8), ty::TyKind::Int(ast::IntTy::I8))
            | (ty::TyKind::Int(ast::IntTy::I8), ty::TyKind::Int(ast::IntTy::I16))
            | (ty::TyKind::Int(ast::IntTy::I8), ty::TyKind::Int(ast::IntTy::I32))
            | (ty::TyKind::Int(ast::IntTy::I8), ty::TyKind::Int(ast::IntTy::I64))
            | (
                ty::TyKind::Int(ast::IntTy::I8),
                ty::TyKind::Int(ast::IntTy::I128),
            )
            | (
                ty::TyKind::Int(ast::IntTy::I16),
                ty::TyKind::Int(ast::IntTy::I16),
            )
            | (
                ty::TyKind::Int(ast::IntTy::I16),
                ty::TyKind::Int(ast::IntTy::I32),
            )
            | (
                ty::TyKind::Int(ast::IntTy::I16),
                ty::TyKind::Int(ast::IntTy::I64),
            )
            | (
                ty::TyKind::Int(ast::IntTy::I16),
                ty::TyKind::Int(ast::IntTy::I128),
            )
            | (
                ty::TyKind::Int(ast::IntTy::I32),
                ty::TyKind::Int(ast::IntTy::I32),
            )
            | (
                ty::TyKind::Int(ast::IntTy::I32),
                ty::TyKind::Int(ast::IntTy::I64),
            )
            | (
                ty::TyKind::Int(ast::IntTy::I32),
                ty::TyKind::Int(ast::IntTy::I128),
            )
            | (
                ty::TyKind::Int(ast::IntTy::I64),
                ty::TyKind::Int(ast::IntTy::I64),
            )
            | (
                ty::TyKind::Int(ast::IntTy::I64),
                ty::TyKind::Int(ast::IntTy::I128),
            )
            | (
                ty::TyKind::Int(ast::IntTy::I128),
                ty::TyKind::Int(ast::IntTy::I128),
            )
            | (
                ty::TyKind::Int(ast::IntTy::Isize),
                ty::TyKind::Int(ast::IntTy::Isize),
            )
            | (ty::TyKind::Char, ty::TyKind::Char)
            | (ty::TyKind::Char, ty::TyKind::Uint(ast::UintTy::U8))
            | (ty::TyKind::Char, ty::TyKind::Uint(ast::UintTy::U16))
            | (ty::TyKind::Char, ty::TyKind::Uint(ast::UintTy::U32))
            | (ty::TyKind::Char, ty::TyKind::Uint(ast::UintTy::U64))
            | (ty::TyKind::Char, ty::TyKind::Uint(ast::UintTy::U128))
            | (ty::TyKind::Uint(ast::UintTy::U8), ty::TyKind::Char)
            | (
                ty::TyKind::Uint(ast::UintTy::U8),
                ty::TyKind::Uint(ast::UintTy::U8),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U8),
                ty::TyKind::Uint(ast::UintTy::U16),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U8),
                ty::TyKind::Uint(ast::UintTy::U32),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U8),
                ty::TyKind::Uint(ast::UintTy::U64),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U8),
                ty::TyKind::Uint(ast::UintTy::U128),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U16),
                ty::TyKind::Uint(ast::UintTy::U16),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U16),
                ty::TyKind::Uint(ast::UintTy::U32),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U16),
                ty::TyKind::Uint(ast::UintTy::U64),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U16),
                ty::TyKind::Uint(ast::UintTy::U128),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U32),
                ty::TyKind::Uint(ast::UintTy::U32),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U32),
                ty::TyKind::Uint(ast::UintTy::U64),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U32),
                ty::TyKind::Uint(ast::UintTy::U128),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U64),
                ty::TyKind::Uint(ast::UintTy::U64),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U64),
                ty::TyKind::Uint(ast::UintTy::U128),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::U128),
                ty::TyKind::Uint(ast::UintTy::U128),
            )
            | (
                ty::TyKind::Uint(ast::UintTy::Usize),
                ty::TyKind::Uint(ast::UintTy::Usize),
            ) => self.encode_operand_expr(operand)?,

            _ => unimplemented!(
                "unimplemented cast from type '{:?}' to type '{:?}'",
                src_ty,
                dst_ty
            ),
        };

        Ok(encoded_val)
    }

    pub fn encode_operand_place(
        &self,
        operand: &mir::Operand<'tcx>,
    ) -> PositionlessResult<Option<vir::Expr>> {
        debug!("Encode operand place {:?}", operand);
        Ok(match operand {
            &mir::Operand::Move(ref place) | &mir::Operand::Copy(ref place) => {
                let (src, _, _) = self.encode_place(place)?;
                Some(src)
            }

            &mir::Operand::Constant(_) => None,
        })
    }

    pub fn encode_place_predicate_permission(
        &self,
        place: vir::Expr,
        perm: vir::PermAmount,
    ) -> Option<vir::Expr> {
        vir::Expr::pred_permission(place, perm)
    }

    pub fn encode_old_expr(&self, expr: vir::Expr, label: &str) -> vir::Expr {
        debug!("encode_old_expr {}, {}", expr, label);
        vir::Expr::labelled_old(label, expr)
    }

    pub fn get_span_of_location(&self, location: mir::Location) -> Span {
        self.mir.source_info(location).span
    }

    pub fn get_span_of_basic_block(&self, bbi: mir::BasicBlock) -> Span {
        let bb_data = &self.mir.basic_blocks()[bbi];
        bb_data.terminator().source_info.span
    }

    pub fn encode_expr_pos(&self, span: Span) -> vir::Position {
        self.encoder
            .error_manager()
            .register(span, ErrorCtxt::GenericExpression)
    }

    /// Return the span of the outermost macro
    pub fn get_root_span(&self, span: Span) -> Span {
        let mut res = span;
        while let Some(parent_span) = span.parent() {
            res = parent_span;
        }
        res
    }

    /// Return the cause of a call to `begin_panic`
    pub fn encode_panic_cause(&self, source_info: mir::SourceInfo) -> PanicCause {
        let macro_backtrace: Vec<_> = source_info.span.macro_backtrace().collect();
        debug!("macro_backtrace: {:?}", macro_backtrace);

        // To classify the cause of the panic it's enough to look at the top 3 macro calls
        let lookup_size = 3;
        let tcx = self.encoder.env().tcx();
        let macro_names: Vec<String> = macro_backtrace.iter()
            .take(lookup_size)
            .map(|x| x.macro_def_id.map(|y| tcx.def_path_str(y)))
            .flatten()
            .collect();
        debug!("macro_names: {:?}", macro_names);

        let macro_names_str: Vec<&str> = macro_names.iter()
            .map(|x| x.as_str())
            .collect();
        match &macro_names_str[..] {
            ["std::panic", "std::assert", "std::debug_assert", ..] =>
                PanicCause::DebugAssert,
            ["std::panic", "std::assert", ..] => PanicCause::Assert,
            ["std::panic", "std::unreachable", ..] => PanicCause::Unreachable,
            ["std::panic", "std::unimplemented", ..] => PanicCause::Unimplemented,
            ["std::panic", ..] => PanicCause::Panic,
            _ => PanicCause::Generic,
        }
    }
}
