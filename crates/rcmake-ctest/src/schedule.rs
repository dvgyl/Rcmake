use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use rcmake_base::{BString, Limits, truth};
use rcmake_host::{CommandSpec, Host, Input, Output, ReadRange, RunRequest, path::from_native};
use rcmake_presets::{RepeatMode, TestRepeat};

use crate::cli::Invocation;
use crate::execution::{self, ResultLine, Status};
use crate::reporting::Report;
use crate::selection::{Plan, PlannedTest};
use crate::{CtestError, CtestOptions, state};

#[derive(Clone, Copy, PartialEq, Eq)]
enum FixtureMode {
    Separate,
    Once,
    Together,
}

impl FixtureMode {
    fn name(self) -> &'static str {
        match self {
            Self::Separate => "EACH_TEST_SEPARATELY",
            Self::Once => "AROUND_ALL_REPEATS",
            Self::Together => "AROUND_EACH_REPEAT",
        }
    }

    fn parse(value: &[u8]) -> Option<Self> {
        match value {
            b"EACH_TEST_SEPARATELY" => Some(Self::Separate),
            b"AROUND_ALL_REPEATS" | b"ONCE" => Some(Self::Once),
            b"AROUND_EACH_REPEAT" => Some(Self::Together),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Fixture {
    members: Vec<usize>,
    requested: Option<(FixtureMode, usize)>,
    default: Option<FixtureMode>,
}

struct Group {
    members: Vec<usize>,
    pending: usize,
    runs_left: i32,
}

struct Repetition {
    repeat: Option<TestRepeat>,
    modes: Vec<FixtureMode>,
    group_of: Vec<Option<usize>>,
    groups: Vec<Group>,
    internal_dependencies: Vec<usize>,
}

impl Repetition {
    #[allow(
        clippy::indexing_slicing,
        reason = "The plan supplies all test indices"
    )]
    fn new(plan: &Plan, repeat: Option<TestRepeat>) -> Result<Self, CtestError> {
        let mut result = Self {
            repeat: repeat.filter(|repeat| repeat.count > 1),
            modes: vec![FixtureMode::Separate; plan.tests.len()],
            group_of: vec![None; plan.tests.len()],
            groups: Vec::new(),
            internal_dependencies: vec![0; plan.tests.len()],
        };
        let Some(repeat) = result.repeat else {
            return Ok(result);
        };
        let mut fixtures = BTreeMap::<BString, Fixture>::new();
        for (id, test) in plan.tests.iter().enumerate() {
            for name in test.list(b"FIXTURES_REQUIRED") {
                fixtures.entry(name).or_default().members.push(id);
            }
            for name in test
                .list(b"FIXTURES_SETUP")
                .into_iter()
                .chain(test.list(b"FIXTURES_CLEANUP"))
            {
                let fixture = fixtures.entry(name.clone()).or_default();
                fixture.members.push(id);
                if let Some(mode) = FixtureMode::parse(test.property(b"FIXTURE_REPEAT_MODE"))
                    && let Some((previous, owner)) = fixture.requested.replace((mode, id))
                    && previous != mode
                {
                    let mut requests = [(&test.name, mode), (&plan.tests[owner].name, previous)];
                    requests.sort_by(|a, b| a.0.cmp(b.0));
                    let [(first, first_mode), (second, second_mode)] = requests;
                    return Err(CtestError::Schedule(format!(
                        "Error: the setup and cleanup tests of fixture \"{name}\" request conflicting FIXTURE_REPEAT_MODE values:\n  \"{first}\" requests {}\n  \"{second}\" requests {}\nAll setup and cleanup tests of a fixture must request the same mode.",
                        first_mode.name(),
                        second_mode.name()
                    )));
                }
                fixture.default = fixture.default.or_else(|| {
                    FixtureMode::parse(test.property(b"_CMAKE_DEFAULT_FIXTURE_REPEAT_MODE"))
                });
            }
        }

        let mut modes = vec![None; plan.tests.len()];
        let mut connected: Vec<_> = (0..plan.tests.len()).collect();
        for (name, fixture) in fixtures {
            let mode = fixture
                .requested
                .map(|(mode, _)| mode)
                .or(fixture.default)
                .unwrap_or(FixtureMode::Separate);
            let Some(&first) = fixture.members.first() else {
                continue;
            };
            for id in fixture.members {
                if let Some((previous, fixture)) = modes[id].replace((mode, name.clone()))
                    && previous != mode
                {
                    return Err(CtestError::Schedule(format!(
                        "Error: test \"{}\" takes part in fixtures with conflicting FIXTURE_REPEAT_MODE values:\n  fixture \"{name}\" uses {}\n  fixture \"{fixture}\" uses {}\nFixtures that share a test must use the same mode.",
                        plan.tests[id].name,
                        mode.name(),
                        previous.name()
                    )));
                }
                if mode == FixtureMode::Together {
                    let first_root = root(&mut connected, first);
                    let test_root = root(&mut connected, id);
                    connected[test_root] = first_root;
                }
            }
        }

        let mut groups = BTreeMap::<usize, Vec<usize>>::new();
        for (id, mode) in modes.into_iter().enumerate() {
            match mode.map(|(mode, _)| mode) {
                Some(FixtureMode::Together) => {
                    groups.entry(root(&mut connected, id)).or_default().push(id);
                    result.modes[id] = FixtureMode::Together;
                }
                Some(FixtureMode::Once) if owns_fixture(&plan.tests[id]) => {
                    result.modes[id] = FixtureMode::Once;
                }
                _ => {}
            }
        }
        for members in groups.into_values() {
            let group_id = result.groups.len();
            for &id in &members {
                result.group_of[id] = Some(group_id);
            }
            result.groups.push(Group {
                pending: members.len(),
                members,
                runs_left: repeat.count - 1,
            });
        }
        for (id, children) in plan.dependents.iter().enumerate() {
            if let Some(group) = result.group_of[id] {
                for &child in children {
                    if result.group_of[child] == Some(group) {
                        result.internal_dependencies[child] += 1;
                    }
                }
            }
        }
        Ok(result)
    }

    #[allow(clippy::indexing_slicing, reason = "The plan supplies the test index")]
    fn numbered_run(&self, id: usize, run: i32) -> Option<(i32, i32)> {
        self.repeat
            .filter(|_| self.modes[id] != FixtureMode::Once)
            .map(|repeat| (run, repeat.count))
    }

    #[allow(clippy::indexing_slicing, reason = "The plan supplies the test index")]
    fn individual(&self, id: usize) -> Option<TestRepeat> {
        self.repeat
            .filter(|_| self.modes[id] == FixtureMode::Separate)
    }
}

#[allow(clippy::indexing_slicing, reason = "Each parent is a valid test index")]
fn root(parents: &mut [usize], mut id: usize) -> usize {
    while parents[id] != id {
        parents[id] = parents[parents[id]];
        id = parents[id];
    }
    id
}

fn owns_fixture(test: &PlannedTest) -> bool {
    !test.property(b"FIXTURES_SETUP").is_empty() || !test.property(b"FIXTURES_CLEANUP").is_empty()
}

fn repeat_again(mode: RepeatMode, all_passed: bool, any_timeout: bool) -> bool {
    match mode {
        RepeatMode::UntilFail => all_passed,
        RepeatMode::UntilPass => !all_passed,
        RepeatMode::AfterTimeout => any_timeout,
    }
}

struct Ready {
    remaining: Vec<usize>,
    ordered: BTreeSet<(usize, usize)>,
    rank: Vec<usize>,
}

impl Ready {
    #[allow(
        clippy::indexing_slicing,
        reason = "The plan supplies all test indices"
    )]
    fn new(plan: &Plan) -> Self {
        let mut result = Self {
            remaining: plan.prerequisites.clone(),
            ordered: BTreeSet::new(),
            rank: vec![0; plan.tests.len()],
        };
        for (rank, &id) in plan.schedule_order.iter().enumerate() {
            result.rank[id] = rank;
            result.enqueue_if_ready(id);
        }
        result
    }

    #[allow(clippy::indexing_slicing, reason = "The plan supplies the test index")]
    fn enqueue_if_ready(&mut self, id: usize) {
        if self.remaining[id] == 0 {
            self.ordered.insert((self.rank[id], id));
        }
    }

    #[allow(
        clippy::indexing_slicing,
        reason = "The plan supplies all test indices"
    )]
    fn release(
        &mut self,
        plan: &Plan,
        id: usize,
        include: impl Fn(usize) -> bool,
    ) -> Result<(), CtestError> {
        for &child in &plan.dependents[id] {
            if include(child) {
                self.remaining[child] = self.remaining[child]
                    .checked_sub(1)
                    .ok_or_else(invariant_error)?;
                self.enqueue_if_ready(child);
            }
        }
        Ok(())
    }
}

struct TestResources {
    slots: usize,
    serial: bool,
    locks: BTreeSet<BString>,
}

impl TestResources {
    fn new(test: &PlannedTest, jobs: usize) -> Self {
        let processors = std::str::from_utf8(test.property(b"PROCESSORS"))
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(1);
        Self {
            slots: processors.clamp(1, jobs),
            serial: truth::is_on(test.property(b"RUN_SERIAL")),
            locks: test.list(b"RESOURCE_LOCK").into_iter().collect(),
        }
    }
}

#[derive(Default)]
struct Running {
    slots: usize,
    serial: bool,
    locks: BTreeSet<BString>,
}

impl Running {
    fn fits(&self, test: &TestResources, spare: usize) -> bool {
        !self.serial
            && (!test.serial || self.slots == 0)
            && test.slots <= spare
            && self.locks.is_disjoint(&test.locks)
    }

    fn start(&mut self, test: &TestResources) {
        self.slots += test.slots;
        self.serial = test.serial;
        self.locks.extend(test.locks.iter().cloned());
    }

    fn finish(&mut self, test: &TestResources) -> Result<(), CtestError> {
        self.slots = self
            .slots
            .checked_sub(test.slots)
            .ok_or_else(invariant_error)?;
        self.serial &= !test.serial;
        self.locks.retain(|lock| !test.locks.contains(lock));
        Ok(())
    }
}

pub(crate) fn stop_deadline(host: &dyn Host, stop_time: Option<u32>) -> Option<Duration> {
    let stop_time = i64::from(stop_time?);
    let now = host.now();
    let epoch = if now.secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(now.secs as u64, now.nanos))?
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(now.secs.unsigned_abs()))?
            .checked_add(Duration::from_nanos(u64::from(now.nanos)))?
    };
    let offset = host.time_zone().to_offset(epoch.try_into().ok()?).seconds();
    let local = (now.secs + i64::from(offset)).rem_euclid(86_400);
    let seconds = if stop_time < local {
        stop_time + 86_400 - local
    } else {
        stop_time - local
    };
    Some(
        host.monotonic().saturating_add(
            Duration::from_secs(seconds as u64)
                .saturating_sub(Duration::from_nanos(u64::from(now.nanos))),
        ),
    )
}

fn system_load(host: &dyn Host) -> u64 {
    let output = if host.system().sysname == "Darwin" {
        host.run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: "/usr/sbin/sysctl".into(),
                    args: vec!["-n".into(), "vm.loadavg".into()],
                }],
                cwd: host.initial_cwd(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Null,
                timeout: Some(Duration::from_secs(1)),
                tee: false,
            },
            &mut |_, _| {},
        )
        .ok()
        .map(|output| output.stdout)
    } else {
        host.read(
            Path::new("/proc/loadavg"),
            ReadRange::Slice {
                offset: 0,
                len: Some(128),
            },
        )
        .ok()
    };
    output
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|line| {
            line.trim_start_matches(['{', ' '])
                .split_whitespace()
                .next()
        })
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|load| load.is_finite() && *load >= 0.0)
        .unwrap_or_default()
        .ceil() as u64
}

struct Attempt {
    id: usize,
    result: ResultLine,
    again: bool,
}

fn remaining_time(host: &dyn Host, deadline: Option<Duration>) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_sub(host.monotonic()))
}

fn run_attempts(
    host: &dyn Host,
    test: &PlannedTest,
    mut request: RunRequest,
    instrument: Option<&crate::instrumentation::Recorder<'_>>,
    repeat: Option<TestRepeat>,
    deadline: Option<Duration>,
    send: impl Fn(ResultLine, bool) -> bool,
) {
    let count = repeat.map_or(1, |repeat| repeat.count);
    let timeout = request.timeout;
    for attempt in 1..=count {
        request.timeout = remaining_time(host, deadline)
            .map(|remaining| remaining.min(timeout.unwrap_or(Duration::MAX)))
            .or(timeout);
        let result = execution::execute(host, test, &request, instrument);
        let again = attempt < count
            && !matches!(
                result.status,
                Status::Disabled | Status::Skipped | Status::NotRun
            )
            && !remaining_time(host, deadline).is_some_and(|remaining| remaining.is_zero())
            && repeat.is_some_and(|repeat| {
                repeat_again(
                    repeat.mode,
                    result.status == Status::Passed,
                    result.status == Status::Timeout,
                )
            });
        if !send(result, again) || !again {
            break;
        }
    }
}

struct RunState<'a> {
    host: &'a dyn Host,
    dir: &'a Path,
    costs: state::CostData,
    log: Vec<u8>,
}

impl<'a> RunState<'a> {
    fn new(host: &'a dyn Host, dir: &'a Path) -> Result<Self, CtestError> {
        let costs = state::read_costs(host, dir)?;
        let log = format!("Start testing: {}\n", host.now().secs).into_bytes();
        Ok(Self {
            host,
            dir,
            costs,
            log,
        })
    }

    fn record(
        &mut self,
        test: &PlannedTest,
        result: &ResultLine,
        request: Option<&RunRequest>,
    ) -> Result<(), CtestError> {
        if !matches!(result.status, Status::NotRun | Status::Disabled) {
            state::write_stamp(self.host, self.dir, &test.name)?;
        }
        let entry = self
            .costs
            .tests
            .entry(test.name.clone())
            .or_insert_with(|| state::CostEntry {
                average: crate::cli::number_prefix(test.property(b"COST"))
                    .filter(|number| number.is_finite())
                    .unwrap_or_default(),
                ..state::CostEntry::default()
            });
        if result.status == Status::Passed {
            entry.record_pass(result.elapsed.as_secs_f64());
        }

        let mut header = format!(
            "----------------------------------------------------------\n{} Testing: {}\n",
            test.number, test.name
        );
        if let Some(request) = request
            && let Some(command) = request.pipeline.first()
        {
            let _ = write!(header, "Command: \"{}\"", command.program);
            for arg in &command.args {
                let _ = write!(header, " \"{arg}\"");
            }
            let _ = writeln!(header, "\nDirectory: {}", from_native(&request.cwd));
        }
        header.push_str("Output:\n----------------------------------------------------------\n");
        let status = match result.status {
            Status::Passed => "Test Passed.",
            Status::Failed => "Test Failed.",
            Status::Timeout => "Test Timed Out.",
            Status::Disabled => "Test Disabled.",
            Status::Skipped => "Test Skipped.",
            Status::NotRun => "Test Not Run.",
        };
        let reason = result
            .regex_reason
            .as_ref()
            .map_or_else(String::new, |reason| {
                format!("Test Pass Reason:\n{reason}\n")
            });
        let footer = format!(
            "\n<end of output>\nTest time = {:6.2} sec\n{status}\n{reason}",
            result.elapsed.as_secs_f64()
        );
        let limit = Limits::default().max_value_bytes;
        let available = limit.saturating_sub(self.log.len() + header.len() + footer.len() + 128);
        if available > 0 {
            self.log.extend_from_slice(header.as_bytes());
            let detail = result
                .detail
                .iter()
                .flat_map(|detail| detail.bytes().chain(*b"\n"));
            self.log
                .extend(detail.chain(result.output.iter().copied()).take(available));
            self.log.extend_from_slice(footer.as_bytes());
        }
        Ok(())
    }

    fn finish(mut self, plan: &Plan, statuses: &[Option<Status>]) -> Result<(), CtestError> {
        let failures: Vec<_> = plan
            .tests
            .iter()
            .zip(statuses)
            .filter(|(_, status)| status.is_some_and(Status::failed))
            .map(|(test, _)| state::FailedTest {
                number: test.number,
                name: test.name.clone(),
            })
            .collect();
        self.costs.failed = failures.iter().map(|test| test.name.clone()).collect();
        self.log
            .extend_from_slice(format!("\nEnd testing: {}\n", self.host.now().secs).as_bytes());
        state::write_log(self.host, self.dir, &self.log)?;
        state::write_failed(self.host, self.dir, &failures)?;
        state::write_costs(self.host, self.dir, &self.costs)
    }
}

struct Scheduler<'a> {
    plan: &'a Plan,
    ready: Ready,
    repetition: Repetition,
    statuses: Vec<Option<Status>>,
    completed: usize,
    stopped: bool,
}

impl Scheduler<'_> {
    #[allow(
        clippy::indexing_slicing,
        reason = "The plan supplies all test indices"
    )]
    fn resume(&mut self, completed: &BTreeSet<usize>) -> Result<Vec<usize>, CtestError> {
        let mut resumed = Vec::new();
        for (id, test) in self.plan.tests.iter().enumerate() {
            if !completed.contains(&test.number) {
                continue;
            }
            if let Some(group) = self.repetition.group_of[id] {
                if !self.repetition.groups[group]
                    .members
                    .iter()
                    .all(|&member| completed.contains(&self.plan.tests[member].number))
                {
                    continue;
                }
                self.repetition.groups[group].runs_left = 0;
            }
            self.statuses[id] = Some(Status::Passed);
            self.completed += 1;
            self.advance(id)?;
            resumed.push(id);
        }
        for &id in &resumed {
            self.ready.ordered.remove(&(self.ready.rank[id], id));
        }
        Ok(resumed)
    }

    #[allow(
        clippy::indexing_slicing,
        reason = "The plan supplies test indices, and Repetition assigns group indices"
    )]
    fn advance(&mut self, id: usize) -> Result<(), CtestError> {
        let Some(group_id) = self.repetition.group_of[id] else {
            return self.ready.release(self.plan, id, |_| true);
        };
        let group_of = &self.repetition.group_of;
        self.ready
            .release(self.plan, id, |child| group_of[child] == Some(group_id))?;
        let group = &mut self.repetition.groups[group_id];
        group.pending = group.pending.checked_sub(1).ok_or_else(invariant_error)?;
        if group.pending > 0 {
            return Ok(());
        }
        let statuses = || group.members.iter().map(|&id| self.statuses[id]);
        let again = group.runs_left > 0
            && !self.stopped
            && self.repetition.repeat.is_some_and(|repeat| {
                repeat_again(
                    repeat.mode,
                    statuses().all(|status| status == Some(Status::Passed)),
                    statuses().any(|status| status == Some(Status::Timeout)),
                )
            });
        if again {
            group.runs_left = group.runs_left.checked_sub(1).ok_or_else(invariant_error)?;
            group.pending = group.members.len();
            for &member in &group.members {
                self.statuses[member].take().ok_or_else(invariant_error)?;
                self.completed = self.completed.checked_sub(1).ok_or_else(invariant_error)?;
                self.ready.remaining[member] = self.repetition.internal_dependencies[member];
                self.ready.enqueue_if_ready(member);
            }
        } else {
            for &member in &group.members {
                self.ready
                    .release(self.plan, member, |child| group_of[child] != Some(group_id))?;
            }
        }
        Ok(())
    }
}

fn invariant_error() -> CtestError {
    CtestError::Schedule("internal scheduler invariant violated".into())
}

#[allow(
    clippy::indexing_slicing,
    reason = "The plan supplies all test indices"
)]
pub(crate) fn run(
    host: &dyn Host,
    dir: &Path,
    invocation: &Invocation,
    plan: &Plan,
    report: &Report<'_>,
    deadline: Option<Duration>,
    coverage_tool: &[u8],
) -> Result<Vec<Option<Status>>, CtestError> {
    let options = &invocation.options;
    let jobs = options.jobs.max(1);
    let mut scheduler = Scheduler {
        plan,
        ready: Ready::new(plan),
        repetition: Repetition::new(plan, invocation.execution.repeat)?,
        statuses: vec![None; plan.tests.len()],
        completed: 0,
        stopped: false,
    };
    let completed = state::resume(
        host,
        dir,
        invocation.execution.enable_failover == Some(true),
    )?;
    let resumed = scheduler.resume(&completed)?;
    let mut state = RunState::new(host, dir)?;
    let resources: Vec<_> = plan
        .tests
        .iter()
        .map(|test| TestResources::new(test, jobs))
        .collect();
    let mut running = Running::default();
    let mut requests = BTreeMap::new();
    let limit = invocation.test_load.filter(|&limit| limit > 0);
    let mut simulated_load = host
        .env_get(b"__CTEST_FAKE_LOAD_AVERAGE_FOR_TESTING")
        .and_then(|value| value.to_string().parse::<u64>().ok());
    let mut load = 0;
    let mut runs = vec![0; plan.tests.len()];

    let memcheck = crate::memcheck::Session::start(host, dir, invocation)?;
    let instrument = crate::instrumentation::Recorder::open(host, dir, options.config.as_ref())?;
    let instrument = instrument.as_ref();
    thread::scope(|scope| -> Result<(), CtestError> {
        let (send, receive) = mpsc::sync_channel::<Attempt>(jobs.min(64));
        let mut budget = None;
        loop {
            let expired =
                remaining_time(host, deadline).is_some_and(|remaining| remaining.is_zero());
            let completion = receive.try_recv().ok();
            let attempt = if let Some(completion) = completion {
                budget = None;
                completion
            } else if !expired && !scheduler.ready.ordered.is_empty() && running.slots < jobs {
                let spare = *budget.get_or_insert_with(|| {
                    limit.map_or(jobs, |limit| {
                        load = simulated_load.unwrap_or_else(|| system_load(host));
                        usize::try_from(limit.saturating_sub(load)).unwrap_or(usize::MAX)
                    })
                });
                let spare = spare.min(
                    jobs.checked_sub(running.slots)
                        .ok_or_else(invariant_error)?,
                );
                if let Some(&(rank, id)) = scheduler
                    .ready
                    .ordered
                    .iter()
                    .find(|&&(_, id)| running.fits(&resources[id], spare))
                {
                    scheduler.ready.ordered.remove(&(rank, id));
                    let test = &plan.tests[id];
                    if scheduler.stopped && !test.cleanup {
                        scheduler.advance(id)?;
                        continue;
                    }
                    runs[id] += 1;
                    let run = scheduler.repetition.numbered_run(id, runs[id]);
                    let result = match prepare(host, options, plan, &scheduler.statuses, test) {
                        Ok(mut request) => {
                            if let Some(memcheck) = &memcheck {
                                memcheck.apply(&mut request, test.number);
                            }
                            if coverage_tool == b"LLVM-COV" {
                                let name = test.name.to_string().replace(['/', '\\'], "_");
                                let profile = from_native(&dir.join(format!("{name}_%p.profraw")));
                                request.env.retain(|(name, _)| name != "LLVM_PROFILE_FILE");
                                request.env.push(("LLVM_PROFILE_FILE".into(), profile));
                            }
                            report.start(test, Some(&request), run);
                            let send = send.clone();
                            let repeat = scheduler.repetition.individual(id);
                            let worker_request = request.clone();
                            let worker = thread::Builder::new().spawn_scoped(scope, move || {
                                let result =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        run_attempts(
                                            host,
                                            test,
                                            worker_request,
                                            instrument,
                                            repeat,
                                            deadline,
                                            |result, again| {
                                                send.send(Attempt { id, result, again }).is_ok()
                                            },
                                        );
                                    }));
                                if result.is_err() {
                                    let _ = send.send(Attempt {
                                        id,
                                        again: false,
                                        result: ResultLine::not_run(
                                            "The test worker stopped unexpectedly.",
                                        ),
                                    });
                                }
                            });
                            match worker {
                                Ok(_) => {
                                    running.start(&resources[id]);
                                    budget = Some(
                                        budget
                                            .unwrap_or_default()
                                            .checked_sub(resources[id].slots)
                                            .ok_or_else(invariant_error)?,
                                    );
                                    requests.insert(id, request);
                                    continue;
                                }
                                Err(error) => ResultLine::not_run(format!(
                                    "Cannot start test worker: {error}"
                                )),
                            }
                        }
                        Err(result) => {
                            report.start(test, None, run);
                            result
                        }
                    };
                    Attempt {
                        id,
                        result,
                        again: false,
                    }
                } else if running.slots == 0 {
                    if let Some(limit) = limit {
                        let smallest = scheduler
                            .ready
                            .ordered
                            .iter()
                            .map(|&(_, id)| (&plan.tests[id], resources[id].slots))
                            .min_by_key(|(_, slots)| *slots);
                        report.wait_for_load(load, limit, smallest);
                    }
                    if simulated_load.is_some() {
                        simulated_load = Some(0);
                    }
                    let wait = remaining_time(host, deadline).unwrap_or(Duration::MAX);
                    host.sleep(wait.min(Duration::from_secs(1)));
                    budget = None;
                    continue;
                } else {
                    budget = None;
                    let Ok(completion) = receive.recv() else {
                        break;
                    };
                    completion
                }
            } else if running.slots > 0 {
                budget = None;
                let Ok(completion) = receive.recv() else {
                    break;
                };
                completion
            } else {
                if !expired && !scheduler.stopped && scheduler.completed < plan.tests.len() {
                    return Err(CtestError::Schedule(
                        "The test dependencies prevent a fixture from repeating.".into(),
                    ));
                }
                break;
            };

            let Attempt { id, result, again } = attempt;
            let test = &plan.tests[id];
            state.record(test, &result, requests.get(&id))?;
            let progress = match scheduler.repetition.individual(id) {
                Some(repeat) if repeat.mode == RepeatMode::UntilFail => runs[id] == repeat.count,
                _ => runs[id] == 1 || scheduler.repetition.group_of[id].is_some(),
            };
            report.finish(test, &result, progress.then_some(scheduler.completed + 1));
            if again {
                runs[id] += 1;
                report.start(
                    test,
                    requests.get(&id),
                    scheduler.repetition.numbered_run(id, runs[id]),
                );
                continue;
            }
            if requests.remove(&id).is_some() {
                running.finish(&resources[id])?;
            }
            scheduler.completed += 1;
            scheduler.statuses[id] = Some(result.status);
            scheduler.stopped |= options.stop_on_failure && result.status.failed();
            scheduler.stopped |=
                remaining_time(host, deadline).is_some_and(|remaining| remaining.is_zero());
            scheduler.advance(id)?;
            match scheduler.repetition.group_of[id] {
                None => state::checkpoint(host, dir, [test.number])?,
                Some(group) if scheduler.repetition.groups[group].pending == 0 => {
                    state::checkpoint(
                        host,
                        dir,
                        scheduler.repetition.groups[group]
                            .members
                            .iter()
                            .map(|&member| plan.tests[member].number),
                    )?;
                }
                Some(_) => {}
            }
        }
        Ok(())
    })?;
    for id in resumed {
        scheduler.statuses[id] = None;
    }
    state.finish(plan, &scheduler.statuses)?;
    state::clear_checkpoint(host, dir)?;
    if let Some(memcheck) = memcheck {
        memcheck.finish(host, dir, options.quiet)?;
    }
    Ok(scheduler.statuses)
}

#[allow(
    clippy::indexing_slicing,
    reason = "The plan supplies all test indices"
)]
fn prepare(
    host: &dyn Host,
    options: &CtestOptions,
    plan: &Plan,
    statuses: &[Option<Status>],
    test: &PlannedTest,
) -> Result<RunRequest, ResultLine> {
    if truth::is_on(test.property(b"DISABLED")) {
        return Err(ResultLine::new(Status::Disabled));
    }
    if !test.timeout.errors.is_empty() {
        return Err(ResultLine::not_run(test.timeout.errors.join("\n")));
    }
    let failed_setups: Vec<_> = test
        .required_setups
        .iter()
        .filter(|&&id| !statuses[id].is_some_and(Status::fixture_ready))
        .map(|&id| plan.tests[id].name.to_string())
        .collect();
    if !failed_setups.is_empty() {
        return Err(ResultLine::not_run(format!(
            "Failed test dependencies: {}",
            failed_setups.join(" ")
        )));
    }
    let mode = test.property(b"FIXTURE_REPEAT_MODE");
    if !mode.is_empty() && FixtureMode::parse(mode).is_none() {
        return Err(ResultLine::not_run(format!(
            "FIXTURE_REPEAT_MODE \"{}\" not recognized. Must be AROUND_ALL_REPEATS, AROUND_EACH_REPEAT, or EACH_TEST_SEPARATELY.",
            BString::from(mode)
        )));
    }
    for dependency in test.list(b"_CMAKE_TEST_BUILD_DEPENDS") {
        let path = test
            .directory
            .join(rcmake_host::path::to_native(&dependency));
        if host.metadata(&path, rcmake_host::Follow::Yes).is_err() {
            return Err(ResultLine::not_run(format!(
                "Unable to find build dependency: {dependency}"
            )));
        }
    }
    execution::request(host, test, options).map_err(ResultLine::not_run)
}
