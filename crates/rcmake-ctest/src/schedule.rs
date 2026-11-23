use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::thread;

use rcmake_base::truth;
use rcmake_host::Host;

use crate::CtestOptions;
use crate::execution::{self, ResultLine, Status};
use crate::reporting::Report;
use crate::selection::{Plan, PlannedTest};

pub(crate) fn run(
    host: &dyn Host,
    options: &CtestOptions,
    plan: &Plan,
    report: &Report<'_>,
) -> Vec<Option<Status>> {
    let jobs = options.jobs.max(1);
    let mut remaining = plan.prerequisites.clone();
    let mut ready: BTreeSet<_> = remaining
        .iter()
        .enumerate()
        .filter_map(|(id, &count)| (count == 0).then_some(id))
        .collect();
    let mut statuses = vec![None; plan.tests.len()];
    let mut active = 0;
    let mut completed = 0;
    let mut stopped = false;

    thread::scope(|scope| {
        let (send, receive) = mpsc::channel();
        loop {
            let (id, result) = if let Ok(completion) = receive.try_recv() {
                active -= 1;
                completion
            } else if active < jobs
                && let Some(id) = ready.pop_first()
            {
                let Some(test) = plan.tests.get(id) else {
                    continue;
                };
                if stopped && !test.cleanup {
                    ready.extend(plan.complete(id, &mut remaining));
                    continue;
                }
                let result = match prepare(host, options, plan, report, &statuses, test) {
                    Ok(request) => {
                        let send = send.clone();
                        let worker = thread::Builder::new().spawn_scoped(scope, move || {
                            let result = catch_unwind(AssertUnwindSafe(|| {
                                execution::execute(host, test, &request)
                            }))
                            .unwrap_or_else(|_| {
                                ResultLine::not_run("The test worker stopped unexpectedly.")
                            });
                            let _ = send.send((id, result));
                        });
                        match worker {
                            Ok(_) => {
                                active += 1;
                                continue;
                            }
                            Err(error) => {
                                ResultLine::not_run(format!("Cannot start test worker: {error}"))
                            }
                        }
                    }
                    Err(result) => result,
                };
                (id, result)
            } else if active > 0
                && let Ok(completion) = receive.recv()
            {
                active -= 1;
                completion
            } else {
                break;
            };

            if let (Some(test), Some(status)) = (plan.tests.get(id), statuses.get_mut(id)) {
                *status = Some(result.status);
                completed += 1;
                report.finish(test, &result, completed);
            }
            stopped |= options.stop_on_failure && result.status.failed();
            ready.extend(plan.complete(id, &mut remaining));
        }
    });
    statuses
}

fn prepare(
    host: &dyn Host,
    options: &CtestOptions,
    plan: &Plan,
    report: &Report<'_>,
    statuses: &[Option<Status>],
    test: &PlannedTest,
) -> Result<rcmake_host::RunRequest, ResultLine> {
    let request = if truth::is_on(test.property(b"DISABLED")) {
        Err(ResultLine::new(Status::Disabled))
    } else {
        let failed_setups: Vec<_> = test
            .required_setups
            .iter()
            .filter_map(|&id| {
                let status = statuses.get(id).copied().flatten();
                plan.tests
                    .get(id)
                    .filter(|_| !status.is_some_and(Status::fixture_ready))
            })
            .map(|setup| setup.name.to_string())
            .collect();
        if failed_setups.is_empty() {
            execution::request(host, test, options).map_err(ResultLine::not_run)
        } else {
            Err(ResultLine::not_run(format!(
                "Failed test dependencies: {}",
                failed_setups.join(" ")
            )))
        }
    };
    report.start(test, request.as_ref().ok());
    request
}
