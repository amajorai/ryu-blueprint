//! Step DAG validation and layout.
//!
//! The companion draws the graph with `@xyflow/react`, which places nodes wherever it
//! is told and has no opinion about where that is. The usual answer is to ship dagre
//! or elk to the browser and lay out on every render; this crate does it here instead,
//! once per revision, for three reasons:
//!
//! * **The layout is part of the plan, not part of the view.** Two people looking at
//!   the same revision should see the same picture, and a client-side heuristic seeded
//!   differently (or a different library version) does not guarantee that.
//! * **It has to be validated anyway.** A cycle in `depends_on` is a plan bug that must
//!   be reported to the agent at publish time with the cycle named — and cycle
//!   detection is most of a layering pass. Doing the layering too is nearly free.
//! * **A layout crash in the browser blanks the review surface.** Here it is a typed
//!   error and a 400.
//!
//! # The algorithm
//!
//! Longest-path layering: a step's layer is one more than the deepest layer among its
//! dependencies, so every edge points strictly downward and a step never sits above
//! something it waits on. Within a layer, order is `(the smallest order among the
//! step's dependencies, then document order)` — which pulls a step under its earliest
//! parent instead of leaving edges crossing the whole width, without needing an
//! iterative crossing-minimization pass whose result would depend on iteration count.
//! Steps in layer 0 have no dependencies by construction, so they order by document
//! position alone.
//!
//! Every part of that is deterministic. Same steps in, same `(layer, order)` out — a
//! property the tests pin, because "the graph rearranged itself between two loads of
//! the same plan" is exactly the kind of thing nobody reports and everybody distrusts.

use std::collections::HashMap;

use serde::Serialize;

use crate::model::Step;

/// Where one step sits in the layered drawing. `layer` is the row (0 at the top),
/// `order` the position within that row (0 at the left).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Placement {
    pub step_id: String,
    pub layer: u32,
    pub order: u32,
}

/// Why a step graph cannot be drawn. Every variant names the steps involved: an error
/// that says "there is a cycle" and nothing else leaves the agent guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    /// Two steps share an id. Ids address annotations, so a duplicate would make the
    /// reviewer's mark ambiguous.
    DuplicateStep(String),
    /// A `depends_on` entry names nothing. Only reachable from caller-supplied steps —
    /// the markdown derivation drops references it cannot resolve.
    UnknownDependency { step: String, depends_on: String },
    /// The steps, in cycle order, with the first repeated at the end so the loop reads
    /// as a loop: `a -> b -> c -> a`.
    Cycle(Vec<String>),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::DuplicateStep(id) => write!(
                f,
                "two steps share the id '{id}' — step ids anchor annotations, so they must be unique"
            ),
            GraphError::UnknownDependency { step, depends_on } => write!(
                f,
                "step '{step}' depends on '{depends_on}', which is not a step in this plan"
            ),
            GraphError::Cycle(path) => write!(
                f,
                "the steps depend on each other in a cycle: {} — a plan whose steps \
                 wait on each other can never start",
                path.join(" -> ")
            ),
        }
    }
}

impl std::error::Error for GraphError {}

/// Validate the DAG and lay it out.
///
/// Returns placements sorted by `(layer, order)`, which is also the order the UI wants
/// to iterate for rendering.
///
/// # Errors
/// [`GraphError`] when ids collide, a dependency names nothing, or the steps form a
/// cycle.
pub fn layout(steps: &[Step]) -> Result<Vec<Placement>, GraphError> {
    let index = index_of(steps)?;
    let deps = resolve_edges(steps, &index)?;
    let layers = layer_steps(steps, &deps)?;
    Ok(order_within_layers(steps, &deps, &layers))
}

/// Validate the DAG without building a layout, for callers that only care whether the
/// plan is publishable.
///
/// # Errors
/// Same as [`layout`].
pub fn validate(steps: &[Step]) -> Result<(), GraphError> {
    layout(steps).map(|_| ())
}

fn index_of(steps: &[Step]) -> Result<HashMap<&str, usize>, GraphError> {
    let mut index = HashMap::with_capacity(steps.len());
    for (idx, step) in steps.iter().enumerate() {
        if index.insert(step.id.as_str(), idx).is_some() {
            return Err(GraphError::DuplicateStep(step.id.clone()));
        }
    }
    Ok(index)
}

/// `depends_on` as indices. Self-edges are reported as a one-step cycle rather than
/// slipping through as a step that is its own prerequisite.
fn resolve_edges(
    steps: &[Step],
    index: &HashMap<&str, usize>,
) -> Result<Vec<Vec<usize>>, GraphError> {
    let mut out = Vec::with_capacity(steps.len());
    for (idx, step) in steps.iter().enumerate() {
        let mut edges = Vec::with_capacity(step.depends_on.len());
        for dep in &step.depends_on {
            let Some(&target) = index.get(dep.as_str()) else {
                return Err(GraphError::UnknownDependency {
                    step: step.id.clone(),
                    depends_on: dep.clone(),
                });
            };
            if target == idx {
                return Err(GraphError::Cycle(vec![step.id.clone(), step.id.clone()]));
            }
            if !edges.contains(&target) {
                edges.push(target);
            }
        }
        out.push(edges);
    }
    Ok(out)
}

/// Iterative depth-first longest-path layering.
///
/// Iterative rather than recursive on purpose: `depends_on` arrives over HTTP from an
/// agent, and a thousand-step chain must produce a plan, not a blown stack in a
/// handler.
fn layer_steps(steps: &[Step], deps: &[Vec<usize>]) -> Result<Vec<u32>, GraphError> {
    const WHITE: u8 = 0;
    const GRAY: u8 = 1;
    const BLACK: u8 = 2;

    let mut color = vec![WHITE; steps.len()];
    let mut layer = vec![0u32; steps.len()];

    for root in 0..steps.len() {
        if color[root] != WHITE {
            continue;
        }
        // (node, how many of its dependencies we have already walked)
        let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
        color[root] = GRAY;

        while let Some((node, cursor)) = stack.pop() {
            if cursor < deps[node].len() {
                stack.push((node, cursor + 1));
                let next = deps[node][cursor];
                match color[next] {
                    WHITE => {
                        color[next] = GRAY;
                        stack.push((next, 0));
                    }
                    // A gray node is still on the stack: we have walked back into the
                    // path we came from, which is the definition of a cycle.
                    GRAY => return Err(GraphError::Cycle(cycle_path(&stack, next, steps))),
                    _ => {}
                }
                continue;
            }
            // All dependencies settled: this node's layer is one past the deepest.
            layer[node] = deps[node]
                .iter()
                .map(|d| layer[*d] + 1)
                .max()
                .unwrap_or(0);
            color[node] = BLACK;
        }
    }
    Ok(layer)
}

/// Name the cycle we just walked into: the suffix of the current path starting at the
/// node we re-entered, closed by repeating it.
fn cycle_path(stack: &[(usize, usize)], reentered: usize, steps: &[Step]) -> Vec<String> {
    let start = stack
        .iter()
        .position(|(node, _)| *node == reentered)
        .unwrap_or(0);
    let mut path: Vec<String> = stack[start..]
        .iter()
        .map(|(node, _)| steps[*node].id.clone())
        .collect();
    if let Some(first) = path.first().cloned() {
        path.push(first);
    }
    path
}

fn order_within_layers(steps: &[Step], deps: &[Vec<usize>], layers: &[u32]) -> Vec<Placement> {
    if steps.is_empty() {
        return Vec::new();
    }
    let max_layer = layers.iter().copied().max().unwrap_or(0) as usize;
    // Bucket once rather than rescanning every step per layer: a 5 000-step plan is a
    // legal input, and the quadratic form of this loop turns it into seconds of CPU
    // inside a request handler.
    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); max_layer + 1];
    for (idx, layer) in layers.iter().enumerate() {
        buckets[*layer as usize].push(idx);
    }

    // `order[i]` is filled layer by layer, and every dependency of a layer-N step sits
    // in a layer below N, so a parent's order is always known before its child needs it.
    let mut order = vec![0u32; steps.len()];
    let mut out: Vec<Placement> = Vec::with_capacity(steps.len());

    for (current, members) in buckets.iter_mut().enumerate() {
        members.sort_by_key(|i| {
            let anchor = deps[*i].iter().map(|d| order[*d]).min();
            // Document order breaks every tie, which is what makes this stable.
            (anchor.unwrap_or(0), *i)
        });
        for (position, member) in members.iter().enumerate() {
            order[*member] = position as u32;
            out.push(Placement {
                step_id: steps[*member].id.clone(),
                layer: current as u32,
                order: position as u32,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StepStatus;

    fn step(id: &str, deps: &[&str]) -> Step {
        Step {
            id: id.into(),
            title: id.into(),
            summary: None,
            depends_on: deps.iter().map(|d| (*d).to_owned()).collect(),
            files: Vec::new(),
            status: StepStatus::Todo,
            risk: None,
        }
    }

    fn placed(steps: &[Step]) -> Vec<(String, u32, u32)> {
        layout(steps)
            .expect("lays out")
            .into_iter()
            .map(|p| (p.step_id, p.layer, p.order))
            .collect()
    }

    #[test]
    fn a_chain_becomes_one_step_per_layer() {
        let steps = vec![step("a", &[]), step("b", &["a"]), step("c", &["b"])];
        assert_eq!(
            placed(&steps),
            vec![
                ("a".into(), 0, 0),
                ("b".into(), 1, 0),
                ("c".into(), 2, 0),
            ]
        );
    }

    #[test]
    fn independent_steps_share_layer_zero_in_document_order() {
        let steps = vec![step("a", &[]), step("b", &[]), step("c", &[])];
        assert_eq!(
            placed(&steps),
            vec![
                ("a".into(), 0, 0),
                ("b".into(), 0, 1),
                ("c".into(), 0, 2),
            ]
        );
    }

    #[test]
    fn a_diamond_puts_the_join_below_both_branches() {
        // a -> b, a -> c, b -> d, c -> d
        let steps = vec![
            step("a", &[]),
            step("b", &["a"]),
            step("c", &["a"]),
            step("d", &["b", "c"]),
        ];
        assert_eq!(
            placed(&steps),
            vec![
                ("a".into(), 0, 0),
                ("b".into(), 1, 0),
                ("c".into(), 1, 1),
                ("d".into(), 2, 0),
            ]
        );
    }

    #[test]
    fn longest_path_wins_so_no_edge_ever_points_sideways() {
        // `d` depends on `a` directly AND through b -> c. With shortest-path layering
        // it would land in layer 1 alongside `b`, drawing an edge from c (layer 2)
        // upward into it. Longest-path pushes it to 3.
        let steps = vec![
            step("a", &[]),
            step("b", &["a"]),
            step("c", &["b"]),
            step("d", &["a", "c"]),
        ];
        let layers: HashMap<String, u32> = layout(&steps)
            .expect("lays out")
            .into_iter()
            .map(|p| (p.step_id, p.layer))
            .collect();
        assert_eq!(layers["d"], 3);
        for s in &steps {
            for dep in &s.depends_on {
                assert!(
                    layers[dep] < layers[&s.id],
                    "edge {dep} -> {} points the wrong way",
                    s.id
                );
            }
        }
    }

    #[test]
    fn in_layer_order_follows_the_earliest_parent_then_document_order() {
        // Two roots. `x` hangs off the second root, `y` off the first — so `y` must be
        // drawn left of `x` even though it comes later in the document.
        let steps = vec![
            step("root1", &[]),
            step("root2", &[]),
            step("x", &["root2"]),
            step("y", &["root1"]),
        ];
        let placements = placed(&steps);
        assert_eq!(placements[0], ("root1".into(), 0, 0));
        assert_eq!(placements[1], ("root2".into(), 0, 1));
        assert_eq!(placements[2], ("y".into(), 1, 0));
        assert_eq!(placements[3], ("x".into(), 1, 1));
    }

    #[test]
    fn the_layout_is_identical_across_runs() {
        let steps = vec![
            step("a", &[]),
            step("b", &[]),
            step("c", &["a", "b"]),
            step("d", &["a"]),
            step("e", &["c", "d"]),
        ];
        let first = layout(&steps).expect("lays out");
        for _ in 0..25 {
            assert_eq!(
                layout(&steps).expect("lays out"),
                first,
                "the same steps must always draw the same picture — a graph that \
                 rearranges itself between loads is one nobody trusts"
            );
        }
    }

    #[test]
    fn a_cycle_is_rejected_and_names_its_members() {
        let steps = vec![
            step("a", &["c"]),
            step("b", &["a"]),
            step("c", &["b"]),
            step("unrelated", &[]),
        ];
        let err = layout(&steps).expect_err("a cycle cannot be laid out");
        let GraphError::Cycle(path) = err else {
            panic!("expected a cycle, got {err:?}");
        };
        assert_eq!(path.first(), path.last(), "the path must close the loop");
        for id in ["a", "b", "c"] {
            assert!(path.iter().any(|p| p == id), "cycle {path:?} omits {id}");
        }
        assert!(
            !path.iter().any(|p| p == "unrelated"),
            "a bystander step must not be blamed for the cycle: {path:?}"
        );
    }

    #[test]
    fn a_step_that_depends_on_itself_is_a_cycle_not_a_root() {
        let err = layout(&[step("a", &["a"])]).expect_err("self-edge");
        assert!(matches!(err, GraphError::Cycle(_)), "got {err:?}");
    }

    #[test]
    fn an_unknown_dependency_names_both_ends() {
        let err = layout(&[step("a", &["ghost"])]).expect_err("dangling edge");
        assert_eq!(
            err,
            GraphError::UnknownDependency {
                step: "a".into(),
                depends_on: "ghost".into()
            }
        );
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn duplicate_ids_are_rejected_because_annotations_anchor_to_them() {
        let err = layout(&[step("a", &[]), step("a", &[])]).expect_err("duplicate");
        assert_eq!(err, GraphError::DuplicateStep("a".into()));
    }

    #[test]
    fn a_repeated_dependency_is_not_a_duplicate_edge() {
        let steps = vec![step("a", &[]), step("b", &["a", "a"])];
        assert_eq!(
            placed(&steps),
            vec![("a".into(), 0, 0), ("b".into(), 1, 0)]
        );
    }

    #[test]
    fn no_steps_is_an_empty_layout_not_an_error() {
        assert!(layout(&[]).expect("empty is fine").is_empty());
    }

    #[test]
    fn a_deep_chain_does_not_blow_the_stack() {
        // `depends_on` arrives over HTTP; a recursive walk would turn a long plan into
        // a segfault inside a request handler.
        let mut steps = vec![step("s0", &[])];
        for i in 1..5_000 {
            let prev = format!("s{}", i - 1);
            steps.push(step(&format!("s{i}"), &[prev.as_str()]));
        }
        let placements = layout(&steps).expect("lays out");
        assert_eq!(placements.len(), 5_000);
        assert_eq!(placements[4_999].layer, 4_999);
    }
}
