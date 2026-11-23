use std::collections::{BTreeMap, VecDeque};

use rcmake_base::{BString, Limits, truth};
use rcmake_regex::Regex;

use crate::discovery::Test;
use crate::{CtestError, CtestOptions};

pub(crate) struct PlannedTest {
    pub(crate) number: usize,
    pub(crate) test: Test,
    pub(crate) checks: OutputChecks,
    pub(crate) cleanup: bool,
    pub(crate) required_setups: Vec<usize>,
}

impl std::ops::Deref for PlannedTest {
    type Target = Test;

    fn deref(&self) -> &Self::Target {
        &self.test
    }
}

pub(crate) struct Plan {
    pub(crate) tests: Vec<PlannedTest>,
    pub(crate) dependents: Vec<Vec<usize>>,
    pub(crate) prerequisites: Vec<usize>,
}

impl Plan {
    pub(crate) fn is_acyclic(&self) -> bool {
        let mut remaining = self.prerequisites.clone();
        let mut ready: VecDeque<_> = remaining
            .iter()
            .enumerate()
            .filter_map(|(id, &count)| (count == 0).then_some(id))
            .collect();
        let mut visited = 0;
        while let Some(id) = ready.pop_front() {
            visited += 1;
            ready.extend(self.complete(id, &mut remaining));
        }
        visited == self.tests.len()
    }

    pub(crate) fn complete<'a>(
        &'a self,
        id: usize,
        remaining: &'a mut [usize],
    ) -> impl Iterator<Item = usize> + 'a {
        self.dependents
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(move |&next| {
                let count = remaining.get_mut(next)?;
                *count -= 1;
                (*count == 0).then_some(next)
            })
    }

    fn new(tests: Vec<PlannedTest>, edges: Vec<Vec<usize>>) -> Self {
        let mut dependents = vec![Vec::new(); tests.len()];
        let prerequisites = edges.iter().map(Vec::len).collect();
        for (id, deps) in edges.into_iter().enumerate() {
            for dep in deps {
                if let Some(children) = dependents.get_mut(dep) {
                    children.push(id);
                }
            }
        }
        Self {
            tests,
            dependents,
            prerequisites,
        }
    }
}

fn compile(pattern: &BString) -> Result<Regex, CtestError> {
    Regex::new(pattern.as_slice()).map_err(|_| CtestError::BadRegex(pattern.clone()))
}

fn matches(pattern: &Regex, value: &[u8]) -> Result<bool, CtestError> {
    let mut budget = Limits::default().regex_steps;
    pattern
        .find_at(value, 0, Default::default(), &mut budget)
        .map(|result| result.is_some())
        .map_err(|_| CtestError::RegexLimit)
}

pub(crate) fn any_match(patterns: &[Regex], value: &[u8]) -> Result<bool, CtestError> {
    for pattern in patterns {
        if matches(pattern, value)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) struct OutputChecks {
    pub(crate) pass: Vec<Regex>,
    pub(crate) fail: Vec<Regex>,
    pub(crate) skip: Vec<Regex>,
}

impl OutputChecks {
    fn new(test: &Test) -> Result<Self, CtestError> {
        let compile_list = |key: &[u8]| {
            test.list(key)
                .iter()
                .map(compile)
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Self {
            pass: compile_list(b"PASS_REGULAR_EXPRESSION")?,
            fail: compile_list(b"FAIL_REGULAR_EXPRESSION")?,
            skip: compile_list(b"SKIP_REGULAR_EXPRESSION")?,
        })
    }
}

struct Filters {
    include: Option<Regex>,
    exclude: Option<Regex>,
    include_labels: Vec<Regex>,
    exclude_labels: Vec<Regex>,
}

impl Filters {
    fn new(options: &CtestOptions) -> Result<Self, CtestError> {
        Ok(Self {
            include: options.include.as_ref().map(compile).transpose()?,
            exclude: options.exclude.as_ref().map(compile).transpose()?,
            include_labels: options
                .include_labels
                .iter()
                .map(compile)
                .collect::<Result<_, _>>()?,
            exclude_labels: options
                .exclude_labels
                .iter()
                .map(compile)
                .collect::<Result<_, _>>()?,
        })
    }

    fn includes(&self, test: &Test) -> Result<bool, CtestError> {
        if let Some(pattern) = &self.include
            && !matches(pattern, &test.name)?
        {
            return Ok(false);
        }
        if let Some(pattern) = &self.exclude
            && matches(pattern, &test.name)?
        {
            return Ok(false);
        }
        let labels = test.list(b"LABELS");
        Ok(labels_match(&self.include_labels, &labels)?
            && (self.exclude_labels.is_empty() || !labels_match(&self.exclude_labels, &labels)?))
    }
}

fn labels_match(patterns: &[Regex], labels: &[BString]) -> Result<bool, CtestError> {
    for pattern in patterns {
        let mut matched = false;
        for label in labels {
            if matches(pattern, label)? {
                matched = true;
                break;
            }
        }
        if !matched {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Default)]
struct Fixture {
    setups: Vec<usize>,
    users: Vec<usize>,
    cleanups: Vec<usize>,
}

type TestIndex = BTreeMap<BString, Vec<usize>>;

fn fixture_index(tests: impl Iterator<Item = (usize, Vec<BString>)>) -> TestIndex {
    let mut index = TestIndex::new();
    for (id, names) in tests {
        for name in names {
            index.entry(name).or_default().push(id);
        }
    }
    index
}

fn fixture_selection(tests: &[(usize, Test)], selected: &mut [bool]) {
    let helpers = fixture_index(tests.iter().enumerate().map(|(id, (_, test))| {
        let mut fixtures = test.list(b"FIXTURES_SETUP");
        fixtures.extend(test.list(b"FIXTURES_CLEANUP"));
        (id, fixtures)
    }));
    let mut queue: VecDeque<_> = selected
        .iter()
        .enumerate()
        .filter_map(|(id, &selected)| selected.then_some(id))
        .collect();
    let mut visited = std::collections::BTreeSet::new();
    while let Some(id) = queue.pop_front() {
        let Some((_, test)) = tests.get(id) else {
            continue;
        };
        if truth::is_on(test.property(b"DISABLED")) {
            continue;
        }
        for name in test.list(b"FIXTURES_REQUIRED") {
            if !visited.insert(name.clone()) {
                continue;
            }
            for &helper in helpers.get(&name).into_iter().flatten() {
                if let Some(chosen) = selected.get_mut(helper)
                    && !*chosen
                {
                    *chosen = true;
                    queue.push_back(helper);
                }
            }
        }
    }
}

fn build_graph(mut tests: Vec<PlannedTest>) -> Plan {
    let mut names = TestIndex::new();
    let mut fixtures = BTreeMap::<BString, Fixture>::new();
    for (id, test) in tests.iter().enumerate() {
        names.entry(test.name.clone()).or_default().push(id);
        for name in test.list(b"FIXTURES_SETUP") {
            fixtures.entry(name).or_default().setups.push(id);
        }
        for name in test.list(b"FIXTURES_REQUIRED") {
            fixtures.entry(name).or_default().users.push(id);
        }
        for name in test.list(b"FIXTURES_CLEANUP") {
            fixtures.entry(name).or_default().cleanups.push(id);
        }
    }

    let mut cleanup_dependencies = vec![Vec::new(); tests.len()];
    for fixture in fixtures.values() {
        for &cleanup in &fixture.cleanups {
            if let Some(deps) = cleanup_dependencies.get_mut(cleanup) {
                deps.extend(
                    fixture
                        .setups
                        .iter()
                        .chain(&fixture.users)
                        .copied()
                        .filter(|&id| id != cleanup),
                );
            }
        }
    }

    let mut edges = Vec::with_capacity(tests.len());
    for (id, (test, mut deps)) in tests.iter_mut().zip(cleanup_dependencies).enumerate() {
        for name in test.list(b"DEPENDS") {
            deps.extend(names.get(&name).into_iter().flatten().copied());
        }
        let mut setups = Vec::new();
        for name in test.list(b"FIXTURES_REQUIRED") {
            if let Some(fixture) = fixtures.get(&name) {
                setups.extend(fixture.setups.iter().copied().filter(|&setup| setup != id));
            }
        }
        setups.sort_unstable();
        setups.dedup();
        deps.extend(&setups);
        deps.sort_unstable();
        deps.dedup();
        test.required_setups = setups;
        edges.push(deps);
    }
    Plan::new(tests, edges)
}

pub(crate) fn plan(tests: Vec<Test>, options: &CtestOptions) -> Result<Plan, CtestError> {
    let filters = Filters::new(options)?;
    let tests: Vec<_> = tests
        .into_iter()
        .enumerate()
        .filter(|(_, test)| {
            let configurations = test.list(b"CONFIGURATIONS");
            configurations.is_empty()
                || options.config.as_ref().is_some_and(|config| {
                    configurations
                        .iter()
                        .any(|candidate| candidate.eq_ignore_ascii_case(config))
                })
        })
        .collect();
    let mut selected = tests
        .iter()
        .map(|(_, test)| filters.includes(test))
        .collect::<Result<Vec<_>, _>>()?;
    fixture_selection(&tests, &mut selected);

    let tests = tests
        .into_iter()
        .zip(selected)
        .filter(|(_, selected)| *selected)
        .map(|((index, test), _)| {
            Ok(PlannedTest {
                number: index + 1,
                checks: OutputChecks::new(&test)?,
                cleanup: !test.list(b"FIXTURES_CLEANUP").is_empty(),
                test,
                required_setups: Vec::new(),
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(build_graph(tests))
}
