use std::sync::Arc;
use std::time::Duration;

use rcmake_host::PureHost;

use super::*;
use crate::discovery;
use crate::state::{CostData, CostEntry, FailedTest};

fn tree(source: &str) -> Result<Arc<PureHost>, CtestError> {
    let host = Arc::new(PureHost::default());
    seed(&host, "CTestTestfile.cmake", source.as_bytes())?;
    Ok(host)
}

fn seed(host: &PureHost, path: &str, contents: &[u8]) -> Result<(), CtestError> {
    let path = Path::new("/work").join(path);
    host.seed_file(&path, contents)
        .map_err(|source| CtestError::Access { path, source })
}

fn select(
    host: &Arc<PureHost>,
    options: &CtestOptions,
    selection: &SelectionOptions,
) -> Result<Plan, CtestError> {
    let dir = Path::new("/work");
    let tests = discovery::load(host.clone(), dir, options.config.as_ref())?;
    plan_with(host.as_ref(), dir, tests, options, selection)
}

fn chosen(plan: &Plan) -> Vec<(usize, &BString)> {
    plan.tests
        .iter()
        .map(|test| (test.number, &test.name))
        .collect()
}

fn assert_selection(plan: &Plan, expected: &[(usize, &str)]) {
    let actual: Vec<_> = chosen(plan)
        .into_iter()
        .map(|(number, name)| (number, name.to_string()))
        .collect();
    let expected: Vec<_> = expected
        .iter()
        .map(|&(number, name)| (number, name.to_owned()))
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn indexes_filtered_list_and_uses_discovery_numbers_for_union() -> Result<(), CtestError> {
    let host = tree(
        "add_test(a run)\nadd_test(b run)\nadd_test(c run)\nadd_test(d run)\nadd_test(e run)\n",
    )?;
    let options = CtestOptions {
        include: Some("[ace]".into()),
        show_only: true,
        ..CtestOptions::default()
    };
    let mut selection = SelectionOptions {
        indices: Some(IndexSource::Argument("2,2".into())),
        ..SelectionOptions::default()
    };
    assert_selection(&select(&host, &options, &selection)?, &[(3, "c")]);
    selection.union = true;
    assert_selection(
        &select(&host, &options, &selection)?,
        &[(1, "a"), (2, "b"), (3, "c"), (5, "e")],
    );
    let options = CtestOptions {
        include: None,
        ..options
    };
    selection.union = false;
    selection.indices = Some(IndexSource::Argument("1,5,1.5,5".into()));
    assert_selection(
        &select(&host, &options, &selection)?,
        &[(1, "a"), (2, "b"), (4, "d"), (5, "e")],
    );
    selection.indices = Some(IndexSource::Argument("bad".into()));
    assert_selection(
        &select(&host, &options, &selection)?,
        &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")],
    );
    Ok(())
}

#[test]
fn reads_index_files_and_filters_exact_names() -> Result<(), CtestError> {
    let host =
        tree("add_test(a run)\nadd_test(\"b test\" run)\nadd_test(c run)\nadd_test(d run)\n")?;
    seed(&host, "index.txt", b"2,4\r\n1,1\r\n")?;
    seed(&host, "include.txt", b"a\r\nb test\r\nc\r\n d\r\n")?;
    seed(&host, "exclude.txt", b"c\n")?;
    let mut selection = SelectionOptions {
        indices: Some(IndexSource::Argument("index.txt".into())),
        tests_from_file: Some("include.txt".into()),
        exclude_from_file: Some("exclude.txt".into()),
        ..SelectionOptions::default()
    };
    assert_selection(
        &select(&host, &CtestOptions::default(), &selection)?,
        &[(2, "b test")],
    );
    // CTest uses the last byte of an unterminated line for its string terminator.
    seed(&host, "index.txt", b"2,2")?;
    selection.tests_from_file = None;
    selection.exclude_from_file = None;
    assert_selection(
        &select(&host, &CtestOptions::default(), &selection)?,
        &[(2, "b test"), (3, "c"), (4, "d")],
    );
    selection.indices = Some(IndexSource::Argument(
        format!(",,0,{}2", "2,".repeat(300)).into(),
    ));
    assert_selection(
        &select(&host, &CtestOptions::default(), &selection)?,
        &[(2, "b test")],
    );
    Ok(())
}

#[test]
fn rerun_matches_indices_and_skips_other_filters() -> Result<(), CtestError> {
    let host = tree("add_test(a run)\nadd_test(b run)\nadd_test(c run)\n")?;
    state::write_failed(
        host.as_ref(),
        Path::new("/work"),
        &[FailedTest {
            number: 2,
            name: "old name".into(),
        }],
    )?;
    let options = CtestOptions {
        include: Some("[".into()),
        exclude: Some("b".into()),
        include_labels: vec!["[".into()],
        exclude_labels: vec!["[".into()],
        ..CtestOptions::default()
    };
    let selection = SelectionOptions {
        rerun_failed: true,
        out_of_date: true,
        indices: Some(IndexSource::Argument("1,1".into())),
        tests_from_file: Some("missing.txt".into()),
        ..SelectionOptions::default()
    };
    assert_selection(&select(&host, &options, &selection)?, &[(2, "b")]);
    Ok(())
}

#[test]
fn fixture_filters_apply_only_to_automatic_tests() -> Result<(), CtestError> {
    let host = tree(
        r#"
        add_test(setup run)
        add_test(cleanup run)
        add_test(user run)
        set_tests_properties(setup PROPERTIES FIXTURES_SETUP database)
        set_tests_properties(cleanup PROPERTIES FIXTURES_CLEANUP database)
        set_tests_properties(user PROPERTIES FIXTURES_REQUIRED database)
    "#,
    )?;
    let mut options = CtestOptions {
        include: Some("^user$".into()),
        ..CtestOptions::default()
    };
    let mut selection = SelectionOptions::default();
    assert_selection(
        &select(&host, &options, &selection)?,
        &[(1, "setup"), (2, "cleanup"), (3, "user")],
    );
    selection.exclude_fixture_setup = Some("^data".into());
    assert_selection(
        &select(&host, &options, &selection)?,
        &[(2, "cleanup"), (3, "user")],
    );
    selection.exclude_fixture_setup = None;
    selection.exclude_fixture_cleanup = Some("base$".into());
    assert_selection(
        &select(&host, &options, &selection)?,
        &[(1, "setup"), (3, "user")],
    );
    selection.exclude_fixture = Some("database".into());
    assert_selection(&select(&host, &options, &selection)?, &[(3, "user")]);
    options.include = Some("setup|user".into());
    assert_selection(
        &select(&host, &options, &selection)?,
        &[(1, "setup"), (3, "user")],
    );
    Ok(())
}

#[test]
fn selects_tests_with_missing_or_newer_build_dependencies() -> Result<(), CtestError> {
    let host = tree(
        r#"
        add_test(tracked run)
        add_test(untracked run)
        set_tests_properties(tracked PROPERTIES _CMAKE_TEST_BUILD_DEPENDS "dependency;/work/other")
    "#,
    )?;
    let dir = Path::new("/work");
    let options = CtestOptions {
        show_only: true,
        ..CtestOptions::default()
    };
    let selection = SelectionOptions {
        out_of_date: true,
        ..SelectionOptions::default()
    };
    assert_selection(&select(&host, &options, &selection)?, &[(1, "tracked")]);
    seed(&host, "dependency", b"first")?;
    seed(&host, "other", b"first")?;
    host.sleep(Duration::from_secs(1));
    state::write_stamp(host.as_ref(), dir, b"tracked")?;
    assert_selection(&select(&host, &options, &selection)?, &[]);
    host.sleep(Duration::from_secs(1));
    seed(&host, "other", b"changed")?;
    assert_selection(&select(&host, &options, &selection)?, &[(1, "tracked")]);
    Ok(())
}

#[test]
fn orders_by_cost_and_seed_without_changing_test_numbers() -> Result<(), CtestError> {
    let host = tree(
        r#"
        add_test(a run)
        add_test(b run)
        add_test(c run)
        add_test(d run)
        add_test(e run)
        set_tests_properties(a PROPERTIES COST 10)
        set_tests_properties(c PROPERTIES DEPENDS a)
    "#,
    )?;
    let costs = CostData {
        tests: BTreeMap::from([
            (
                "a".into(),
                CostEntry {
                    runs: 2,
                    average: 1.0,
                },
            ),
            (
                "b".into(),
                CostEntry {
                    runs: 2,
                    average: 2.0,
                },
            ),
            (
                "c".into(),
                CostEntry {
                    runs: 2,
                    average: 100.0,
                },
            ),
        ]),
        failed: vec!["d".into()],
    };
    state::write_costs(host.as_ref(), Path::new("/work"), &costs)?;
    let mut options = CtestOptions {
        jobs: 4,
        ..CtestOptions::default()
    };
    let mut selection = SelectionOptions::default();
    let plan = select(&host, &options, &selection)?;
    assert_selection(&plan, &[(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")]);
    assert_eq!(plan.schedule_order, [3, 0, 2, 1, 4]);
    options.jobs = 1;
    assert_eq!(
        select(&host, &options, &selection)?.schedule_order,
        [0, 1, 2, 3, 4]
    );
    selection.schedule_random = true;
    selection.random_seed = Some(17);
    let first = select(&host, &options, &selection)?.schedule_order;
    assert_eq!(select(&host, &options, &selection)?.schedule_order, first);
    selection.random_seed = Some(18);
    assert_ne!(select(&host, &options, &selection)?.schedule_order, first);
    assert!(first.iter().position(|&id| id == 0) < first.iter().position(|&id| id == 2));
    Ok(())
}
