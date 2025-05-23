// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::BTreeSet;
use std::ops::Range;

use itertools::Itertools;
use mz_repr::RelationType;

use crate::visit::Visit;
use crate::{MirRelationExpr, MirScalarExpr, VariadicFunc};

/// Any column in a join expression exists in two contexts:
/// 1) It has a position relative to the result of the join (global)
/// 2) It has a position relative to the specific input it came from (local)
/// This utility focuses on taking expressions that are in terms of
/// the local input and re-expressing them in global terms and vice versa.
///
/// Methods in this class that take an argument `equivalences` are only
/// guaranteed to return a correct answer if equivalence classes are in
/// canonical form.
/// (See [`crate::relation::canonicalize::canonicalize_equivalences`].)
#[derive(Debug)]
pub struct JoinInputMapper {
    /// The number of columns per input. All other fields in this struct are
    /// derived using the information in this field.
    arities: Vec<usize>,
    /// Looks up which input each column belongs to. Derived from `arities`.
    /// Stored as a field to avoid recomputation.
    input_relation: Vec<usize>,
    /// The sum of the arities of the previous inputs in the join. Derived from
    /// `arities`. Stored as a field to avoid recomputation.
    prior_arities: Vec<usize>,
}

impl JoinInputMapper {
    /// Creates a new `JoinInputMapper` and calculates the mapping of global context
    /// columns to local context columns.
    pub fn new(inputs: &[MirRelationExpr]) -> Self {
        Self::new_from_input_arities(inputs.iter().map(|i| i.arity()))
    }

    /// Creates a new `JoinInputMapper` and calculates the mapping of global context
    /// columns to local context columns. Using this method saves is more
    /// efficient if input types have been pre-calculated
    pub fn new_from_input_types(types: &[RelationType]) -> Self {
        Self::new_from_input_arities(types.iter().map(|t| t.column_types.len()))
    }

    /// Creates a new `JoinInputMapper` and calculates the mapping of global context
    /// columns to local context columns. Using this method saves is more
    /// efficient if input arities have been pre-calculated
    pub fn new_from_input_arities<I>(arities: I) -> Self
    where
        I: IntoIterator<Item = usize>,
    {
        let arities = arities.into_iter().collect::<Vec<usize>>();
        let mut offset = 0;
        let mut prior_arities = Vec::new();
        for input in 0..arities.len() {
            prior_arities.push(offset);
            offset += arities[input];
        }

        let input_relation = arities
            .iter()
            .enumerate()
            .flat_map(|(r, a)| std::iter::repeat(r).take(*a))
            .collect::<Vec<_>>();

        JoinInputMapper {
            arities,
            input_relation,
            prior_arities,
        }
    }

    /// reports sum of the number of columns of each input
    pub fn total_columns(&self) -> usize {
        self.arities.iter().sum()
    }

    /// reports total numbers of inputs in the join
    pub fn total_inputs(&self) -> usize {
        self.arities.len()
    }

    /// Using the keys that came from each local input,
    /// figures out which keys remain unique in the larger join
    /// Currently, we only figure out a small subset of the keys that
    /// can remain unique.
    pub fn global_keys<'a, I>(
        &self,
        mut local_keys: I,
        equivalences: &[Vec<MirScalarExpr>],
    ) -> Vec<Vec<usize>>
    where
        I: Iterator<Item = &'a Vec<Vec<usize>>>,
    {
        // A relation's uniqueness constraint holds if there is a
        // sequence of the other relations such that each one has
        // a uniqueness constraint whose columns are used in join
        // constraints with relations prior in the sequence.
        //
        // Currently, we only:
        // 1. test for whether the uniqueness constraints for the first input will hold
        // 2. try one sequence, namely the inputs in order
        // 3. check that the column themselves are used in the join constraints
        //    Technically uniqueness constraint would still hold if a 1-to-1
        //    expression on a unique key is used in the join constraint.

        // for inputs `1..self.total_inputs()`, store a set of columns from that
        // input that exist in join constraints that have expressions belonging to
        // earlier inputs.
        let mut column_with_prior_bound_by_input = vec![BTreeSet::new(); self.total_inputs() - 1];
        for equivalence in equivalences {
            // do a scan to find the first input represented in the constraint
            let min_bound_input = equivalence
                .iter()
                .flat_map(|expr| self.lookup_inputs(expr).max())
                .min();
            if let Some(min_bound_input) = min_bound_input {
                for expr in equivalence {
                    // then store all columns in the constraint that don't come
                    // from the first input
                    if let MirScalarExpr::Column(c, _name) = expr {
                        let (col, input) = self.map_column_to_local(*c);
                        if input > min_bound_input {
                            column_with_prior_bound_by_input[input - 1].insert(col);
                        }
                    }
                }
            }
        }

        if self.total_inputs() > 0 {
            let first_input_keys = local_keys.next().unwrap().clone();
            // for inputs `1..self.total_inputs()`, checks the keys belong to each
            // input against the storage of columns that exist in join constraints
            // that have expressions belonging to earlier inputs.
            let remains_unique = local_keys.enumerate().all(|(index, keys)| {
                keys.iter().any(|ks| {
                    ks.iter()
                        .all(|k| column_with_prior_bound_by_input[index].contains(k))
                })
            });

            if remains_unique {
                return first_input_keys;
            }
        }
        vec![]
    }

    /// returns the arity for a particular input
    #[inline]
    pub fn input_arity(&self, index: usize) -> usize {
        self.arities[index]
    }

    /// All column numbers in order for a particular input in the local context
    #[inline]
    pub fn local_columns(&self, index: usize) -> Range<usize> {
        0..self.arities[index]
    }

    /// All column numbers in order for a particular input in the global context
    #[inline]
    pub fn global_columns(&self, index: usize) -> Range<usize> {
        self.prior_arities[index]..(self.prior_arities[index] + self.arities[index])
    }

    /// Takes an expression from the global context and creates a new version
    /// where column references have been remapped to the local context.
    /// Assumes that all columns in `expr` are from the same input.
    pub fn map_expr_to_local(&self, mut expr: MirScalarExpr) -> MirScalarExpr {
        expr.visit_pre_mut(|e| {
            if let MirScalarExpr::Column(c, _name) = e {
                *c -= self.prior_arities[self.input_relation[*c]];
            }
        });
        expr
    }

    /// Takes an expression from the local context of the `index`th input and
    /// creates a new version where column references have been remapped to the
    /// global context.
    pub fn map_expr_to_global(&self, mut expr: MirScalarExpr, index: usize) -> MirScalarExpr {
        expr.visit_pre_mut(|e| {
            if let MirScalarExpr::Column(c, _name) = e {
                *c += self.prior_arities[index];
            }
        });
        expr
    }

    /// Remap column numbers from the global to the local context.
    /// Returns a 2-tuple `(<new column number>, <index of input>)`
    pub fn map_column_to_local(&self, column: usize) -> (usize, usize) {
        let index = self.input_relation[column];
        (column - self.prior_arities[index], index)
    }

    /// Remap a column number from a local context to the global context.
    pub fn map_column_to_global(&self, column: usize, index: usize) -> usize {
        column + self.prior_arities[index]
    }

    /// Takes a sequence of columns in the global context and splits it into
    /// a `Vec` containing `self.total_inputs()` `BTreeSet`s, each containing
    /// the localized columns that belong to the particular input.
    pub fn split_column_set_by_input<'a, I>(&self, columns: I) -> Vec<BTreeSet<usize>>
    where
        I: Iterator<Item = &'a usize>,
    {
        let mut new_columns = vec![BTreeSet::new(); self.total_inputs()];
        for column in columns {
            let (new_col, input) = self.map_column_to_local(*column);
            new_columns[input].extend(std::iter::once(new_col));
        }
        new_columns
    }

    /// Find the sorted, dedupped set of inputs an expression references
    pub fn lookup_inputs(&self, expr: &MirScalarExpr) -> impl Iterator<Item = usize> + use<> {
        expr.support()
            .iter()
            .map(|c| self.input_relation[*c])
            .sorted()
            .dedup()
    }

    /// Returns the index of the only input referenced in the given expression.
    pub fn single_input(&self, expr: &MirScalarExpr) -> Option<usize> {
        let mut inputs = self.lookup_inputs(expr);
        if let Some(first_input) = inputs.next() {
            if inputs.next().is_none() {
                return Some(first_input);
            }
        }
        None
    }

    /// Returns whether the given expr refers to columns of only the `index`th input.
    pub fn is_localized(&self, expr: &MirScalarExpr, index: usize) -> bool {
        if let Some(single_input) = self.single_input(expr) {
            if single_input == index {
                return true;
            }
        }
        false
    }

    /// Takes an expression in the global context and looks in `equivalences`
    /// for an equivalent expression (also expressed in the global context) that
    /// belongs to one or more of the inputs in `bound_inputs`
    ///
    /// # Examples
    ///
    /// ```
    /// use mz_repr::{Datum, ColumnType, RelationType, ScalarType};
    /// use mz_expr::{JoinInputMapper, MirRelationExpr, MirScalarExpr};
    ///
    /// // A two-column schema common to each of the three inputs
    /// let schema = RelationType::new(vec![
    ///   ScalarType::Int32.nullable(false),
    ///   ScalarType::Int32.nullable(false),
    /// ]);
    ///
    /// // the specific data are not important here.
    /// let data = vec![Datum::Int32(0), Datum::Int32(1)];
    /// let input0 = MirRelationExpr::constant(vec![data.clone()], schema.clone());
    /// let input1 = MirRelationExpr::constant(vec![data.clone()], schema.clone());
    /// let input2 = MirRelationExpr::constant(vec![data.clone()], schema.clone());
    ///
    /// // [input0(#0) = input2(#1)], [input0(#1) = input1(#0) = input2(#0)]
    /// let equivalences = vec![
    ///   vec![MirScalarExpr::column(0), MirScalarExpr::column(5)],
    ///   vec![MirScalarExpr::column(1), MirScalarExpr::column(2), MirScalarExpr::column(4)],
    /// ];
    ///
    /// let input_mapper = JoinInputMapper::new(&[input0, input1, input2]);
    /// assert_eq!(
    ///   Some(MirScalarExpr::column(4)),
    ///   input_mapper.find_bound_expr(&MirScalarExpr::column(2), &[2], &equivalences)
    /// );
    /// assert_eq!(
    ///   None,
    ///   input_mapper.find_bound_expr(&MirScalarExpr::column(0), &[1], &equivalences)
    /// );
    /// ```
    pub fn find_bound_expr(
        &self,
        expr: &MirScalarExpr,
        bound_inputs: &[usize],
        equivalences: &[Vec<MirScalarExpr>],
    ) -> Option<MirScalarExpr> {
        if let Some(equivalence) = equivalences.iter().find(|equivs| equivs.contains(expr)) {
            if let Some(bound_expr) = equivalence
                .iter()
                .find(|expr| self.lookup_inputs(expr).all(|i| bound_inputs.contains(&i)))
            {
                return Some(bound_expr.clone());
            }
        }
        None
    }

    /// Try to rewrite `expr` from the global context so that all the
    /// columns point to the `index`th input by replacing subexpressions with their
    /// bound equivalents in the `index`th input if necessary.
    /// Returns whether the rewriting was successful.
    /// If it returns true, then `expr` is in the context of the `index`th input.
    /// If it returns false, then still some subexpressions might have been rewritten. However,
    /// `expr` is still in the global context.
    pub fn try_localize_to_input_with_bound_expr(
        &self,
        expr: &mut MirScalarExpr,
        index: usize,
        equivalences: &[Vec<MirScalarExpr>],
    ) -> bool {
        // TODO (wangandi): Consider changing this code to be post-order
        // instead of pre-order? `lookup_inputs` traverses all the nodes in
        // `e` anyway, so we end up visiting nodes in `e` multiple times
        // here. Alternatively, consider having the future `PredicateKnowledge`
        // take over the responsibilities of this code?
        #[allow(deprecated)]
        expr.visit_mut_pre_post_nolimit(
            &mut |e| {
                let mut inputs = self.lookup_inputs(e);
                if let Some(first_input) = inputs.next() {
                    if inputs.next().is_none() && first_input == index {
                        // there is only one input, and it is equal to index, so we're
                        // good. do not continue the recursion
                        return Some(vec![]);
                    }
                }

                if let Some(bound_expr) = self.find_bound_expr(e, &[index], equivalences) {
                    // Replace the subexpression with the equivalent one from input `index`
                    *e = bound_expr;
                    // The entire subexpression has been rewritten, so there is
                    // no need to visit any child expressions.
                    Some(vec![])
                } else {
                    None
                }
            },
            &mut |_| {},
        );
        if self.is_localized(expr, index) {
            // If the localization attempt is successful, all columns in `expr`
            // should only come from input `index`. Switch to the local context.
            *expr = self.map_expr_to_local(expr.clone());
            return true;
        }
        false
    }

    /// Try to find a consequence `c` of the given expression `e` for the given input.
    ///
    /// If we return `Some(c)`, that means
    ///   1. `c` uses only columns from the given input;
    ///   2. if `c` doesn't hold on a row of the input, then `e` also wouldn't hold;
    ///   3. if `c` holds on a row of the input, then `e` might or might not hold.
    /// 1. and 2. means that if we have a join with predicate `e` then we can use `c` for
    /// pre-filtering a join input before the join. However, 3. means that `e` shouldn't be deleted
    /// from the join predicates, i.e., we can't do a "traditional" predicate pushdown.
    ///
    /// Note that "`c` is a consequence of `e`" is the same thing as 2., see
    /// <https://en.wikipedia.org/wiki/Contraposition>
    ///
    /// Example: For
    /// `(t1.f2 = 3 AND t2.f2 = 4) OR (t1.f2 = 5 AND t2.f2 = 6)`
    /// we find
    /// `t1.f2 = 3 OR t1.f2 = 5` for t1, and
    /// `t2.f2 = 4 OR t2.f2 = 6` for t2.
    ///
    /// Further examples are in TPC-H Q07, Q19, and chbench Q07, Q19.
    ///
    /// Parameters:
    ///  - `expr`: The expression `e` from above. `try_localize_to_input_with_bound_expr` should
    ///    be called on `expr` before us!
    ///  - `index`: The index of the join input whose columns we will use.
    ///  - `equivalences`: Join equivalences that we can use for `try_map_to_input_with_bound_expr`.
    /// If successful, the returned expression is in the local context of the specified input.
    pub fn consequence_for_input(
        &self,
        expr: &MirScalarExpr,
        index: usize,
    ) -> Option<MirScalarExpr> {
        if self.is_localized(expr, index) {
            Some(self.map_expr_to_local(expr.clone()))
        } else {
            match expr {
                MirScalarExpr::CallVariadic {
                    func: VariadicFunc::Or,
                    exprs: or_args,
                } => {
                    // Each OR arg should provide a consequence. If they do, we OR them.
                    let consequences_per_arg = or_args
                        .into_iter()
                        .map(|or_arg| {
                            mz_ore::stack::maybe_grow(|| self.consequence_for_input(or_arg, index))
                        })
                        .collect::<Option<Vec<_>>>()?; // return None if any of them are None
                    Some(MirScalarExpr::CallVariadic {
                        func: VariadicFunc::Or,
                        exprs: consequences_per_arg,
                    })
                }
                MirScalarExpr::CallVariadic {
                    func: VariadicFunc::And,
                    exprs: and_args,
                } => {
                    // If any of the AND args provide a consequence, then we take those that do,
                    // and AND them.
                    let consequences_per_arg = and_args
                        .into_iter()
                        .map(|and_arg| {
                            mz_ore::stack::maybe_grow(|| self.consequence_for_input(and_arg, index))
                        })
                        .flat_map(|c| c) // take only those that provide a consequence
                        .collect_vec();
                    if consequences_per_arg.is_empty() {
                        None
                    } else {
                        Some(MirScalarExpr::CallVariadic {
                            func: VariadicFunc::And,
                            exprs: consequences_per_arg,
                        })
                    }
                }
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use mz_repr::{Datum, ScalarType};

    use crate::{BinaryFunc, MirScalarExpr, UnaryFunc};

    use super::*;

    #[mz_ore::test]
    #[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `rust_psm_stack_pointer` on OS `linux`
    fn try_map_to_input_with_bound_expr_test() {
        let input_mapper = JoinInputMapper {
            arities: vec![2, 3, 3],
            input_relation: vec![0, 0, 1, 1, 1, 2, 2, 2],
            prior_arities: vec![0, 2, 5],
        };

        // keys are numbered by (equivalence class #, input #)
        let key10 = MirScalarExpr::column(0);
        let key12 = MirScalarExpr::column(6);
        let localized_key12 = MirScalarExpr::column(1);

        let mut equivalences = vec![vec![key10.clone(), key12.clone()]];

        // when the column is already part of the target input, all that happens
        // is that it gets localized
        let mut cloned = key12.clone();
        input_mapper.try_localize_to_input_with_bound_expr(&mut cloned, 2, &equivalences);
        assert_eq!(MirScalarExpr::column(1), cloned);

        // basic tests that we can find a column's corresponding column in a
        // different input
        let mut cloned = key12.clone();
        input_mapper.try_localize_to_input_with_bound_expr(&mut cloned, 0, &equivalences);
        assert_eq!(key10, cloned);
        let mut cloned = key12.clone();
        assert_eq!(
            false,
            input_mapper.try_localize_to_input_with_bound_expr(&mut cloned, 1, &equivalences),
        );

        let key20 = MirScalarExpr::CallUnary {
            func: UnaryFunc::NegInt32(crate::func::NegInt32),
            expr: Box::new(MirScalarExpr::column(1)),
        };
        let key21 = MirScalarExpr::CallBinary {
            func: BinaryFunc::AddInt32,
            expr1: Box::new(MirScalarExpr::column(2)),
            expr2: Box::new(MirScalarExpr::literal(
                Ok(Datum::Int32(4)),
                ScalarType::Int32,
            )),
        };
        let key22 = MirScalarExpr::column(5);
        let localized_key22 = MirScalarExpr::column(0);
        equivalences.push(vec![key22.clone(), key20.clone(), key21.clone()]);

        // basic tests that we can find an expression's corresponding expression in a
        // different input
        let mut cloned = key21.clone();
        input_mapper.try_localize_to_input_with_bound_expr(&mut cloned, 0, &equivalences);
        assert_eq!(key20, cloned);
        let mut cloned = key21.clone();
        input_mapper.try_localize_to_input_with_bound_expr(&mut cloned, 2, &equivalences);
        assert_eq!(localized_key22, cloned);

        // test that `try_map_to_input_with_bound_expr` will map multiple
        // subexpressions to the corresponding expressions bound to a different input
        let key_comp = MirScalarExpr::CallBinary {
            func: BinaryFunc::MulInt32,
            expr1: Box::new(key12.clone()),
            expr2: Box::new(key22),
        };
        let mut cloned = key_comp.clone();
        input_mapper.try_localize_to_input_with_bound_expr(&mut cloned, 0, &equivalences);
        assert_eq!(
            MirScalarExpr::CallBinary {
                func: BinaryFunc::MulInt32,
                expr1: Box::new(key10.clone()),
                expr2: Box::new(key20.clone()),
            },
            cloned,
        );

        // test that the function returns None when part
        // of the expression can be mapped to an input but the rest can't
        let mut cloned = key_comp.clone();
        assert_eq!(
            false,
            input_mapper.try_localize_to_input_with_bound_expr(&mut cloned, 1, &equivalences),
        );

        let key_comp_plus_non_key = MirScalarExpr::CallBinary {
            func: BinaryFunc::Eq,
            expr1: Box::new(key_comp),
            expr2: Box::new(MirScalarExpr::column(7)),
        };
        let mut mutab = key_comp_plus_non_key;
        assert_eq!(
            false,
            input_mapper.try_localize_to_input_with_bound_expr(&mut mutab, 0, &equivalences),
        );

        let key_comp_multi_input = MirScalarExpr::CallBinary {
            func: BinaryFunc::Eq,
            expr1: Box::new(key12),
            expr2: Box::new(key21),
        };
        // test that the function works when part of the expression is already
        // part of the target input
        let mut cloned = key_comp_multi_input.clone();
        input_mapper.try_localize_to_input_with_bound_expr(&mut cloned, 2, &equivalences);
        assert_eq!(
            MirScalarExpr::CallBinary {
                func: BinaryFunc::Eq,
                expr1: Box::new(localized_key12),
                expr2: Box::new(localized_key22),
            },
            cloned,
        );
        // test that the function works when parts of the expression come from
        // multiple inputs
        let mut cloned = key_comp_multi_input.clone();
        input_mapper.try_localize_to_input_with_bound_expr(&mut cloned, 0, &equivalences);
        assert_eq!(
            MirScalarExpr::CallBinary {
                func: BinaryFunc::Eq,
                expr1: Box::new(key10),
                expr2: Box::new(key20),
            },
            cloned,
        );
        let mut mutab = key_comp_multi_input;
        assert_eq!(
            false,
            input_mapper.try_localize_to_input_with_bound_expr(&mut mutab, 1, &equivalences),
        )
    }
}
