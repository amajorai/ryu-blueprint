use std::collections::BTreeSet;

use ryu_blueprint::graph::{self, GraphError};
use ryu_blueprint::model::{Step, StepStatus};
use serde::Deserialize;

const FIXTURE: &str = include_str!("../fixtures/conformance.json");

#[derive(Debug, Deserialize)]
struct FixtureFile {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    cases: Vec<FixtureCase>,
}

#[derive(Debug, Deserialize)]
struct FixtureCase {
    id: String,
    expected: String,
    steps: Vec<FixtureStep>,
    #[serde(default)]
    placements: Vec<FixturePlacement>,
    #[serde(default)]
    cycle_contains: Vec<String>,
    #[serde(default)]
    error: Option<FixtureError>,
}

#[derive(Debug, Deserialize)]
struct FixtureStep {
    id: String,
    #[serde(default)]
    depends_on: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct FixturePlacement {
    step_id: String,
    layer: u32,
    order: u32,
}

#[derive(Debug, Deserialize)]
struct FixtureError {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    step: Option<String>,
    #[serde(default)]
    depends_on: Option<String>,
}

fn steps(case: &FixtureCase) -> Vec<Step> {
    case.steps
        .iter()
        .map(|step| Step {
            id: step.id.clone(),
            title: step.id.clone(),
            summary: None,
            depends_on: step.depends_on.clone(),
            files: Vec::new(),
            status: StepStatus::Todo,
            risk: None,
        })
        .collect()
}

#[test]
fn production_blueprint_graph_matches_the_shared_conformance_fixture() {
    let fixture: FixtureFile = serde_json::from_str(FIXTURE).expect("valid conformance fixture");
    assert_eq!(fixture.schema_version, 1);
    assert_eq!(fixture.cases.len(), 5);

    let mut ids = BTreeSet::new();
    for case in fixture.cases {
        assert!(
            ids.insert(case.id.clone()),
            "duplicate fixture id: {}",
            case.id
        );
        let result = graph::layout(&steps(&case));
        match case.expected.as_str() {
            "layout" => {
                let placements: Vec<FixturePlacement> = result
                    .expect("fixture layout must be valid")
                    .into_iter()
                    .map(|placement| FixturePlacement {
                        step_id: placement.step_id,
                        layer: placement.layer,
                        order: placement.order,
                    })
                    .collect();
                assert_eq!(placements, case.placements, "{}", case.id);
            }
            "cycle" => {
                let GraphError::Cycle(path) = result.expect_err("fixture cycle must be rejected")
                else {
                    panic!("{} must produce a cycle", case.id);
                };
                let members: BTreeSet<_> = path.into_iter().collect();
                let expected: BTreeSet<_> = case.cycle_contains.into_iter().collect();
                assert!(expected.is_subset(&members), "{}: {members:?}", case.id);
            }
            "unknown_dependency" => {
                let GraphError::UnknownDependency { step, depends_on } =
                    result.expect_err("fixture dangling edge must be rejected")
                else {
                    panic!("{} must produce an unknown dependency", case.id);
                };
                let error = case.error.expect("unknown dependency error details");
                assert_eq!(Some(step), error.step);
                assert_eq!(Some(depends_on), error.depends_on);
            }
            "duplicate_step" => {
                let GraphError::DuplicateStep(id) =
                    result.expect_err("fixture duplicate must be rejected")
                else {
                    panic!("{} must produce a duplicate step", case.id);
                };
                assert_eq!(Some(id), case.error.expect("duplicate error details").id);
            }
            expected => panic!("unsupported fixture expectation: {expected}"),
        }
    }
}
