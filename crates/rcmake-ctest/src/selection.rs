//! Test selection, dependencies, and scheduling priority.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use rcmake_base::{BString, Limits, truth};
use rcmake_host::{FileKind, Follow, Host, path::to_native};
use rcmake_presets::TestIndex as PresetIndex;
use rcmake_regex::Regex;

use crate::discovery::Test;
use crate::{CtestError, CtestOptions, state};

/// Index settings from the command line or a test preset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IndexSource {
    Argument(BString),
    Preset(PresetIndex),
}

/// Selection and priority settings beyond the basic runner options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SelectionOptions {
    pub(crate) indices: Option<IndexSource>,
    pub(crate) union: bool,
    pub(crate) exclude_fixture: Option<BString>,
    pub(crate) exclude_fixture_setup: Option<BString>,
    pub(crate) exclude_fixture_cleanup: Option<BString>,
    pub(crate) tests_from_file: Option<PathBuf>,
    pub(crate) exclude_from_file: Option<PathBuf>,
    pub(crate) rerun_failed: bool,
    pub(crate) out_of_date: bool,
    pub(crate) schedule_random: bool,
    pub(crate) random_seed: Option<u32>,
}

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
    /// Plan indices in scheduling priority order.
    pub(crate) schedule_order: Vec<usize>,
}

impl Plan {
    pub(crate) fn dependency_cycle(&self) -> Option<&BString> {
        if let Some(id) = self
            .dependents
            .iter()
            .enumerate()
            .find_map(|(id, children)| children.contains(&id).then_some(id))
        {
            return self.tests.get(id).map(|test| &test.name);
        }
        let mut remaining = self.prerequisites.clone();
        let mut ready: VecDeque<_> = remaining
            .iter()
            .enumerate()
            .filter_map(|(id, &count)| (count == 0).then_some(id))
            .collect();
        while let Some(id) = ready.pop_front() {
            ready.extend(self.complete(id, &mut remaining));
        }
        self.tests
            .iter()
            .zip(remaining)
            .find_map(|(test, count)| (count > 0).then_some(&test.name))
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
            schedule_order: (0..tests.len()).collect(),
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

pub(crate) fn first_match(patterns: &[Regex], value: &[u8]) -> Result<Option<usize>, CtestError> {
    for (index, pattern) in patterns.iter().enumerate() {
        if matches(pattern, value)? {
            return Ok(Some(index));
        }
    }
    Ok(None)
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
            include: options
                .include
                .as_ref()
                .filter(|pattern| !pattern.is_empty())
                .map(compile)
                .transpose()?,
            exclude: options
                .exclude
                .as_ref()
                .filter(|pattern| !pattern.is_empty())
                .map(compile)
                .transpose()?,
            include_labels: options
                .include_labels
                .iter()
                .filter(|pattern| !pattern.is_empty())
                .map(compile)
                .collect::<Result<_, _>>()?,
            exclude_labels: options
                .exclude_labels
                .iter()
                .filter(|pattern| !pattern.is_empty())
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

#[derive(Default)]
struct FixtureFilters {
    any: Option<Regex>,
    setup: Option<Regex>,
    cleanup: Option<Regex>,
}

impl FixtureFilters {
    fn new(options: &SelectionOptions) -> Result<Self, CtestError> {
        let fixture_regex = |pattern: Option<&BString>| {
            pattern
                .filter(|pattern| !pattern.is_empty())
                .map(compile)
                .transpose()
        };
        Ok(Self {
            any: fixture_regex(options.exclude_fixture.as_ref())?,
            setup: fixture_regex(options.exclude_fixture_setup.as_ref())?,
            cleanup: fixture_regex(options.exclude_fixture_cleanup.as_ref())?,
        })
    }
}

fn fixture_selection(
    tests: &[(usize, Test)],
    selected: &mut [bool],
    filters: &FixtureFilters,
) -> Result<(), CtestError> {
    let helpers = |property| {
        fixture_index(
            tests
                .iter()
                .enumerate()
                .map(|(id, (_, test))| (id, test.list(property))),
        )
    };
    let setups = helpers(b"FIXTURES_SETUP");
    let cleanups = helpers(b"FIXTURES_CLEANUP");
    let mut queue: VecDeque<_> = selected
        .iter()
        .enumerate()
        .filter_map(|(id, &selected)| selected.then_some(id))
        .collect();
    let mut visited = BTreeSet::new();
    while let Some(id) = queue.pop_front() {
        let Some((_, test)) = tests.get(id) else {
            continue;
        };
        if truth::is_on(test.property(b"DISABLED")) {
            continue;
        }
        for name in test.list(b"FIXTURES_REQUIRED") {
            if !visited.insert(name.clone()) || match_optional(filters.any.as_ref(), &name)? {
                continue;
            }
            for (helpers, excluded) in [(&setups, &filters.setup), (&cleanups, &filters.cleanup)] {
                if match_optional(excluded.as_ref(), &name)? {
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
    Ok(())
}

fn match_optional(pattern: Option<&Regex>, value: &[u8]) -> Result<bool, CtestError> {
    pattern.map_or(Ok(false), |pattern| matches(pattern, value))
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
                deps.extend(fixture.setups.iter().chain(&fixture.users).copied());
            }
        }
    }

    let mut edges = Vec::with_capacity(tests.len());
    for (test, mut deps) in tests.iter_mut().zip(cleanup_dependencies) {
        for name in test.list(b"DEPENDS") {
            deps.extend(names.get(&name).into_iter().flatten().copied());
        }
        let mut setups = Vec::new();
        for name in test.list(b"FIXTURES_REQUIRED") {
            if let Some(fixture) = fixtures.get(&name) {
                setups.extend(fixture.setups.iter().copied());
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
    let tests = configured_tests(tests, options);
    let selected = tests
        .iter()
        .map(|(_, test)| filters.includes(test))
        .collect::<Result<_, _>>()?;
    finish_plan(tests, selected, &FixtureFilters::default())
}

/// Select tests with the full command line filters and set their scheduling order.
pub(crate) fn plan_with(
    host: &dyn Host,
    dir: &Path,
    tests: Vec<Test>,
    options: &CtestOptions,
    selection: &SelectionOptions,
) -> Result<Plan, CtestError> {
    let mut tests = configured_tests(tests, options);
    let selected = if selection.rerun_failed {
        let failures: BTreeSet<_> = state::last_failed(host, dir)?
            .into_iter()
            .map(|test| test.number)
            .collect();
        tests
            .iter()
            .map(|(number, _)| failures.is_empty() || failures.contains(number))
            .collect()
    } else {
        select_tests(host, dir, options, selection, &mut tests)?
    };
    let mut plan = finish_plan(tests, selected, &FixtureFilters::new(selection)?)?;
    if !options.show_only {
        plan.schedule_order = schedule_order(host, dir, &plan, options, selection)?;
    }
    Ok(plan)
}

fn configured_tests(tests: Vec<Test>, options: &CtestOptions) -> Vec<(usize, Test)> {
    tests
        .into_iter()
        .enumerate()
        .filter_map(|(index, test)| {
            let configurations = test.list(b"CONFIGURATIONS");
            let included = configurations.is_empty()
                || options.config.as_ref().is_some_and(|config| {
                    configurations
                        .iter()
                        .any(|candidate| candidate.eq_ignore_ascii_case(config))
                });
            included.then_some((index + 1, test))
        })
        .collect()
}

fn finish_plan(
    tests: Vec<(usize, Test)>,
    mut selected: Vec<bool>,
    fixture_filters: &FixtureFilters,
) -> Result<Plan, CtestError> {
    fixture_selection(&tests, &mut selected, fixture_filters)?;
    let tests = tests
        .into_iter()
        .zip(selected)
        .filter(|(_, selected)| *selected)
        .map(|((number, test), _)| {
            Ok(PlannedTest {
                number,
                checks: OutputChecks::new(&test)?,
                cleanup: !test.list(b"FIXTURES_CLEANUP").is_empty(),
                test,
                required_setups: Vec::new(),
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(build_graph(tests))
}

fn select_tests(
    host: &dyn Host,
    dir: &Path,
    options: &CtestOptions,
    selection: &SelectionOptions,
    tests: &mut Vec<(usize, Test)>,
) -> Result<Vec<bool>, CtestError> {
    let mut filters = Filters::new(options)?;
    // CTest removes name exclusions during discovery if there is no name inclusion filter.
    if filters.include.is_none()
        && let Some(exclude) = filters.exclude.take()
    {
        let mut kept = Vec::with_capacity(tests.len());
        let mut removed = 0;
        for (number, test) in std::mem::take(tests) {
            if matches(&exclude, &test.name)? {
                removed += 1;
            } else {
                kept.push((number - removed, test));
            }
        }
        *tests = kept;
    }
    let by_regex = tests
        .iter()
        .map(|(_, test)| filters.includes(test))
        .collect::<Result<Vec<_>, _>>()?;
    let index_count = if selection.union {
        tests.len()
    } else {
        by_regex.iter().filter(|&&included| included).count()
    };
    let indices = selection
        .indices
        .as_ref()
        .map(|source| index_filter(host, dir, source, index_count))
        .transpose()?
        .flatten();
    let from_file = test_names(host, dir, selection.tests_from_file.as_deref())?;
    let exclude_file = test_names(host, dir, selection.exclude_from_file.as_deref())?;
    let mut regex_number = 0;
    tests
        .iter()
        .zip(by_regex)
        .enumerate()
        .map(|(id, ((_, test), by_regex))| {
            regex_number += usize::from(by_regex);
            let by_index = indices.as_ref().map(|filter| {
                let number = if selection.union {
                    id + 1
                } else {
                    regex_number
                };
                filter.contains(number)
            });
            let included = if selection.union {
                by_regex || by_index.unwrap_or(true)
            } else {
                by_regex && by_index.unwrap_or(true)
            };
            if !included
                || from_file
                    .as_ref()
                    .is_some_and(|names| !names.contains(&test.name))
                || exclude_file
                    .as_ref()
                    .is_some_and(|names| names.contains(&test.name))
            {
                return Ok(false);
            }
            if selection.out_of_date {
                return out_of_date(host, dir, test);
            }
            Ok(true)
        })
        .collect()
}

fn test_names(
    host: &dyn Host,
    dir: &Path,
    file: Option<&Path>,
) -> Result<Option<BTreeSet<BString>>, CtestError> {
    let Some(file) = file else { return Ok(None) };
    let path = dir.join(file);
    let data = state::read_optional(host, &path)
        .ok()
        .flatten()
        .ok_or(CtestError::TestList(path))?;
    Ok(Some(
        state::lines(&data)
            .filter(|line| !line.is_empty())
            .map(Into::into)
            .collect(),
    ))
}

fn out_of_date(host: &dyn Host, dir: &Path, test: &Test) -> Result<bool, CtestError> {
    let dependencies = test.list(b"_CMAKE_TEST_BUILD_DEPENDS");
    if dependencies.is_empty() {
        return Ok(false);
    }
    let Some(stamp) = state::read_stamp(host, dir, &test.name)? else {
        return Ok(true);
    };
    for dependency in dependencies {
        let path = dir.join(to_native(&dependency));
        if state::modified(host, &path)?.is_none_or(|modified| modified >= stamp) {
            return Ok(true);
        }
    }
    Ok(false)
}

struct IndexFilter {
    start: i32,
    end: i32,
    stride: f64,
    explicit: BTreeSet<i32>,
}

impl IndexFilter {
    fn contains(&self, number: usize) -> bool {
        let Ok(number) = i32::try_from(number) else {
            return false;
        };
        if self.explicit.contains(&number) {
            return true;
        }
        if number < self.start || number > self.end || self.stride <= 0.0 {
            return false;
        }
        if self.stride <= 1.0 {
            return true;
        }
        // Calculate only the steps that can produce this index after truncation.
        let offset = f64::from(number) - f64::from(self.start);
        let step = (offset / self.stride).floor();
        let start = f64::from(self.start);
        [step, step + 1.0].iter().any(|step| {
            let value = start + step * self.stride;
            value <= f64::from(self.end) && value.trunc() == f64::from(number)
        })
    }
}

fn index_filter(
    host: &dyn Host,
    dir: &Path,
    source: &IndexSource,
    count: usize,
) -> Result<Option<IndexFilter>, CtestError> {
    let fields = match source {
        IndexSource::Preset(PresetIndex::Range(range)) => IndexFields {
            start: range.start,
            end: range.end,
            stride: range.stride.map(f64::from),
            explicit: range
                .specific_tests
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
        },
        IndexSource::Preset(PresetIndex::File(file)) | IndexSource::Argument(file) => {
            let path = dir.join(to_native(file));
            let data = if host
                .metadata(&path, Follow::Yes)
                .is_ok_and(|metadata| metadata.kind == FileKind::File)
            {
                state::read_optional(host, &path)?
            } else {
                None
            };
            let raw = match data.as_deref() {
                Some(data) => {
                    // CTest reads the first line. Without a newline, its buffer omits the last byte.
                    let line = if let Some(end) = data.iter().position(|&byte| byte == b'\n') {
                        data.get(..end).unwrap_or_default()
                    } else {
                        data.get(..data.len().saturating_sub(1)).unwrap_or_default()
                    };
                    line.strip_suffix(b"\r").unwrap_or(line)
                }
                None => file,
            };
            if raw.is_empty() {
                return Ok(None);
            }
            parse_indices(raw)
        }
    };
    let filter = IndexFilter {
        start: fields.start.filter(|&start| start != -1).unwrap_or(1),
        end: fields
            .end
            .filter(|&end| end != -1)
            .unwrap_or_else(|| i32::try_from(count).unwrap_or(i32::MAX)),
        stride: fields
            .stride
            .filter(|&stride| stride != -1.0)
            .unwrap_or(1.0),
        explicit: fields.explicit,
    };
    // A range that expands to no indices leaves the selection unrestricted.
    Ok(
        (!filter.explicit.is_empty() || (filter.stride > 0.0 && filter.start <= filter.end))
            .then_some(filter),
    )
}

struct IndexFields {
    start: Option<i32>,
    end: Option<i32>,
    stride: Option<f64>,
    explicit: BTreeSet<i32>,
}

fn parse_indices(raw: &[u8]) -> IndexFields {
    let mut fields = raw.split(|&byte| byte == b',');
    let integer = |field: &[u8]| {
        if field.is_empty() {
            return None;
        }
        let field = field.trim_ascii_start();
        let digits = field
            .strip_prefix(b"-")
            .or_else(|| field.strip_prefix(b"+"))
            .unwrap_or(field);
        let len = field.len() - digits.len()
            + digits
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
        Some(
            field
                .get(..len)
                .and_then(|value| std::str::from_utf8(value).ok())
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
        )
    };
    IndexFields {
        start: fields.next().and_then(integer),
        end: fields.next().and_then(integer),
        stride: fields
            .next()
            .filter(|field| !field.is_empty())
            .map(|field| {
                crate::cli::number_prefix(field)
                    .filter(|number| number.is_finite())
                    .unwrap_or(0.0)
            }),
        explicit: fields.map(|field| integer(field).unwrap_or(-1)).collect(),
    }
}

fn schedule_order(
    host: &dyn Host,
    dir: &Path,
    plan: &Plan,
    options: &CtestOptions,
    selection: &SelectionOptions,
) -> Result<Vec<usize>, CtestError> {
    let costs = state::read_costs(host, dir)?;
    let failed: BTreeSet<_> = costs.failed.iter().collect();
    let mut seed = selection.random_seed.map_or_else(
        || {
            let now = host.now();
            now.secs as u64 ^ u64::from(now.nanos) ^ host.monotonic().as_nanos() as u64
        },
        u64::from,
    );
    // A random priority changes test order without changing the dependency graph.
    let priorities: Vec<_> = plan
        .tests
        .iter()
        .map(|test| {
            let property = test.property(b"COST");
            let cost = crate::cli::number_prefix(property)
                .filter(|cost| cost.is_finite())
                .unwrap_or_default();
            let priority = if selection.schedule_random {
                random_priority(&mut seed)
            } else if cost == 0.0 && options.jobs > 1 {
                costs
                    .tests
                    .get(&test.name)
                    .map_or(0.0, |entry| entry.average)
            } else {
                cost
            };
            (options.jobs > 1 && failed.contains(&test.name), priority)
        })
        .collect();

    let mut dependencies = vec![Vec::new(); plan.tests.len()];
    for (parent, children) in plan.dependents.iter().enumerate() {
        for &child in children {
            if let Some(deps) = dependencies.get_mut(child) {
                deps.push(parent);
            }
        }
    }
    let mut order: Vec<_> = (0..plan.tests.len()).collect();
    let priority = |id: usize| priorities.get(id).copied().unwrap_or_default();
    if options.jobs > 1 {
        let levels = dependency_depths(plan, &dependencies);
        order.sort_by(|&left, &right| {
            let (left_failed, left_cost) = priority(left);
            let (right_failed, right_cost) = priority(right);
            right_failed
                .cmp(&left_failed)
                .then_with(|| levels.get(right).cmp(&levels.get(left)))
                .then_with(|| right_cost.total_cmp(&left_cost))
        });
        return Ok(order);
    }
    order.sort_by(|&left, &right| priority(right).1.total_cmp(&priority(left).1));
    Ok(serial_dependencies_first(order, &dependencies))
}

fn dependency_depths(plan: &Plan, deps: &[Vec<usize>]) -> Vec<usize> {
    let mut levels = vec![0; deps.len()];
    let mut remaining: Vec<_> = plan.dependents.iter().map(Vec::len).collect();
    let mut ready: Vec<_> = remaining
        .iter()
        .enumerate()
        .filter_map(|(id, &count)| (count == 0).then_some(id))
        .collect();
    while let Some(id) = ready.pop() {
        let depth = levels.get(id).copied().unwrap_or_default() + 1;
        for &dep in deps.get(id).into_iter().flatten() {
            if let Some(level) = levels.get_mut(dep) {
                *level = depth.max(*level);
            }
            if let Some(count) = remaining.get_mut(dep) {
                *count -= 1;
                if *count == 0 {
                    ready.push(dep);
                }
            }
        }
    }
    levels
}

fn serial_dependencies_first(order: Vec<usize>, dependencies: &[Vec<usize>]) -> Vec<usize> {
    let mut priority = Vec::with_capacity(order.len());
    let mut seen = vec![false; order.len()];
    let mut stack = Vec::new();
    for id in order {
        stack.push((id, false));
        while let Some((next, visited)) = stack.pop() {
            if visited {
                priority.push(next);
            } else if let Some(mark) = seen.get_mut(next)
                && !std::mem::replace(mark, true)
            {
                stack.push((next, true));
                stack.extend(
                    dependencies
                        .get(next)
                        .into_iter()
                        .flatten()
                        .rev()
                        .map(|&dep| (dep, false)),
                );
            }
        }
    }
    priority
}

fn random_priority(seed: &mut u64) -> f64 {
    // SplitMix64 makes the same seed produce the same priorities on all hosts.
    *seed = seed.wrapping_add(0x9e3779b97f4a7c15);
    let mut value = *seed;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    (value >> 11) as f64
}

#[cfg(test)]
mod tests;
