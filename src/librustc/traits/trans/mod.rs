// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// This file contains various trait resolution methods used by trans.
// They all assume regions can be erased and monomorphic types.  It
// seems likely that they should eventually be merged into more
// general routines.

use dep_graph::{DepKind, DepTrackingMapConfig};
use std::marker::PhantomData;
use syntax_pos::DUMMY_SP;
use infer::InferCtxt;
use syntax_pos::Span;
use traits::{FulfillmentContext, Obligation, ObligationCause, SelectionContext, Vtable};
use ty::{self, Ty, TyCtxt};
use ty::subst::{Subst, Substs};
use ty::fold::TypeFoldable;

/// Attempts to resolve an obligation to a vtable.. The result is
/// a shallow vtable resolution -- meaning that we do not
/// (necessarily) resolve all nested obligations on the impl. Note
/// that type check should guarantee to us that all nested
/// obligations *could be* resolved if we wanted to.
/// Assumes that this is run after the entire crate has been successfully type-checked.
pub fn trans_fulfill_obligation<'a, 'tcx>(ty: TyCtxt<'a, 'tcx, 'tcx>,
                                          (param_env, trait_ref):
                                          (ty::ParamEnv<'tcx>, ty::PolyTraitRef<'tcx>))
                                          -> Vtable<'tcx, ()>
{
    // Remove any references to regions; this helps improve caching.
    let trait_ref = ty.erase_regions(&trait_ref);

    debug!("trans::fulfill_obligation(trait_ref={:?}, def_id={:?})",
            (param_env, trait_ref), trait_ref.def_id());

    // Do the initial selection for the obligation. This yields the
    // shallow result we are looking for -- that is, what specific impl.
    ty.infer_ctxt().enter(|infcx| {
        let mut selcx = SelectionContext::new(&infcx);

        let obligation_cause = ObligationCause::dummy();
        let obligation = Obligation::new(obligation_cause,
                                            param_env,
                                            trait_ref.to_poly_trait_predicate());

        let selection = match selcx.select(&obligation) {
            Ok(Some(selection)) => selection,
            Ok(None) => {
                // Ambiguity can happen when monomorphizing during trans
                // expands to some humongo type that never occurred
                // statically -- this humongo type can then overflow,
                // leading to an ambiguous result. So report this as an
                // overflow bug, since I believe this is the only case
                // where ambiguity can result.
                bug!("Encountered ambiguity selecting `{:?}` during trans, \
                        presuming due to overflow",
                        trait_ref)
            }
            Err(e) => {
                bug!("Encountered error `{:?}` selecting `{:?}` during trans",
                            e, trait_ref)
            }
        };

        debug!("fulfill_obligation: selection={:?}", selection);

        // Currently, we use a fulfillment context to completely resolve
        // all nested obligations. This is because they can inform the
        // inference of the impl's type parameters.
        let mut fulfill_cx = FulfillmentContext::new();
        let vtable = selection.map(|predicate| {
            debug!("fulfill_obligation: register_predicate_obligation {:?}", predicate);
            fulfill_cx.register_predicate_obligation(&infcx, predicate);
        });
        let vtable = infcx.drain_fulfillment_cx_or_panic(DUMMY_SP, &mut fulfill_cx, &vtable);

        info!("Cache miss: {:?} => {:?}", trait_ref, vtable);
        vtable
    })
}

impl<'a, 'tcx> TyCtxt<'a, 'tcx, 'tcx> {
    /// Monomorphizes a type from the AST by first applying the
    /// in-scope substitutions and then normalizing any associated
    /// types.
    pub fn subst_and_normalize_erasing_regions<T>(
        self,
        param_substs: &Substs<'tcx>,
        param_env: ty::ParamEnv<'tcx>,
        value: &T
    ) -> T
    where
        T: TypeFoldable<'tcx>,
    {
        debug!(
            "subst_and_normalize_erasing_regions(\
             param_substs={:?}, \
             value={:?}, \
             param_env={:?})",
            param_substs,
            value,
            param_env,
        );
        let substituted = value.subst(self, param_substs);
        self.normalize_erasing_regions(param_env, substituted)
    }
}

// Implement DepTrackingMapConfig for `trait_cache`
pub struct TraitSelectionCache<'tcx> {
    data: PhantomData<&'tcx ()>
}

impl<'tcx> DepTrackingMapConfig for TraitSelectionCache<'tcx> {
    type Key = (ty::ParamEnv<'tcx>, ty::PolyTraitRef<'tcx>);
    type Value = Vtable<'tcx, ()>;
    fn to_dep_kind() -> DepKind {
        DepKind::TraitSelect
    }
}

// # Global Cache

pub struct ProjectionCache<'gcx> {
    data: PhantomData<&'gcx ()>
}

impl<'gcx> DepTrackingMapConfig for ProjectionCache<'gcx> {
    type Key = Ty<'gcx>;
    type Value = Ty<'gcx>;
    fn to_dep_kind() -> DepKind {
        DepKind::TraitSelect
    }
}

impl<'a, 'gcx, 'tcx> InferCtxt<'a, 'gcx, 'tcx> {
    /// Finishes processes any obligations that remain in the
    /// fulfillment context, and then returns the result with all type
    /// variables removed and regions erased. Because this is intended
    /// for use after type-check has completed, if any errors occur,
    /// it will panic. It is used during normalization and other cases
    /// where processing the obligations in `fulfill_cx` may cause
    /// type inference variables that appear in `result` to be
    /// unified, and hence we need to process those obligations to get
    /// the complete picture of the type.
    fn drain_fulfillment_cx_or_panic<T>(&self,
                                        span: Span,
                                        fulfill_cx: &mut FulfillmentContext<'tcx>,
                                        result: &T)
                                        -> T::Lifted
        where T: TypeFoldable<'tcx> + ty::Lift<'gcx>
    {
        debug!("drain_fulfillment_cx_or_panic()");

        // In principle, we only need to do this so long as `result`
        // contains unbound type parameters. It could be a slight
        // optimization to stop iterating early.
        match fulfill_cx.select_all_or_error(self) {
            Ok(()) => { }
            Err(errors) => {
                span_bug!(span, "Encountered errors `{:?}` resolving bounds after type-checking",
                          errors);
            }
        }

        let result = self.resolve_type_vars_if_possible(result);
        let result = self.tcx.erase_regions(&result);

        match self.tcx.lift_to_global(&result) {
            Some(result) => result,
            None => {
                span_bug!(span, "Uninferred types/regions in `{:?}`", result);
            }
        }
    }
}
