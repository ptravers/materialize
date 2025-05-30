// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// Clippy's cognitive complexity is easy to reach.
//#![allow(clippy::cognitive_complexity)]

//! Determines the join implementation for join operators.
//!
//! This includes determining the type of join (e.g. differential linear, or delta queries),
//! determining the orders of collections, lifting predicates if useful arrangements exist,
//! and identifying opportunities to use indexes to replace filters.

use std::collections::BTreeMap;

use mz_expr::JoinImplementation::{Differential, IndexedFilter, Unimplemented};
use mz_expr::visit::{Visit, VisitChildren};
use mz_expr::{
    FilterCharacteristics, Id, JoinInputCharacteristics, JoinInputMapper, MapFilterProject,
    MirRelationExpr, MirScalarExpr, RECURSION_LIMIT,
};
use mz_ore::stack::{CheckedRecursion, RecursionGuard};
use mz_ore::{soft_assert_or_log, soft_panic_or_log};
use mz_repr::optimize::OptimizerFeatures;

use crate::analysis::{Cardinality, DerivedBuilder};
use crate::join_implementation::index_map::IndexMap;
use crate::predicate_pushdown::PredicatePushdown;
use crate::{StatisticsOracle, TransformCtx, TransformError};

/// Determines the join implementation for join operators.
#[derive(Debug)]
pub struct JoinImplementation {
    recursion_guard: RecursionGuard,
}

impl Default for JoinImplementation {
    /// Construct a new [`JoinImplementation`] where `recursion_guard`
    /// is initialized with [`RECURSION_LIMIT`] as limit.
    fn default() -> JoinImplementation {
        JoinImplementation {
            recursion_guard: RecursionGuard::with_limit(RECURSION_LIMIT),
        }
    }
}

impl CheckedRecursion for JoinImplementation {
    fn recursion_guard(&self) -> &RecursionGuard {
        &self.recursion_guard
    }
}

impl crate::Transform for JoinImplementation {
    fn name(&self) -> &'static str {
        "JoinImplementation"
    }

    #[mz_ore::instrument(
        target = "optimizer",
        level = "debug",
        fields(path.segment = "join_implementation")
    )]
    fn actually_perform_transform(
        &self,
        relation: &mut MirRelationExpr,
        ctx: &mut TransformCtx,
    ) -> Result<(), TransformError> {
        let result = self.action_recursive(
            relation,
            &mut IndexMap::new(ctx.indexes),
            ctx.stats,
            ctx.features,
        );
        mz_repr::explain::trace_plan(&*relation);
        result
    }
}

impl JoinImplementation {
    /// Pre-order visitor for each `MirRelationExpr` to find join operators.
    ///
    /// This method accumulates state about let-bound arrangements, so that
    /// join operators can more accurately assess their available arrangements.
    pub fn action_recursive(
        &self,
        relation: &mut MirRelationExpr,
        indexes: &mut IndexMap,
        stats: &dyn StatisticsOracle,
        features: &OptimizerFeatures,
    ) -> Result<(), TransformError> {
        self.checked_recur(|_| {
            if let MirRelationExpr::Let { id, value, body } = relation {
                self.action_recursive(value, indexes, stats, features)?;
                match &**value {
                    MirRelationExpr::ArrangeBy { keys, .. } => {
                        for key in keys {
                            indexes.add_local(*id, key.clone());
                        }
                    }
                    MirRelationExpr::Reduce { group_key, .. } => {
                        indexes.add_local(
                            *id,
                            (0..group_key.len()).map(MirScalarExpr::column).collect(),
                        );
                    }
                    _ => {}
                }
                self.action_recursive(body, indexes, stats, features)?;
                indexes.remove_local(*id);
                Ok(())
            } else {
                let (mfp, mfp_input) =
                    MapFilterProject::extract_non_errors_from_expr_ref_mut(relation);
                mfp_input.try_visit_mut_children(|e| {
                    self.action_recursive(e, indexes, stats, features)
                })?;
                self.action(mfp_input, mfp, indexes, stats, features)?;
                Ok(())
            }
        })
    }

    /// Determines the join implementation for join operators.
    pub fn action(
        &self,
        relation: &mut MirRelationExpr,
        mfp_above: MapFilterProject,
        indexes: &IndexMap,
        stats: &dyn StatisticsOracle,
        features: &OptimizerFeatures,
    ) -> Result<(), TransformError> {
        if let MirRelationExpr::Join {
            inputs,
            equivalences,
            // (Note that `JoinImplementation` runs in a fixpoint loop.)
            // If the current implementation is
            //  - Unimplemented, then we need to come up with an implementation.
            //  - Differential, then we consider switching to a Delta join, because we might have
            //    inserted some ArrangeBys that create new arrangements when we came up with the
            //    Differential plan, in which case a Delta join might have become viable.
            //  - Delta, then we are good already.
            //  - IndexedFilter, then we just leave that alone, because those are out of scope
            //    for JoinImplementation (they are created by `LiteralConstraints`).
            // We don't want to change from a Differential plan to an other Differential plan, or
            // from a Delta plan to an other Delta plan, because the second run cannot distinguish
            // between an ArrangeBy that marks an already existing arrangement and an ArrangeBy
            // that was inserted by a previous run of JoinImplementation. (We should eventually
            // refactor this to make ArrangeBy unambiguous somehow. Maybe move JoinImplementation
            // to the lowering.)
            implementation: implementation @ (Unimplemented | Differential(..)),
        } = relation
        {
            // If we eagerly plan delta joins, we don't need the second run to "pick up" delta joins
            // that could be planned with the arrangements from a differential. If such a delta
            // join were viable, we'd have already planned it the first time.
            if features.enable_eager_delta_joins && !matches!(implementation, Unimplemented) {
                return Ok(());
            }

            let input_types = inputs.iter().map(|i| i.typ()).collect::<Vec<_>>();

            // Canonicalize the equivalence classes
            if matches!(implementation, Unimplemented) {
                // Let's do this only if it's the first run of JoinImplementation, in which case we
                // are guaranteed to produce a new plan, which will be compatible with the modified
                // equivalences from the below call. Otherwise, if we already have a Differential or
                // a Delta join, then we might discard the new plan and go with the old plan, which
                // was created previously for the old equivalences, and might be invalid for the
                // modified equivalences from the below call. Note that this issue can arise only if
                // `canonicalize_equivalences` is not idempotent, which unfortunately seems to be
                // the case.
                mz_expr::canonicalize::canonicalize_equivalences(
                    equivalences,
                    input_types.iter().map(|t| &t.column_types),
                );
            }

            // Common information of broad utility.
            let input_mapper = JoinInputMapper::new_from_input_types(&input_types);

            // The first fundamental question is whether we should employ a delta query or not.
            //
            // Here we conservatively use the rule that if sufficient arrangements exist we will
            // use a delta query (except for 2-input joins). (With eager delta joins, we will
            // settle for fewer arrangements in the delta join than in the differential join.)
            //
            // An arrangement is considered available for an input
            // - if it is a `Get` with columns present in `indexes`,
            //   - or the same wrapped by an IndexedFilter,
            // - if it is an `ArrangeBy` with the columns present (note that the ArrangeBy might
            //   have been inserted by a previous run of JoinImplementation),
            // - if it is a `Reduce` whose output is arranged the right way,
            // - if it is a filter wrapped around either of these (see the mfp extraction).
            //
            // The `IndexedFilter` case above is to avoid losing some Delta joins
            // due to `IndexedFilter` on a join input. This means that in the absolute worst
            // case (when the `IndexedFilter` doesn't filter out anything), we will fully
            // re-create some arrangements that we already have for that input. This worst case
            // is still better than what can happen if we lose a Delta join: Differential joins
            // will create several new arrangements that doesn't even have a size bound, i.e.,
            // they might be larger than any user-created index.

            let unique_keys = input_types
                .into_iter()
                .map(|typ| typ.keys)
                .collect::<Vec<_>>();
            let mut available_arrangements = vec![Vec::new(); inputs.len()];
            let mut filters = Vec::with_capacity(inputs.len());
            let mut cardinalities = Vec::with_capacity(inputs.len());

            // We figure out what predicates from mfp_above could be pushed to which input.
            // We won't actually push these down now; this just informs FilterCharacteristics.
            let (map, mut filter, _) = mfp_above.as_map_filter_project();
            let all_errors = filter.iter().all(|p| p.is_literal_err());
            let (_, pushed_through_map) = PredicatePushdown::push_filters_through_map(
                &map,
                &mut filter,
                mfp_above.input_arity,
                all_errors,
            )?;
            let (_, push_downs) = PredicatePushdown::push_filters_through_join(
                &input_mapper,
                equivalences,
                pushed_through_map,
            );

            for index in 0..inputs.len() {
                // We can work around mfps, as we can lift the mfps into the join execution.
                let (mfp, input) = MapFilterProject::extract_non_errors_from_expr(&inputs[index]);
                let (_, filter, project) = mfp.as_map_filter_project();

                // We gather filter characteristics:
                // - From the filter that is directly at the top mfp of the input.
                // - IndexedFilter joins are constructed from literal equality filters.
                // - If the input is an ArrangeBy, then we gather filter characteristics from
                //   the mfp below the ArrangeBy. (JoinImplementation often inserts ArrangeBys.)
                // - From filters that could be pushed down from above the join to this input.
                //   (In LIR, these will be executed right after the join path executes the join
                //   for this input.)
                // - (No need to look behind Gets, see the inline_mfp argument of RelationCSE.)
                let mut characteristics = FilterCharacteristics::filter_characteristics(&filter)?;
                if matches!(
                    input,
                    MirRelationExpr::Join {
                        implementation: IndexedFilter(..),
                        ..
                    }
                ) {
                    characteristics.add_literal_equality();
                }
                if let MirRelationExpr::ArrangeBy {
                    input: arrange_by_input,
                    ..
                } = input
                {
                    let (mfp, input) =
                        MapFilterProject::extract_non_errors_from_expr(arrange_by_input);
                    let (_, filter, _) = mfp.as_map_filter_project();
                    characteristics |= FilterCharacteristics::filter_characteristics(&filter)?;
                    if matches!(
                        input,
                        MirRelationExpr::Join {
                            implementation: IndexedFilter(..),
                            ..
                        }
                    ) {
                        characteristics.add_literal_equality();
                    }
                }
                let push_down_characteristics =
                    FilterCharacteristics::filter_characteristics(&push_downs[index])?;
                let push_down_factor = push_down_characteristics.worst_case_scaling_factor();
                characteristics |= push_down_characteristics;

                // Estimate cardinality
                if features.enable_cardinality_estimates {
                    let mut builder = DerivedBuilder::new(features);
                    // TODO(mgree): it would be good to not have to copy the statistics here
                    builder.require(Cardinality::with_stats(stats.as_map()));
                    let derived = builder.visit(input);

                    let estimate = *derived.as_view().value::<Cardinality>().unwrap();
                    // we've already accounted for the filters _in_ the term; these capture the ones above
                    let scaled = estimate * push_down_factor;
                    cardinalities.push(scaled.rounded());
                } else {
                    cardinalities.push(None);
                }

                filters.push(characteristics);

                // Collect available arrangements on this input.
                match input {
                    MirRelationExpr::Get { id, typ: _, .. } => {
                        available_arrangements[index]
                            .extend(indexes.get(*id).map(|key| key.to_vec()));
                    }
                    MirRelationExpr::ArrangeBy { input, keys } => {
                        // We may use any presented arrangement keys.
                        available_arrangements[index].extend(keys.clone());
                        if let MirRelationExpr::Get { id, typ: _, .. } = &**input {
                            available_arrangements[index]
                                .extend(indexes.get(*id).map(|key| key.to_vec()));
                        }
                    }
                    MirRelationExpr::Reduce { group_key, .. } => {
                        // The first `group_key.len()` columns form an arrangement key.
                        available_arrangements[index]
                            .push((0..group_key.len()).map(MirScalarExpr::column).collect());
                    }
                    MirRelationExpr::Join {
                        implementation: IndexedFilter(id, ..),
                        ..
                    } => {
                        available_arrangements[index]
                            .extend(indexes.get(Id::Global(id.clone())).map(|key| key.to_vec()));
                    }
                    _ => {}
                }
                available_arrangements[index].sort();
                available_arrangements[index].dedup();
                let reverse_project = project
                    .into_iter()
                    .enumerate()
                    .map(|(i, c)| (c, i))
                    .collect::<BTreeMap<_, _>>();
                // Eliminate arrangements referring to columns that have been
                // projected away by surrounding MFPs.
                available_arrangements[index].retain(|key| {
                    key.iter()
                        .all(|k| k.support().iter().all(|c| reverse_project.contains_key(c)))
                });
                // Permute arrangements so columns reference what is after the MFP.
                for key in available_arrangements[index].iter_mut() {
                    for k in key.iter_mut() {
                        k.permute_map(&reverse_project);
                    }
                }
                // Currently we only support using arrangements all of whose
                // key components can be found in some equivalence.
                // Note: because `order_input` currently only finds arrangements
                // with exact key matches, the code below can be removed with no
                // change in behavior, but this is being kept for a future
                // TODO: expand `order_input`
                available_arrangements[index].retain(|key| {
                    key.iter().all(|k| {
                        let k = input_mapper.map_expr_to_global(k.clone(), index);
                        equivalences
                            .iter()
                            .any(|equivalence| equivalence.contains(&k))
                    })
                });
            }

            let old_implementation = implementation.clone();
            let num_inputs = inputs.len();
            // We've already planned a differential join... should we replace it with a delta join?
            //
            // This code path is only active when `eager_delta_joins` is false.
            if matches!(old_implementation, Differential(..)) {
                soft_assert_or_log!(
                    !features.enable_eager_delta_joins,
                    "eager delta joins run join implementation just once"
                );

                // Binary joins can't be delta joins---give up.
                if inputs.len() <= 2 {
                    return Ok(());
                }

                // Only plan a delta join if it's no new arrangements (beyond what differential planned).
                if let Ok((delta_query_plan, 0)) = delta_queries::plan(
                    relation,
                    &input_mapper,
                    &available_arrangements,
                    &unique_keys,
                    &cardinalities,
                    &filters,
                    features,
                ) {
                    tracing::debug!(plan = ?delta_query_plan, "replacing differential join with delta join");
                    *relation = delta_query_plan;
                }

                return Ok(());
            }

            // To have reached here, we must be in our first run of join planning.
            //
            // We plan a differential join first.
            let (differential_query_plan, differential_new_arrangements) = differential::plan(
                relation,
                &input_mapper,
                &available_arrangements,
                &unique_keys,
                &cardinalities,
                &filters,
                features,
            )
            .expect("Failed to produce a differential join plan");

            // Binary joins _must_ be differential. We won't plan a delta join.
            if num_inputs <= 2 {
                // if inputs.len() == 0 then something is very wrong.
                soft_assert_or_log!(num_inputs != 0, "join with no inputs");
                // if inputs.len() == 1:
                // Single input joins are filters and should be planned as
                // differential plans instead of delta queries. Because a
                // a filter gets converted into a single input join only when
                // there are existing arrangements, without this early return,
                // filters will always be planned as delta queries.
                // Note: This can actually occur, see github-24511.slt.
                //
                // if inputs.len() == 2:
                // We decided to always plan this as a differential join for now, because the usual
                // advantage of a Delta join avoiding intermediate arrangements doesn't apply.
                // See more details here:
                // https://github.com/MaterializeInc/materialize/pull/16099#issuecomment-1316857374
                // https://github.com/MaterializeInc/materialize/pull/17708#discussion_r1112848747
                *relation = differential_query_plan;

                return Ok(());
            }

            // We are planning a multiway join for the first time.
            //
            // We compare the delta and differential join plans.
            //
            // A delta query requires that, for every path, there is an arrangement for every
            // input except for the starting one. Such queries are viable when:
            //
            //   (a) all the arrangements already exist, or
            //   (b) both:
            //       (i) we wouldn't create more arrangements than a differential join would
            //       (ii) `enable_eager_delta_joins` is on
            //
            // A differential join of k relations requires k-2 arrangements of intermediate
            // results (plus k arrangements of the inputs).
            //
            // Consider A ⨝ B ⨝ C ⨝ D. If planned as a differential join, we might have:
            //          A » B » C » D
            // This corresponds to the tree:
            //
            // A   B
            //  \ /
            //   ⨝   C
            //    \ /
            //     ⨝   D
            //      \ /
            //       ⨝
            //
            // At the two internal joins, the differential join will need two new arrangements.
            //
            // TODO(mgree): with this refactoring, we should compute `orders` once---both joins
            //              call `optimize_orders` and we can save some work.
            match delta_queries::plan(
                relation,
                &input_mapper,
                &available_arrangements,
                &unique_keys,
                &cardinalities,
                &filters,
                features,
            ) {
                // If delta plan's inputs need no new arrangements, pick the delta plan.
                Ok((delta_query_plan, 0)) => {
                    soft_assert_or_log!(
                        matches!(old_implementation, Unimplemented | Differential(..)),
                        "delta query plans should not be planned twice"
                    );
                    tracing::debug!(
                        plan = ?delta_query_plan,
                        differential_new_arrangements = differential_new_arrangements,
                        "picking delta query plan (no new arrangements)");
                    *relation = delta_query_plan;
                }
                // If the delta plan needs new arrangements, compare with the differential plan.
                Ok((delta_query_plan, delta_new_arrangements)) => {
                    tracing::debug!(
                        delta_new_arrangements = delta_new_arrangements,
                        differential_new_arrangements = differential_new_arrangements,
                        "comparing delta and differential joins",
                    );

                    if features.enable_eager_delta_joins
                        && delta_new_arrangements <= differential_new_arrangements
                    {
                        // If we're eagerly planning delta joins, pick the delta plan if it's more economical.
                        tracing::debug!(
                            plan = ?delta_query_plan,
                            "picking delta query plan");
                        *relation = delta_query_plan;
                    } else if let Unimplemented = old_implementation {
                        // If we haven't planned the join yet, use the differential plan.
                        tracing::debug!(
                            plan = ?differential_query_plan,
                            "picking differential query plan");
                        *relation = differential_query_plan;
                    } else {
                        // But don't replace an existing differential plan.
                        tracing::debug!(plan = ?old_implementation, "keeping old plan");
                        soft_assert_or_log!(
                            matches!(old_implementation, Differential(..)),
                            "implemented plan in second run of join implementation should be differential \
                             if the delta plan is not viable"
                        )
                    }
                }
                // If we can't plan a delta join, plan a differential join.
                Err(err) => {
                    soft_panic_or_log!("delta planning failed: {err}");
                    tracing::debug!(
                        plan = ?differential_query_plan,
                        "picking differential query plan (delta planning failed)");
                    *relation = differential_query_plan;
                }
            }
        }
        Ok(())
    }
}

mod index_map {
    use std::collections::BTreeMap;

    use mz_expr::{Id, LocalId, MirScalarExpr};

    use crate::IndexOracle;

    /// Keeps track of local and global indexes available while descending
    /// a `MirRelationExpr`.
    #[derive(Debug)]
    pub struct IndexMap<'a> {
        local: BTreeMap<LocalId, Vec<Vec<MirScalarExpr>>>,
        global: &'a dyn IndexOracle,
    }

    impl IndexMap<'_> {
        /// Creates a new index map with knowledge of the provided global indexes.
        pub fn new(global: &dyn IndexOracle) -> IndexMap {
            IndexMap {
                local: BTreeMap::new(),
                global,
            }
        }

        /// Adds a local index on the specified collection with the specified key.
        pub fn add_local(&mut self, id: LocalId, key: Vec<MirScalarExpr>) {
            self.local.entry(id).or_default().push(key)
        }

        /// Removes all local indexes on the specified collection.
        pub fn remove_local(&mut self, id: LocalId) {
            self.local.remove(&id);
        }

        pub fn get(&self, id: Id) -> Box<dyn Iterator<Item = &[MirScalarExpr]> + '_> {
            match id {
                Id::Global(id) => Box::new(self.global.indexes_on(id).map(|(_idx_id, key)| key)),
                Id::Local(id) => Box::new(
                    self.local
                        .get(&id)
                        .into_iter()
                        .flatten()
                        .map(|x| x.as_slice()),
                ),
            }
        }
    }
}

mod delta_queries {

    use std::collections::BTreeSet;

    use mz_expr::{
        FilterCharacteristics, JoinImplementation, JoinInputMapper, MirRelationExpr, MirScalarExpr,
    };
    use mz_repr::optimize::OptimizerFeatures;

    use crate::TransformError;

    /// Creates a delta query plan, and any predicates that need to be lifted.
    /// It also returns the number of new arrangements necessary for this plan.
    ///
    /// The method returns `Err` if any errors occur during planning.
    pub fn plan(
        join: &MirRelationExpr,
        input_mapper: &JoinInputMapper,
        available: &[Vec<Vec<MirScalarExpr>>],
        unique_keys: &[Vec<Vec<usize>>],
        cardinalities: &[Option<usize>],
        filters: &[FilterCharacteristics],
        optimizer_features: &OptimizerFeatures,
    ) -> Result<(MirRelationExpr, usize), TransformError> {
        let mut new_join = join.clone();

        if let MirRelationExpr::Join {
            inputs,
            equivalences,
            implementation,
        } = &mut new_join
        {
            // Determine a viable order for each relation, or return `Err` if none found.
            let orders = super::optimize_orders(
                equivalences,
                available,
                unique_keys,
                cardinalities,
                filters,
                input_mapper,
                optimizer_features.enable_join_prioritize_arranged,
            )?;

            // Count new arrangements.
            let new_arrangements: usize = orders
                .iter()
                .flat_map(|o| {
                    o.iter().skip(1).filter_map(|(c, key, input)| {
                        if c.arranged() {
                            None
                        } else {
                            Some((input, key))
                        }
                    })
                })
                .collect::<BTreeSet<_>>()
                .len();

            // Convert the order information into specific (input, key, characteristics) information.
            let mut orders = orders
                .into_iter()
                .map(|o| {
                    o.into_iter()
                        .skip(1)
                        .map(|(c, key, r)| (r, key, Some(c)))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();

            // Implement arrangements in each of the inputs.
            let (lifted_mfp, lifted_projections) =
                super::implement_arrangements(inputs, available, orders.iter().flatten());

            // Permute `order` to compensate for projections being lifted as part of
            // the mfp lifting in `implement_arrangements`.
            orders
                .iter_mut()
                .for_each(|order| super::permute_order(order, &lifted_projections));

            *implementation = JoinImplementation::DeltaQuery(orders);

            super::install_lifted_mfp(&mut new_join, lifted_mfp)?;

            // Hooray done!
            Ok((new_join, new_arrangements))
        } else {
            Err(TransformError::Internal(String::from(
                "delta_queries::plan call on non-join expression",
            )))
        }
    }
}

mod differential {
    use std::collections::BTreeSet;

    use mz_expr::{JoinImplementation, JoinInputMapper, MirRelationExpr, MirScalarExpr};
    use mz_ore::soft_assert_eq_or_log;
    use mz_repr::optimize::OptimizerFeatures;

    use crate::TransformError;
    use crate::join_implementation::FilterCharacteristics;

    /// Creates a linear differential plan, and any predicates that need to be lifted.
    /// It also returns the number of new arrangements necessary for this plan.
    pub fn plan(
        join: &MirRelationExpr,
        input_mapper: &JoinInputMapper,
        available: &[Vec<Vec<MirScalarExpr>>],
        unique_keys: &[Vec<Vec<usize>>],
        cardinalities: &[Option<usize>],
        filters: &[FilterCharacteristics],
        optimizer_features: &OptimizerFeatures,
    ) -> Result<(MirRelationExpr, usize), TransformError> {
        let mut new_join = join.clone();

        if let MirRelationExpr::Join {
            inputs,
            equivalences,
            implementation,
        } = &mut new_join
        {
            // We compute one order for each possible starting point, and we will choose one from
            // these.
            //
            // It is an invariant that the orders are in input order: the ith order begins with the ith input.
            //
            // We could change this preference at any point, but the list of orders should still inform.
            // Important, we should choose something stable under re-ordering, to converge under fixed
            // point iteration; we choose to start with the first input optimizing our criteria, which
            // should remain stable even when promoted to the first position.
            let mut orders = super::optimize_orders(
                equivalences,
                available,
                unique_keys,
                cardinalities,
                filters,
                input_mapper,
                optimizer_features.enable_join_prioritize_arranged,
            )?;

            // Count new arrangements.
            //
            // We collect the count for each input, to be used to calculate `new_arrangements` below.
            let new_input_arrangements: Vec<usize> = orders
                .iter()
                .map(|o| {
                    o.iter()
                        .filter_map(|(c, key, input)| {
                            if c.arranged() {
                                None
                            } else {
                                Some((*input, key.clone()))
                            }
                        })
                        .collect::<BTreeSet<_>>()
                        .len()
                })
                .collect();

            // Inside each order, we take the `FilterCharacteristics` from each element, and OR it
            // to every other element to the right. This is because we are gonna be looking for the
            // worst `Characteristic` in every order, and for this it makes sense to include a
            // filter in a `Characteristic` if the filter was applied not just at that input but
            // any input before. For examples, see chbench.slt Query 02 and 11.
            orders.iter_mut().for_each(|order| {
                let mut sum = FilterCharacteristics::none();
                for (jic, _, _) in order {
                    *jic.filters() |= sum;
                    sum = jic.filters().clone();
                }
            });

            // `orders` has one order for each starting collection, and now we have to choose one
            // from these. First, we find the worst `Characteristics` inside each order, and then we
            // find the best one among these across all orders, which goes into
            // `max_min_characteristics`.
            let max_min_characteristics = orders
                .iter()
                .flat_map(|order| order.iter().map(|(c, _, _)| c.clone()).min())
                .max();
            let mut order = if let Some(max_min_characteristics) = max_min_characteristics {
                orders
                    .into_iter()
                    .filter(|o| {
                        o.iter().map(|(c, _, _)| c).min().unwrap() == &max_min_characteristics
                    })
                    // It can happen that `orders` has multiple such orders that have the same worst
                    // `Characteristic` as `max_min_characteristics`. In this case, we go beyond the
                    // worst `Characteristic`: we inspect the entire `Characteristic` vector of each
                    // of these orders, and choose the best among these. This pushes bad stuff to
                    // happen later, by which time we might have applied some filters.
                    .max_by_key(|o| o.clone())
                    .ok_or_else(|| {
                        TransformError::Internal(String::from(
                            "could not find max-min characteristics",
                        ))
                    })?
                    .into_iter()
                    .map(|(c, key, r)| (r, key, Some(c)))
                    .collect::<Vec<_>>()
            } else {
                // if max_min_characteristics is None, then there must only be
                // one input and thus only one order in orders
                soft_assert_eq_or_log!(orders.len(), 1);
                orders
                    .remove(0)
                    .into_iter()
                    .map(|(c, key, r)| (r, key, Some(c)))
                    .collect::<Vec<_>>()
            };

            let (start, mut start_key, start_characteristics) = order[0].clone();

            // Count new arrangements for this choice of ordering.
            let new_arrangements = inputs.len().saturating_sub(2) + new_input_arrangements[start];

            // Implement arrangements in each of the inputs.
            let (lifted_mfp, lifted_projections) =
                super::implement_arrangements(inputs, available, order.iter());

            // Permute `start_key` and `order` to compensate for projections being lifted as part of
            // the mfp lifting in `implement_arrangements`.
            if let Some(proj) = &lifted_projections[start] {
                start_key.iter_mut().for_each(|k| {
                    k.permute(proj);
                });
            }
            super::permute_order(&mut order, &lifted_projections);

            // now that the starting arrangement has been implemented,
            // remove it from `order` so `order` only contains information
            // about the other inputs
            order.remove(0);

            // Install the implementation.
            *implementation = JoinImplementation::Differential(
                (start, Some(start_key), start_characteristics),
                order,
            );

            super::install_lifted_mfp(&mut new_join, lifted_mfp)?;

            // Hooray done!
            Ok((new_join, new_arrangements))
        } else {
            Err(TransformError::Internal(String::from(
                "differential::plan call on non-join expression.",
            )))
        }
    }
}

/// Modify `inputs` to ensure specified arrangements are available.
///
/// Lift filter predicates when all needed arrangements are otherwise available.
///
/// Returns
///  - The lifted mfps combined into one mfp.
///  - Permutations for each input, which were lifted as part of the mfp lifting. These should be
///    applied to the join order.
fn implement_arrangements<'a>(
    inputs: &mut [MirRelationExpr],
    available_arrangements: &[Vec<Vec<MirScalarExpr>>],
    needed_arrangements: impl Iterator<
        Item = &'a (usize, Vec<MirScalarExpr>, Option<JoinInputCharacteristics>),
    >,
) -> (MapFilterProject, Vec<Option<Vec<usize>>>) {
    // Collect needed arrangements by source index.
    let mut needed = vec![Vec::new(); inputs.len()];
    for (index, key, _characteristics) in needed_arrangements {
        needed[*index].push(key.clone());
    }

    let mut lifted_mfps = vec![None; inputs.len()];
    let mut lifted_projections = vec![None; inputs.len()];

    // Transform inputs[index] based on needed and available arrangements.
    // Specifically, lift intervening mfps if all arrangements exist.
    for (index, needed) in needed.iter_mut().enumerate() {
        needed.sort();
        needed.dedup();
        // We should lift any mfps, iff all arrangements are otherwise available.
        if !needed.is_empty()
            && needed
                .iter()
                .all(|key| available_arrangements[index].contains(key))
        {
            lifted_mfps[index] = Some(MapFilterProject::extract_non_errors_from_expr_mut(
                &mut inputs[index],
            ));
        }
        // Clean up existing arrangements, and install one with the needed keys.
        while let MirRelationExpr::ArrangeBy { input: inner, .. } = &mut inputs[index] {
            inputs[index] = inner.take_dangerous();
        }
        // If a mfp was lifted in order to install the arrangement, permute the arrangement and
        // save the lifted projection.
        if let Some(lifted_mfp) = &lifted_mfps[index] {
            let (_, _, project) = lifted_mfp.as_map_filter_project();
            for arrangement_key in needed.iter_mut() {
                for k in arrangement_key.iter_mut() {
                    k.permute(&project);
                }
            }
            lifted_projections[index] = Some(project);
        }
        if !needed.is_empty() {
            inputs[index] = MirRelationExpr::arrange_by(inputs[index].take_dangerous(), needed);
        }
    }

    // Combine lifted mfps into one.
    let new_join_mapper = JoinInputMapper::new(inputs);
    let mut arity = new_join_mapper.total_columns();
    let combined_mfp = MapFilterProject::new(arity);
    let mut combined_filter = Vec::new();
    let mut combined_map = Vec::new();
    let mut combined_project = Vec::new();
    for (index, lifted_mfp) in lifted_mfps.into_iter().enumerate() {
        if let Some(mut lifted_mfp) = lifted_mfp {
            let column_map = new_join_mapper
                .local_columns(index)
                .zip(new_join_mapper.global_columns(index))
                .collect::<BTreeMap<_, _>>();
            lifted_mfp.permute_fn(
                // globalize all input column references
                |c| column_map[&c],
                // shift the position of scalars to be after the last input
                // column
                arity,
            );
            let (mut map, mut filter, mut project) = lifted_mfp.as_map_filter_project();
            arity += map.len();
            combined_map.append(&mut map);
            combined_filter.append(&mut filter);
            combined_project.append(&mut project);
        } else {
            combined_project.extend(new_join_mapper.global_columns(index));
        }
    }

    (
        combined_mfp
            .map(combined_map)
            .filter(combined_filter)
            .project(combined_project),
        lifted_projections,
    )
}

/// This function continues the surgery that `implement_arrangements` started.
///
/// (In theory, this function could be merged into `implement_arrangements`, but it would be a bit
/// painful, because we need to access the join's `implementation` after `implement_arrangements`,
/// which would be somewhat convoluted after what `install_lifted_mfp` does.)
///
/// The given MFP should be the merged MFP from all the lifted inputs MFPs.
///
/// `install_lifted_mfp` mutates the given join expression as follows:
/// - Puts the given MFP on top of the Join.
/// - Attends to `equivalences`, which was invalidated by `implement_arrangements` if it refers to a
///   column that was permuted or created by the given MFP.
/// - Canonicalizes scalar expressions in maps and filters with respect to the join equivalences.
///   See inline comment for more details.
fn install_lifted_mfp(
    new_join: &mut MirRelationExpr,
    mfp: MapFilterProject,
) -> Result<(), TransformError> {
    if !mfp.is_identity() {
        let (mut map, mut filter, project) = mfp.as_map_filter_project();
        if let MirRelationExpr::Join { equivalences, .. } = new_join {
            for equivalence in equivalences.iter_mut() {
                for expr in equivalence.iter_mut() {
                    // permute `equivalences` in light of the project being lifted
                    expr.permute(&project);
                    // if column references refer to mapped expressions that have been
                    // lifted, replace the column reference with the mapped expression.
                    expr.visit_mut_pre(&mut |e| {
                        // This has to be a loop! This is because it can happen that the new
                        // expression is again a column reference, in which case the visitor
                        // wouldn't be called on it again. (The visitation continues with the
                        // children of the new expression, but won't visit the new expression
                        // itself again.)
                        while let MirScalarExpr::Column(c, _) = e {
                            if *c >= mfp.input_arity {
                                *e = map[*c - mfp.input_arity].clone();
                            } else {
                                break;
                            }
                        }
                    })?;
                }
            }
            // Canonicalize scalar expressions in maps and filters with respect to the join
            // equivalences. This often makes some filters identical, which are then removed.
            // The identical filters come from either
            //  - lifting several predicates that originally were pushed down by localizing to more
            //    than one inputs;
            //  - individual IS NOT NULL filters on each of the inputs, which become identical
            //    when rewritten using the join equivalences.
            //  (This allows for almost the same optimizations as when `Demand`
            //  used to insert Projections that were marking some columns to be
            //  identical, when Demand used to run after `JoinImplementation`.)
            let canonicalizer_map = mz_expr::canonicalize::get_canonicalizer_map(equivalences);
            for expr in map.iter_mut().chain(filter.iter_mut()) {
                expr.visit_mut_post(&mut |e| {
                    if let Some(canonical_expr) = canonicalizer_map.get(e) {
                        *e = canonical_expr.clone();
                    }
                })?
            }
        }
        *new_join = new_join.clone().map(map).filter(filter).project(project);
    }
    Ok(())
}

/// Permute the keys in `order` to compensate for projections being lifted from inputs.
/// `lifted_projections` has an optional projection for each input.
fn permute_order(
    order: &mut Vec<(usize, Vec<MirScalarExpr>, Option<JoinInputCharacteristics>)>,
    lifted_projections: &Vec<Option<Vec<usize>>>,
) {
    order.iter_mut().for_each(|(index, key, _)| {
        key.iter_mut().for_each(|kc| {
            if let Some(proj) = &lifted_projections[*index] {
                kc.permute(proj);
            }
        })
    })
}

// Computes the best join orders for each input.
//
// If there are N inputs, returns N orders, with the ith input starting the ith order.
fn optimize_orders(
    equivalences: &[Vec<MirScalarExpr>], // join equivalences: inside a Vec, the exprs are equivalent
    available: &[Vec<Vec<MirScalarExpr>>], // available arrangements per input
    unique_keys: &[Vec<Vec<usize>>],     // unique keys per input
    cardinalities: &[Option<usize>],     // cardinalities of input relations
    filters: &[FilterCharacteristics],   // filter characteristics per input
    input_mapper: &JoinInputMapper,      // join helper
    enable_join_prioritize_arranged: bool,
) -> Result<Vec<Vec<(JoinInputCharacteristics, Vec<MirScalarExpr>, usize)>>, TransformError> {
    let mut orderer = Orderer::new(
        equivalences,
        available,
        unique_keys,
        cardinalities,
        filters,
        input_mapper,
        enable_join_prioritize_arranged,
    );
    (0..available.len())
        .map(move |i| orderer.optimize_order_for(i))
        .collect::<Result<Vec<_>, _>>()
}

struct Orderer<'a> {
    inputs: usize,
    equivalences: &'a [Vec<MirScalarExpr>],
    arrangements: &'a [Vec<Vec<MirScalarExpr>>],
    unique_keys: &'a [Vec<Vec<usize>>],
    cardinalities: &'a [Option<usize>],
    filters: &'a [FilterCharacteristics],
    input_mapper: &'a JoinInputMapper,
    reverse_equivalences: Vec<Vec<(usize, usize)>>,
    unique_arrangement: Vec<Vec<bool>>,

    order: Vec<(JoinInputCharacteristics, Vec<MirScalarExpr>, usize)>,
    placed: Vec<bool>,
    bound: Vec<Vec<MirScalarExpr>>,
    equivalences_active: Vec<bool>,
    arrangement_active: Vec<Vec<usize>>,
    priority_queue:
        std::collections::BinaryHeap<(JoinInputCharacteristics, Vec<MirScalarExpr>, usize)>,

    enable_join_prioritize_arranged: bool,
}

impl<'a> Orderer<'a> {
    fn new(
        equivalences: &'a [Vec<MirScalarExpr>],
        arrangements: &'a [Vec<Vec<MirScalarExpr>>],
        unique_keys: &'a [Vec<Vec<usize>>],
        cardinalities: &'a [Option<usize>],
        filters: &'a [FilterCharacteristics],
        input_mapper: &'a JoinInputMapper,
        enable_join_prioritize_arranged: bool,
    ) -> Self {
        let inputs = arrangements.len();
        // A map from inputs to the equivalence classes in which they are referenced.
        let mut reverse_equivalences = vec![Vec::new(); inputs];
        for (index, equivalence) in equivalences.iter().enumerate() {
            for (index2, expr) in equivalence.iter().enumerate() {
                for input in input_mapper.lookup_inputs(expr) {
                    reverse_equivalences[input].push((index, index2));
                }
            }
        }
        // Per-arrangement information about uniqueness of the arrangement key.
        let mut unique_arrangement = vec![Vec::new(); inputs];
        for (input, keys) in arrangements.iter().enumerate() {
            for key in keys.iter() {
                unique_arrangement[input].push(unique_keys[input].iter().any(|cols| {
                    cols.iter()
                        .all(|c| key.contains(&MirScalarExpr::column(*c)))
                }));
            }
        }

        let order = Vec::with_capacity(inputs);
        let placed = vec![false; inputs];
        let bound = vec![Vec::new(); inputs];
        let equivalences_active = vec![false; equivalences.len()];
        let arrangement_active = vec![Vec::new(); inputs];
        let priority_queue = std::collections::BinaryHeap::new();
        Self {
            inputs,
            equivalences,
            arrangements,
            unique_keys,
            cardinalities,
            filters,
            input_mapper,
            reverse_equivalences,
            unique_arrangement,
            order,
            placed,
            bound,
            equivalences_active,
            arrangement_active,
            priority_queue,
            enable_join_prioritize_arranged,
        }
    }

    fn optimize_order_for(
        &mut self,
        start: usize,
    ) -> Result<Vec<(JoinInputCharacteristics, Vec<MirScalarExpr>, usize)>, TransformError> {
        self.order.clear();
        self.priority_queue.clear();
        for input in 0..self.inputs {
            self.placed[input] = false;
            self.bound[input].clear();
            self.arrangement_active[input].clear();
        }
        for index in 0..self.equivalences.len() {
            self.equivalences_active[index] = false;
        }

        // Introduce cross joins as a possibility.
        for input in 0..self.inputs {
            let cardinality = self.cardinalities[input];

            let is_unique = self.unique_keys[input].iter().any(|cols| cols.is_empty());
            if let Some(pos) = self.arrangements[input]
                .iter()
                .position(|key| key.is_empty())
            {
                self.arrangement_active[input].push(pos);
                self.priority_queue.push((
                    JoinInputCharacteristics::new(
                        is_unique,
                        0,
                        true,
                        cardinality,
                        self.filters[input].clone(),
                        input,
                        self.enable_join_prioritize_arranged,
                    ),
                    vec![],
                    input,
                ));
            } else {
                self.priority_queue.push((
                    JoinInputCharacteristics::new(
                        is_unique,
                        0,
                        false,
                        cardinality,
                        self.filters[input].clone(),
                        input,
                        self.enable_join_prioritize_arranged,
                    ),
                    vec![],
                    input,
                ));
            }
        }

        // Main loop, ordering all the inputs.
        if self.inputs > 1 {
            self.order_input(start);
            while self.order.len() < self.inputs - 1 {
                let (characteristics, key, input) = self.priority_queue.pop().unwrap();
                // put the tuple into `self.order` unless the tuple with the same
                // input is already in `self.order`. For all inputs other than
                // start, `self.placed[input]` is an indication of whether a
                // corresponding tuple is already in `self.order`.
                if !self.placed[input] {
                    // non-starting inputs are ordered in decreasing priority
                    self.order.push((characteristics, key, input));
                    self.order_input(input);
                }
            }
        }

        // `order` now contains all the inputs except the first. Let's create an item for the first
        // input. We know which input that is, but we need to compute a key and characteristics.
        // We start with some default values:
        let mut start_tuple = (
            JoinInputCharacteristics::new(
                false,
                0,
                false,
                self.cardinalities[start],
                self.filters[start].clone(),
                start,
                self.enable_join_prioritize_arranged,
            ),
            vec![],
            start,
        );
        // The key should line up with the key of the second input (if there is a second input).
        // (At this point, `order[0]` is what will eventually be `order[1]`, i.e., the second input.)
        if let Some((_, key, second)) = self.order.get(0) {
            // for each component of the key of the second input, try to find the corresponding key
            // component in the starting input
            let candidate_start_key = key
                .iter()
                .filter_map(|k| {
                    let k = self.input_mapper.map_expr_to_global(k.clone(), *second);
                    self.input_mapper
                        .find_bound_expr(&k, &[start], self.equivalences)
                        .map(|bound_key| self.input_mapper.map_expr_to_local(bound_key))
                })
                .collect::<Vec<_>>();
            if candidate_start_key.len() == key.len() {
                let cardinality = self.cardinalities[start];
                let is_unique = self.unique_keys[start].iter().any(|cols| {
                    cols.iter()
                        .all(|c| candidate_start_key.contains(&MirScalarExpr::column(*c)))
                });
                let arranged = self.arrangements[start]
                    .iter()
                    .find(|arrangement_key| arrangement_key == &&candidate_start_key)
                    .is_some();
                start_tuple = (
                    JoinInputCharacteristics::new(
                        is_unique,
                        candidate_start_key.len(),
                        arranged,
                        cardinality,
                        self.filters[start].clone(),
                        start,
                        self.enable_join_prioritize_arranged,
                    ),
                    candidate_start_key,
                    start,
                );
            } else {
                // For the second input's key fields, there is nothing else to equate it with but
                // the fields of the first input, so we should find a match for each of the fields.
                // (For a later input, different fields of a key might be equated with fields coming
                // from various inputs.)
                // Technically, this happens as follows:
                // The second input must have been placed in the `priority_queue` either
                // 1) as a cross join possibility, or
                // 2) when we called `order_input` on the starting input.
                // In the 1) case, `key.len()` is 0. In the 2) case, it was the very first call to
                // `order_input`, which means that `placed` was true only for the
                // starting input, which means that `fully_supported` was true due to
                // one of the expressions referring only to the starting input.
                let msg = "Unreachable state in join order optimization".to_string();
                return Err(TransformError::Internal(msg));
                // (This couldn't be a soft_panic: we would form an arrangement with a wrong key.)
            }
        }
        self.order.insert(0, start_tuple);

        Ok(std::mem::replace(&mut self.order, Vec::new()))
    }

    /// Introduces a specific input and keys to the order, along with its characteristics.
    ///
    /// This method places a next element in the order, and updates the associated state
    /// about other candidates, including which columns are now bound and which potential
    /// keys are available to consider (both arranged, and unarranged).
    fn order_input(&mut self, input: usize) {
        self.placed[input] = true;
        for (equivalence, expr_index) in self.reverse_equivalences[input].iter() {
            if !self.equivalences_active[*equivalence] {
                // Placing `input` *may* activate the equivalence. Each of its columns
                // come in to scope, which may result in an expression in `equivalence`
                // becoming fully defined (when its support is contained in placed inputs)
                let fully_supported = self
                    .input_mapper
                    .lookup_inputs(&self.equivalences[*equivalence][*expr_index])
                    .all(|i| self.placed[i]);
                if fully_supported {
                    self.equivalences_active[*equivalence] = true;
                    for expr in self.equivalences[*equivalence].iter() {
                        // find the relations that columns in the expression belong to
                        let mut rels = self.input_mapper.lookup_inputs(expr);
                        // Skip the expression if
                        // * the expression is a literal -> this would translate
                        //   to `rels` being empty
                        // * the expression has columns belonging to more than
                        //   one relation -> TODO: see how we can plan better in
                        //   this case. Arguably, if this happens, it would
                        //   not be unreasonable to ask the user to write the
                        //   query better.
                        if let Some(rel) = rels.next() {
                            if rels.next().is_none() {
                                let expr = self.input_mapper.map_expr_to_local(expr.clone());

                                // Update bound columns.
                                self.bound[rel].push(expr);
                                self.bound[rel].sort();

                                // Reconsider all available arrangements.
                                for (pos, key) in self.arrangements[rel].iter().enumerate() {
                                    if !self.arrangement_active[rel].contains(&pos) {
                                        // TODO: support the restoration of the
                                        // following original lines, which have been
                                        // commented out because Materialize may
                                        // panic otherwise. The original line and comments
                                        // here are:
                                        // Determine if the arrangement is viable, which happens when the
                                        // support of its key is all bound.
                                        // if key.iter().all(|k| k.support().iter().all(|c| self.bound[*rel].contains(&ScalarExpr::Column(*c))) {

                                        // Determine if the arrangement is viable,
                                        // which happens when all its key components are bound.
                                        if key.iter().all(|k| self.bound[rel].contains(k)) {
                                            self.arrangement_active[rel].push(pos);
                                            // TODO: This could be pre-computed, as it is independent of the order.
                                            let is_unique = self.unique_arrangement[rel][pos];
                                            self.priority_queue.push((
                                                JoinInputCharacteristics::new(
                                                    is_unique,
                                                    key.len(),
                                                    true,
                                                    self.cardinalities[rel],
                                                    self.filters[rel].clone(),
                                                    rel,
                                                    self.enable_join_prioritize_arranged,
                                                ),
                                                key.clone(),
                                                rel,
                                            ));
                                        }
                                    }
                                }

                                // does the relation we're joining on have a unique key wrt what's already bound?
                                let is_unique = self.unique_keys[rel].iter().any(|cols| {
                                    cols.iter().all(|c| {
                                        self.bound[rel].contains(&MirScalarExpr::column(*c))
                                    })
                                });
                                self.priority_queue.push((
                                    JoinInputCharacteristics::new(
                                        is_unique,
                                        self.bound[rel].len(),
                                        false,
                                        self.cardinalities[rel],
                                        self.filters[rel].clone(),
                                        rel,
                                        self.enable_join_prioritize_arranged,
                                    ),
                                    self.bound[rel].clone(),
                                    rel,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
}
