use crate::lazy::logical_plan::optimizer::check_down_node;
use crate::lazy::prelude::*;
use crate::lazy::utils::{
    count_downtree_projections, expr_to_root_column, has_expr, rename_expr_root_name,
};
use crate::prelude::*;
use ahash::RandomState;
use std::collections::HashMap;
use std::sync::Arc;

// arbitrary constant to reduce reallocation.
// don't expect more than 100 predicates.
const HASHMAP_SIZE: usize = 100;

fn init_hashmap<K, V>() -> HashMap<K, V, RandomState> {
    HashMap::with_capacity_and_hasher(HASHMAP_SIZE, RandomState::new())
}

/// Don't overwrite predicates but combine them.
fn insert_and_combine_predicate(
    predicates_map: &mut HashMap<Arc<String>, Expr, RandomState>,
    name: Arc<String>,
    predicate: Expr,
) {
    let existing_predicate = predicates_map.entry(name).or_insert_with(|| lit(true));
    *existing_predicate = existing_predicate.clone().and(predicate)
}

pub struct PredicatePushDown {
    // used in has_expr check. This reduces box allocations
    unique_dummy: Expr,
    duplicated_dummy: Expr,
    binary_dummy: Expr,
    is_null_dummy: Expr,
    is_not_null_dummy: Expr,
}

impl Default for PredicatePushDown {
    fn default() -> Self {
        PredicatePushDown {
            unique_dummy: lit("_").is_unique(),
            duplicated_dummy: lit("_").is_duplicated(),
            binary_dummy: lit("_").eq(lit("_")),
            is_null_dummy: lit("_").is_null(),
            is_not_null_dummy: lit("_").is_null(),
        }
    }
}

pub(crate) fn combine_predicates<I>(iter: I) -> Expr
where
    I: Iterator<Item = Expr>,
{
    let mut single_pred = None;
    for expr in iter {
        single_pred = match single_pred {
            None => Some(expr),
            Some(e) => Some(e.and(expr)),
        };
    }
    single_pred.unwrap()
}

impl PredicatePushDown {
    fn finish_at_leaf(
        &self,
        lp: LogicalPlan,
        acc_predicates: HashMap<Arc<String>, Expr, RandomState>,
    ) -> Result<LogicalPlan> {
        match acc_predicates.len() {
            // No filter in the logical plan
            0 => Ok(lp),
            _ => {
                let mut builder = LogicalPlanBuilder::from(lp);

                let predicate = combine_predicates(acc_predicates.values().cloned());
                builder = builder.filter(predicate);
                Ok(builder.build())
            }
        }
    }

    fn finish_node(
        &self,
        local_predicates: Vec<Expr>,
        mut builder: LogicalPlanBuilder,
    ) -> Result<LogicalPlan> {
        if !local_predicates.is_empty() {
            let predicate = combine_predicates(local_predicates.into_iter());
            builder = builder.filter(predicate);
            Ok(builder.build())
        } else {
            Ok(builder.build())
        }
    }

    // acc predicates maps the root column names to predicates
    fn push_down(
        &self,
        logical_plan: LogicalPlan,
        mut acc_predicates: HashMap<Arc<String>, Expr, RandomState>,
    ) -> Result<LogicalPlan> {
        use LogicalPlan::*;

        match logical_plan {
            Selection { predicate, input } => {
                match expr_to_root_column(&predicate) {
                    Ok(name) => insert_and_combine_predicate(&mut acc_predicates, name, predicate),
                    Err(e) => {
                        if let Expr::BinaryExpr { left, right, .. } = &predicate {
                            let left_name = expr_to_root_column(&*left)?;
                            let right_name = expr_to_root_column(&*right)?;
                            let name = Arc::new(format!("{}-binary-{}", left_name, right_name));
                            insert_and_combine_predicate(&mut acc_predicates, name, predicate);
                        } else {
                            panic!(format!("{:?}", e))
                        }
                    }
                }
                self.push_down(*input, acc_predicates)
            }
            Projection { expr, input, .. } => {
                // don't filter before the last projection that is more expensive as projections are free
                if count_downtree_projections(&input, 0) == 0 {
                    let builder = LogicalPlanBuilder::from(self.push_down(
                        *input,
                        HashMap::with_capacity_and_hasher(HASHMAP_SIZE, RandomState::new()),
                    )?)
                    .project(expr);
                    // todo! write utility that takes hashmap values by value
                    self.finish_node(acc_predicates.values().cloned().collect(), builder)
                } else {
                    // maybe update predicate name if a projection is an alias
                    for e in &expr {
                        // check if there is an alias
                        if let Expr::Alias(e, name) = e {
                            // if this alias refers to one of the predicates in the upper nodes
                            // we rename the column of the predicate before we push it downwards.
                            if let Some(predicate) = acc_predicates.remove(name) {
                                let new_name = expr_to_root_column(e).unwrap();
                                let new_predicate =
                                    rename_expr_root_name(&predicate, new_name.clone()).unwrap();
                                insert_and_combine_predicate(
                                    &mut acc_predicates,
                                    new_name,
                                    new_predicate,
                                );
                            }
                        }
                    }
                    Ok(
                        LogicalPlanBuilder::from(self.push_down(*input, acc_predicates)?)
                            .project(expr)
                            .build(),
                    )
                }
            }
            LocalProjection { expr, input, .. } => {
                let input = self.push_down(*input, acc_predicates)?;
                let schema = input.schema();
                // projection from a wildcard may be dropped if the schema changes due to the optimization
                let proj = expr
                    .into_iter()
                    .filter(|e| check_down_node(e, schema))
                    .collect();
                Ok(LogicalPlanBuilder::from(input).project_local(proj).build())
            }
            DataFrameScan { df, schema } => {
                let lp = DataFrameScan { df, schema };
                self.finish_at_leaf(lp, acc_predicates)
            }
            CsvScan {
                path,
                schema,
                has_header,
                delimiter,
                ignore_errors,
                skip_rows,
                stop_after_n_rows,
                with_columns,
            } => {
                let lp = CsvScan {
                    path,
                    schema,
                    has_header,
                    delimiter,
                    ignore_errors,
                    skip_rows,
                    stop_after_n_rows,
                    with_columns,
                };
                self.finish_at_leaf(lp, acc_predicates)
            }
            DataFrameOp { input, operation } => {
                let input = self.push_down(*input, acc_predicates)?;
                Ok(DataFrameOp {
                    input: Box::new(input),
                    operation,
                })
            }
            Distinct {
                input,
                subset,
                maintain_order,
            } => {
                // currently the distinct operation only keeps the first occurrences.
                // this may have influence on the pushed down predicates. If the pushed down predicates
                // contain a binary expression (thus depending on values in multiple columns) the final result may differ if it is pushed down.
                let mut local_pred = Vec::with_capacity(acc_predicates.len());

                let mut new_acc_predicates = init_hashmap();
                for (name, predicate) in acc_predicates {
                    if has_expr(&predicate, &self.binary_dummy) {
                        local_pred.push(predicate)
                    } else {
                        new_acc_predicates.insert(name, predicate);
                    }
                }

                let input = self.push_down(*input, new_acc_predicates)?;
                let lp = Distinct {
                    input: Box::new(input),
                    maintain_order,
                    subset,
                };
                let mut builder = LogicalPlanBuilder::from(lp);
                if !local_pred.is_empty() {
                    let predicate = combine_predicates(local_pred.into_iter());
                    builder = builder.filter(predicate)
                }
                Ok(builder.build())
            }
            Aggregate {
                input,
                keys,
                aggs,
                schema,
            } => {
                // dont push down predicates. An aggregation needs all rows
                let lp = Aggregate {
                    input: Box::new(self.push_down(*input, init_hashmap())?),
                    keys,
                    aggs,
                    schema,
                };
                self.finish_at_leaf(lp, acc_predicates)
            }
            Join {
                input_left,
                input_right,
                left_on,
                right_on,
                how,
                ..
            } => {
                let schema_left = input_left.schema();
                let schema_right = input_right.schema();

                let mut pushdown_left = init_hashmap();
                let mut pushdown_right = init_hashmap();
                let mut local_predicates = Vec::with_capacity(acc_predicates.len());

                for (_, predicate) in acc_predicates {
                    // unique and duplicated can be caused by joins
                    if has_expr(&predicate, &self.unique_dummy) {
                        local_predicates.push(predicate.clone());
                        continue;
                    }
                    if has_expr(&predicate, &self.duplicated_dummy) {
                        local_predicates.push(predicate.clone());
                        continue;
                    }
                    let mut filter_left = false;
                    let mut filter_right = false;

                    // no else if. predicate can be in both tables.
                    if check_down_node(&predicate, schema_left) {
                        let name =
                            Arc::new(predicate.to_field(schema_left).unwrap().name().clone());
                        insert_and_combine_predicate(&mut pushdown_left, name, predicate.clone());
                        filter_left = true;
                    }
                    if check_down_node(&predicate, schema_right) {
                        let name =
                            Arc::new(predicate.to_field(schema_right).unwrap().name().clone());
                        insert_and_combine_predicate(&mut pushdown_right, name, predicate.clone());
                        filter_right = true;
                    }
                    if !(filter_left & filter_right) {
                        local_predicates.push(predicate.clone());
                        continue;
                    }
                    // An outer join or left join may create null values.
                    // we also do it local
                    if (how == JoinType::Outer) | (how == JoinType::Left) {
                        if has_expr(&predicate, &self.is_not_null_dummy) {
                            local_predicates.push(predicate.clone());
                            continue;
                        }
                        if has_expr(&predicate, &self.is_null_dummy) {
                            local_predicates.push(predicate);
                            continue;
                        }
                    }
                }

                let lp_left = self.push_down(*input_left, pushdown_left)?;
                let lp_right = self.push_down(*input_right, pushdown_right)?;

                let builder =
                    LogicalPlanBuilder::from(lp_left).join(lp_right, how, left_on, right_on);
                self.finish_node(local_predicates, builder)
            }
            HStack { input, exprs, .. } => {
                let (local, acc_predicates) =
                    self.split_pushdown_and_local(acc_predicates, input.schema());
                let mut lp_builder =
                    LogicalPlanBuilder::from(self.push_down(*input, acc_predicates)?)
                        .with_columns(exprs);

                if !local.is_empty() {
                    let predicate = combine_predicates(local.into_iter());
                    lp_builder = lp_builder.filter(predicate);
                }
                Ok(lp_builder.build())
            }
        }
    }

    /// Check if a predicate can be pushed down or not. If it cannot remove it from the accumulated predicates.
    fn split_pushdown_and_local(
        &self,
        mut acc_predicates: HashMap<Arc<String>, Expr, RandomState>,
        schema: &Schema,
    ) -> (Vec<Expr>, HashMap<Arc<String>, Expr, RandomState>) {
        let mut local = Vec::with_capacity(acc_predicates.len());
        let mut local_keys = Vec::with_capacity(acc_predicates.len());
        for (key, predicate) in &acc_predicates {
            if !check_down_node(predicate, schema) {
                local_keys.push(key.clone());
            }
        }
        for key in local_keys {
            local.push(acc_predicates.remove(&key).unwrap());
        }
        (local, acc_predicates)
    }
}

impl Optimize for PredicatePushDown {
    fn optimize(&self, logical_plan: LogicalPlan) -> Result<LogicalPlan> {
        self.push_down(
            logical_plan,
            HashMap::with_capacity_and_hasher(HASHMAP_SIZE, RandomState::new()),
        )
    }
}
