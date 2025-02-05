use super::{
    ty::{translate_ty, ty_param_names},
    CloneMap, CloneSummary, TransId, Why3Generator,
};
use crate::{ctx::*, translation::traits, util};
use rustc_ast::Mutability;
use rustc_hir::{def::Namespace, def_id::DefId};
use rustc_middle::ty::{subst::SubstsRef, AdtDef, GenericArg, ParamEnv, Ty, TyCtxt, TyKind};
use rustc_span::{Symbol, DUMMY_SP};
use why3::{
    declaration::{Axiom, Decl, Module, TyDecl},
    exp::{Exp, Pattern},
    Ident,
};

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) enum TyInvKind {
    Trivial,
    Borrow,
    Adt(DefId),
    Tuple(usize),
}

impl TyInvKind {
    pub(crate) fn from_ty(ty: Ty) -> Self {
        match ty.kind() {
            TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_) => {
                TyInvKind::Trivial
            }
            TyKind::Ref(_, ty, Mutability::Not) => TyInvKind::from_ty(*ty),
            TyKind::Ref(_, _, Mutability::Mut) => TyInvKind::Borrow,
            TyKind::Adt(adt_def, adt_substs) if adt_def.is_box() => {
                TyInvKind::from_ty(adt_substs.type_at(0))
            }
            TyKind::Adt(adt_def, _) => TyInvKind::Adt(adt_def.did()),
            TyKind::Tuple(tys) => TyInvKind::Tuple(tys.len()),
            _ => TyInvKind::Trivial, // TODO
        }
    }

    pub(crate) fn to_skeleton_ty<'tcx>(self, tcx: TyCtxt<'tcx>) -> Ty<'tcx> {
        match self {
            TyInvKind::Trivial => tcx.mk_ty_param(0, Symbol::intern("T")),
            TyInvKind::Borrow => {
                let re = tcx.lifetimes.re_erased;
                let ty = tcx.mk_ty_param(0, Symbol::intern("T"));
                tcx.mk_mut_ref(re, ty)
            }
            TyInvKind::Adt(did) => tcx.type_of(did).subst_identity(),
            TyInvKind::Tuple(arity) => tcx.mk_tup_from_iter(
                (0..arity).map(|i| tcx.mk_ty_param(i as _, Symbol::intern(&format!("T{i}")))),
            ),
        }
    }

    pub(crate) fn generics(self, tcx: TyCtxt) -> Vec<Ident> {
        match self {
            TyInvKind::Trivial | TyInvKind::Borrow => vec!["t".into()],
            TyInvKind::Adt(def_id) => ty_param_names(tcx, def_id).collect(),
            TyInvKind::Tuple(arity) => (0..arity).map(|i| format!["t{i}"].into()).collect(),
        }
    }
}

pub(crate) fn tyinv_substs<'tcx>(tcx: TyCtxt<'tcx>, ty: Ty<'tcx>) -> SubstsRef<'tcx> {
    match ty.kind() {
        TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_) => {
            tcx.mk_substs(&[GenericArg::from(ty)])
        }
        TyKind::Ref(_, ty, Mutability::Not) => tyinv_substs(tcx, *ty),
        TyKind::Ref(_, ty, Mutability::Mut) => tcx.mk_substs(&[GenericArg::from(*ty)]),
        TyKind::Adt(adt_def, adt_substs) if adt_def.is_box() => {
            tyinv_substs(tcx, adt_substs.type_at(0))
        }
        TyKind::Adt(_, adt_substs) => adt_substs,
        TyKind::Tuple(tys) => tcx.mk_substs_from_iter(tys.iter().map(GenericArg::from)),
        _ => tcx.mk_substs(&[GenericArg::from(ty)]),
    }
}

pub(crate) fn build_inv_module<'tcx>(
    ctx: &mut Why3Generator<'tcx>,
    inv_kind: TyInvKind,
) -> (Module, CloneSummary<'tcx>) {
    let mut names = CloneMap::new(ctx.tcx, TransId::TyInv(inv_kind), CloneLevel::Stub);
    let generics = inv_kind.generics(ctx.tcx);
    let inv_axiom = build_inv_axiom(ctx, &mut names, inv_kind);

    let mut decls = vec![];
    decls.extend(
        generics
            .into_iter()
            .map(|ty_name| Decl::TyDecl(TyDecl::Opaque { ty_name, ty_params: vec![] })),
    );

    let (clones, summary) = names.to_clones(ctx);
    decls.extend(clones);

    decls.push(Decl::Axiom(inv_axiom));

    (Module { name: util::inv_module_name(ctx.tcx, inv_kind), decls }, summary)
}

fn build_inv_axiom<'tcx>(
    ctx: &mut Why3Generator<'tcx>,
    names: &mut CloneMap<'tcx>,
    inv_kind: TyInvKind,
) -> Axiom {
    let name = match inv_kind {
        TyInvKind::Trivial => "inv_trivial".into(),
        TyInvKind::Borrow => "inv_borrow".into(),
        TyInvKind::Adt(did) => {
            let ty_name = util::item_name(ctx.tcx, did, Namespace::TypeNS);
            format!("inv_{}", &*ty_name).into()
        }
        TyInvKind::Tuple(arity) => format!("inv_tuple{arity}").into(),
    };

    let param_env =
        if let TyInvKind::Adt(did) = inv_kind { ctx.param_env(did) } else { ParamEnv::empty() };

    let ty = inv_kind.to_skeleton_ty(ctx.tcx);
    let lhs: Exp = Exp::impure_qvar(names.ty_inv(ty)).app_to(Exp::pure_var("self".into()));
    let rhs = if TyInvKind::Trivial == inv_kind {
        Exp::mk_true()
    } else {
        build_inv_exp(ctx, names, "self".into(), ty, param_env, true)
            .unwrap_or_else(|| Exp::mk_true())
    };

    let axiom = Exp::Forall(
        vec![("self".into(), translate_ty(ctx, names, DUMMY_SP, ty))],
        Box::new(lhs.eq(rhs)),
    );
    Axiom { name, axiom }
}

fn build_inv_exp<'tcx>(
    ctx: &mut Why3Generator<'tcx>,
    names: &mut CloneMap<'tcx>,
    ident: Ident,
    ty: Ty<'tcx>,
    param_env: ParamEnv<'tcx>,
    destruct_adt: bool,
) -> Option<Exp> {
    let ty = ctx.normalize_erasing_regions(param_env, ty);

    let user_inv = if destruct_adt {
        resolve_user_inv(ctx.tcx, ty, param_env).map(|(uinv_did, uinv_subst)| {
            let inv_name = names.value(uinv_did, uinv_subst);
            Exp::impure_qvar(inv_name).app(vec![Exp::pure_var(ident.clone())])
        })
    } else {
        None
    };

    let struct_inv = build_inv_exp_struct(ctx, names, ident, ty, param_env, destruct_adt);

    match (user_inv, struct_inv) {
        (None, None) => None,
        (Some(inv), None) | (None, Some(inv)) => Some(inv),
        (Some(user_inv), Some(struct_inv)) => Some(user_inv.log_and(struct_inv)),
    }
}

fn build_inv_exp_struct<'tcx>(
    ctx: &mut Why3Generator<'tcx>,
    names: &mut CloneMap<'tcx>,
    ident: Ident,
    ty: Ty<'tcx>,
    param_env: ParamEnv<'tcx>,
    destruct_adt: bool,
) -> Option<Exp> {
    match ty.kind() {
        TyKind::Ref(_, ty, Mutability::Not) => {
            build_inv_exp(ctx, names, ident, *ty, param_env, destruct_adt)
        }
        TyKind::Ref(_, ty, Mutability::Mut) if destruct_adt => {
            // cannot inline in ADTs, or else we might be missing
            // `use prelude.Borrow`

            // TODO include final value
            let deref = Exp::Current(Box::new(Exp::pure_var(ident)));
            let body = build_inv_exp(ctx, names, "a".into(), *ty, param_env, destruct_adt)?;
            Some(Exp::Let {
                pattern: Pattern::VarP("a".into()),
                arg: Box::new(deref),
                body: Box::new(body),
            })
        }
        TyKind::Tuple(tys) => {
            let fields: Vec<Ident> =
                tys.iter().enumerate().map(|(i, _)| format!("a_{i}").into()).collect();

            let body = tys
                .iter()
                .enumerate()
                .flat_map(|(i, t)| {
                    build_inv_exp(ctx, names, fields[i].clone(), t, param_env, destruct_adt)
                })
                .reduce(|e1, e2| e1.log_and(e2))?;

            let pattern = Pattern::TupleP(fields.into_iter().map(Pattern::VarP).collect());
            Some(Exp::Let { pattern, arg: Box::new(Exp::pure_var(ident)), body: Box::new(body) })
        }
        TyKind::Adt(adt_def, adt_subst) if adt_def.is_box() => {
            build_inv_exp(ctx, names, ident, adt_subst[0].expect_ty(), param_env, destruct_adt)
        }
        TyKind::Adt(adt_def, subst) if destruct_adt => {
            build_inv_exp_adt(ctx, names, ident, param_env, *adt_def, subst)
        }
        TyKind::Ref(_, _, _) | TyKind::Adt(_, _) | TyKind::Param(_) => {
            let inv_fun = Exp::impure_qvar(names.ty_inv(ty));
            Some(inv_fun.app_to(Exp::pure_var(ident)))
        }
        _ => None, // TODO add more cases
    }
}

fn build_inv_exp_adt<'tcx>(
    ctx: &mut Why3Generator<'tcx>,
    names: &mut CloneMap<'tcx>,
    ident: Ident,
    param_env: ParamEnv<'tcx>,
    adt_def: AdtDef,
    subst: SubstsRef<'tcx>,
) -> Option<Exp> {
    let mut branches = vec![];
    let mut trivial = true;

    for var_def in adt_def.variants().iter() {
        let tuple_var = var_def.ctor.is_some();

        let mut pats = vec![];
        let mut exp = Exp::mk_true();
        for (field_idx, field_def) in var_def.fields.iter().enumerate() {
            let field_name: Ident = if tuple_var {
                format!("a_{field_idx}").into()
            } else {
                field_def.name.as_str().into()
            };

            let field_ty = field_def.ty(ctx.tcx, subst);
            if let Some(field_inv) =
                build_inv_exp(ctx, names, field_name.clone(), field_ty, param_env, false)
            {
                pats.push(Pattern::VarP(field_name));
                exp = exp.log_and(field_inv);
                trivial = false;
            } else {
                pats.push(Pattern::Wildcard);
            }
        }

        let var_name = names.constructor(var_def.def_id, subst);
        branches.push((Pattern::ConsP(var_name, pats), exp));
    }

    (!trivial).then(|| Exp::Match(Box::new(Exp::pure_var(ident)), branches))
}

fn resolve_user_inv<'tcx>(
    tcx: TyCtxt<'tcx>,
    ty: Ty<'tcx>,
    param_env: ParamEnv<'tcx>,
) -> Option<(DefId, SubstsRef<'tcx>)> {
    let trait_did = tcx.get_diagnostic_item(Symbol::intern("creusot_invariant_user"))?;
    let default_did = tcx.get_diagnostic_item(Symbol::intern("creusot_invariant_user_default"))?;

    let (impl_did, subst) = traits::resolve_assoc_item_opt(
        tcx,
        param_env,
        trait_did,
        tcx.mk_substs(&[GenericArg::from(ty)]),
    )?;
    let subst = tcx.try_normalize_erasing_regions(param_env, subst).unwrap_or(subst);

    // if inv resolved to the default impl and is not specializable, ignore
    if impl_did == default_did && !traits::still_specializable(tcx, param_env, impl_did, subst) {
        None
    } else {
        Some((impl_did, subst))
    }
}
