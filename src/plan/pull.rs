//! Pull expression plan, but without nesting.

use timely::dataflow::operators::{Concat, Concatenate};
use timely::dataflow::scopes::child::Iterative;
use timely::dataflow::Scope;
use timely::order::Product;
use timely::progress::Timestamp;

use differential_dataflow::lattice::Lattice;
use differential_dataflow::AsCollection;

use crate::binding::AsBinding;
use crate::domain::Domain;
use crate::plan::{Dependencies, Implementable};
use crate::timestamp::Rewind;
use crate::{AsAid, Value, Var};
use crate::{CollectionRelation, Implemented, Relation, ShutdownHandle, VariableMap};

/// A plan stage for extracting all matching [e a v] tuples for a
/// given set of attributes and an input relation specifying entities.
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Debug, Serialize, Deserialize)]
pub struct PullLevel<A: AsAid, P: Implementable<A = A>> {
    /// TODO
    pub variables: Vec<Var>,
    /// Plan for the input relation.
    pub plan: Box<P>,
    /// Eid variable.
    pub pull_variable: Var,
    /// Attributes to pull for the input entities.
    pub pull_attributes: Vec<A>,
    /// Attribute names to distinguish plans of the same
    /// length. Useful to feed into a nested hash-map directly.
    pub path_attributes: Vec<A>,
    /// @TODO
    pub cardinality_many: bool,
}

/// A plan stage for pull queries split into individual paths. So
/// `[:parent/name {:parent/child [:child/name]}]` would be
/// represented as:
///
/// (?parent)                      <- [:parent/name] | no constraints
/// (?parent :parent/child ?child) <- [:child/name]  | [?parent :parent/child ?child]
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Debug, Serialize, Deserialize)]
pub struct Pull<P: Implementable> {
    /// TODO
    pub variables: Vec<Var>,
    /// Individual paths to pull.
    pub paths: Vec<P>,
}

fn interleave<A: AsAid>(values: &[Value], constants: &[A]) -> Vec<Value> {
    if values.is_empty() || constants.is_empty() {
        values.to_owned()
    } else {
        let size: usize = values.len() + constants.len();
        // + 2, because we know there'll be a and v coming...
        let mut result: Vec<Value> = Vec::with_capacity(size + 2);

        let mut next_value = 0;
        let mut next_const = 0;

        for i in 0..size {
            if i % 2 == 0 {
                // on even indices we take from the result tuple
                result.push(values[next_value].clone());
                next_value += 1;
            } else {
                // on odd indices we interleave an attribute
                let a = constants[next_const].clone();
                result.push(a.into_value());
                next_const += 1;
            }
        }

        result
    }
}

impl<A: AsAid + 'static, P: Implementable<A = A>> Implementable for PullLevel<A, P> {
    type A = A;

    fn dependencies(&self) -> Dependencies<Self::A> {
        let attribute_dependencies = self
            .pull_attributes
            .iter()
            .cloned()
            .map(Dependencies::attribute)
            .sum();

        self.plan.dependencies() + attribute_dependencies
    }

    fn implement<'b, S>(
        &self,
        nested: &mut Iterative<'b, S, u64>,
        domain: &mut Domain<Self::A, S::Timestamp>,
        local_arrangements: &VariableMap<Self::A, Iterative<'b, S, u64>>,
    ) -> (Implemented<'b, Self::A, S>, ShutdownHandle)
    where
        S: Scope,
        S::Timestamp: Timestamp + Lattice + Rewind,
    {
        use differential_dataflow::operators::arrange::{Arrange, Arranged, TraceAgent};
        use differential_dataflow::operators::JoinCore;
        use differential_dataflow::trace::implementations::ord::OrdValSpine;
        use differential_dataflow::trace::TraceReader;

        let (input, mut shutdown_handle) = self.plan.implement(nested, domain, local_arrangements);

        if self.pull_attributes.is_empty() {
            if self.path_attributes.is_empty() {
                // nothing to pull
                (input, shutdown_handle)
            } else {
                let path_attributes = self.path_attributes.clone();
                let tuples = {
                    let (tuples, shutdown) = input.tuples(nested, domain);
                    shutdown_handle.merge_with(shutdown);

                    tuples.map(move |tuple| interleave(&tuple, &path_attributes))
                };

                (
                    Implemented::Collection(CollectionRelation {
                        variables: self.variables.to_vec(),
                        tuples,
                    }),
                    shutdown_handle,
                )
            }
        } else {
            // Arrange input entities by eid.
            let e_offset = input
                .binds(self.pull_variable)
                .expect("input relation doesn't bind pull_variable");

            let paths = {
                let (tuples, shutdown) = input.tuples(nested, domain);
                shutdown_handle.merge_with(shutdown);
                tuples
            };

            let e_path: Arranged<
                Iterative<S, u64>,
                TraceAgent<OrdValSpine<Value, Vec<Value>, Product<S::Timestamp, u64>, isize>>,
            > = paths.map(move |t| (t[e_offset].clone(), t)).arrange();

            let mut shutdown_handle = shutdown_handle;
            let streams = self.pull_attributes.iter().map(|a| {
                let e_v = match domain.forward_propose(a) {
                    None => panic!("attribute {:?} does not exist", a),
                    Some(propose_trace) => {
                        let frontier: Vec<S::Timestamp> = propose_trace.advance_frontier().to_vec();
                        let (arranged, shutdown_propose) = propose_trace
                            .import_frontier(&nested.parent, &format!("Propose({:?})", a));

                        let e_v = arranged.enter_at(nested, move |_, _, time| {
                            let mut forwarded = time.clone();
                            forwarded.advance_by(&frontier);
                            Product::new(forwarded, 0)
                        });

                        shutdown_handle.add_button(shutdown_propose);

                        e_v
                    }
                };

                let attribute = a.clone().into_value();
                let path_attributes: Vec<Self::A> = self.path_attributes.clone();

                if path_attributes.is_empty() || self.cardinality_many {
                    e_path
                        .join_core(&e_v, move |_e, path: &Vec<Value>, v: &Value| {
                            // Each result tuple must hold the interleaved
                            // path, the attribute, and the value,
                            // i.e. [?p "parent/child" ?c ?a ?v]
                            let mut result = interleave(path, &path_attributes);
                            result.push(attribute.clone());
                            result.push(v.clone());

                            Some(result)
                        })
                        .inner
                } else {
                    e_path
                        .join_core(&e_v, move |_e, path: &Vec<Value>, v: &Value| {
                            // Each result tuple must hold the interleaved
                            // path, the attribute, and the value,
                            // i.e. [?p "parent/child" ?c ?a ?v]
                            let mut result = interleave(path, &path_attributes);

                            // Cardinality single means we don't need
                            // to distinguish child ids (there can
                            // only be one).
                            result.pop().expect("malformed path");

                            result.push(attribute.clone());
                            result.push(v.clone());

                            Some(result)
                        })
                        .inner
                }
            });

            let tuples = if self.path_attributes.is_empty() || self.cardinality_many {
                nested.concatenate(streams)
            } else {
                let db_ids = {
                    let path_attributes = self.path_attributes.clone();
                    paths
                        .map(move |path| {
                            let mut result = interleave(&path, &path_attributes);
                            let eid = result.pop().expect("malformed path");

                            result.push(Value::Aid("db__id".to_string()));
                            result.push(eid);

                            result
                        })
                        .inner
                };

                nested.concatenate(streams).concat(&db_ids)
            };

            let relation = CollectionRelation {
                variables: vec![], // @TODO
                tuples: tuples.as_collection(),
            };

            (Implemented::Collection(relation), shutdown_handle)
        }
    }
}

impl<P: Implementable> Implementable for Pull<P> {
    type A = P::A;

    fn dependencies(&self) -> Dependencies<Self::A> {
        self.paths.iter().map(|path| path.dependencies()).sum()
    }

    fn implement<'b, S>(
        &self,
        nested: &mut Iterative<'b, S, u64>,
        domain: &mut Domain<Self::A, S::Timestamp>,
        local_arrangements: &VariableMap<Self::A, Iterative<'b, S, u64>>,
    ) -> (Implemented<'b, Self::A, S>, ShutdownHandle)
    where
        S: Scope,
        S::Timestamp: Timestamp + Lattice + Rewind,
    {
        let mut scope = nested.clone();
        let mut shutdown_handle = ShutdownHandle::empty();

        let streams = self.paths.iter().map(|path| {
            let relation = {
                let (relation, shutdown) = path.implement(&mut scope, domain, local_arrangements);
                shutdown_handle.merge_with(shutdown);
                relation
            };

            let tuples = {
                let (tuples, shutdown) = relation.tuples(&mut scope, domain);
                shutdown_handle.merge_with(shutdown);
                tuples
            };

            tuples.inner
        });

        let tuples = nested.concatenate(streams).as_collection();

        let relation = CollectionRelation {
            variables: self.variables.to_vec(),
            tuples,
        };

        (Implemented::Collection(relation), shutdown_handle)
    }
}

/// A plan stage for extracting all tuples for a given set of
/// attributes.
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Debug, Serialize, Deserialize)]
pub struct PullAll<A: AsAid> {
    /// TODO
    pub variables: Vec<Var>,
    /// Attributes to pull for the input entities.
    pub pull_attributes: Vec<A>,
}

impl<A: AsAid> Implementable for PullAll<A> {
    type A = A;

    fn dependencies(&self) -> Dependencies<A> {
        self.pull_attributes
            .iter()
            .cloned()
            .map(Dependencies::attribute)
            .sum()
    }

    fn implement<'b, S>(
        &self,
        nested: &mut Iterative<'b, S, u64>,
        domain: &mut Domain<A, S::Timestamp>,
        _local_arrangements: &VariableMap<Self::A, Iterative<'b, S, u64>>,
    ) -> (Implemented<'b, Self::A, S>, ShutdownHandle)
    where
        S: Scope,
        S::Timestamp: Timestamp + Lattice + Rewind,
    {
        use differential_dataflow::trace::TraceReader;

        assert!(!self.pull_attributes.is_empty());

        let mut shutdown_handle = ShutdownHandle::empty();

        let streams = self.pull_attributes.iter().map(|a| {
            let e_v = match domain.forward_propose(a) {
                None => panic!("attribute {:?} does not exist", a),
                Some(propose_trace) => {
                    let frontier: Vec<S::Timestamp> = propose_trace.advance_frontier().to_vec();
                    let (arranged, shutdown_propose) =
                        propose_trace.import_frontier(&nested.parent, &format!("Propose({:?})", a));

                    let e_v = arranged.enter_at(nested, move |_, _, time| {
                        let mut forwarded = time.clone();
                        forwarded.advance_by(&frontier);
                        Product::new(forwarded, 0)
                    });

                    shutdown_handle.add_button(shutdown_propose);

                    e_v
                }
            };

            let attribute = a.clone().into_value();

            e_v.as_collection(move |e, v| vec![e.clone(), attribute.clone(), v.clone()])
                .inner
        });

        let tuples = nested.concatenate(streams).as_collection();

        let relation = CollectionRelation {
            variables: vec![], // @TODO
            tuples,
        };

        (Implemented::Collection(relation), shutdown_handle)
    }
}
