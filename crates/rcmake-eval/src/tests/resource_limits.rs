use super::new_interp;
use crate::{Interp, RunMode};
use rcmake_host::Stream;
use std::path::PathBuf;

const BUDGET: usize = 1 << 20;
const BUDGET_MESSAGE: &str = "Total variable storage would exceed the 1048576-byte limit.";

fn run_with_budget(main: &str) -> (i32, String, Interp) {
    let (mut interp, host) = new_interp(&[("/work/main.cmake", main)], RunMode::Script);
    interp.config.limits.max_live_variable_bytes = BUDGET;
    let outcome = interp.run_script(&PathBuf::from("main.cmake"));
    let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
    (outcome.exit_code, stderr, interp)
}

#[test]
fn live_variable_budget_stops_exponential_growth() {
    let mut script = format!("set(A \"{}\")\n", "x".repeat(64));
    // Without a total budget, 40 doublings would ask for 64 TiB.
    script.push_str(&"set(A \"${A}${A}\")\n".repeat(40));
    script.push_str("set(AFTER reached)\n");
    let (exit, stderr, interp) = run_with_budget(&script);
    assert_ne!(exit, 0);
    assert!(stderr.contains(BUDGET_MESSAGE), "{stderr}");
    assert!(!stderr.contains("Variable value exceeded"), "{stderr}");
    assert_eq!(interp.vars.get(b"AFTER"), None);
    // The check runs before each write, so the total never passes the budget.
    assert!(interp.vars.live_bytes() <= BUDGET);
}

#[test]
fn live_variable_budget_stops_copies() {
    let script = format!(
        "set(A \"{}\")\nforeach(i RANGE 10)\n  set(B${{i}} \"${{A}}\")\nendforeach()\nset(AFTER reached)\n",
        "x".repeat(300_000)
    );
    let (exit, stderr, interp) = run_with_budget(&script);
    assert_ne!(exit, 0);
    assert!(stderr.contains(BUDGET_MESSAGE), "{stderr}");
    assert!(interp.vars.get(b"B1").is_some());
    assert_eq!(interp.vars.get(b"B2"), None);
    assert!(interp.vars.live_bytes() <= BUDGET);
    assert_eq!(interp.vars.get(b"AFTER"), None);
}

#[test]
fn live_variable_budget_counts_parent_scope_copies() {
    let script = format!(
        "set(A \"{}\")\nfunction(raise)\n  set(R1 \"${{A}}\" PARENT_SCOPE)\n  set(R2 \"${{A}}\" PARENT_SCOPE)\n  set(R3 \"${{A}}\" PARENT_SCOPE)\nendfunction()\nraise()\nset(AFTER reached)\n",
        "x".repeat(300_000)
    );
    let (exit, stderr, interp) = run_with_budget(&script);
    assert_ne!(exit, 0);
    assert!(stderr.contains(BUDGET_MESSAGE), "{stderr}");
    assert_eq!(interp.vars.get(b"AFTER"), None);
}

#[test]
fn live_bytes_return_to_their_value_after_a_function_scope() {
    let (mut interp, _) = new_interp(&[], RunMode::Script);
    let local = "y".repeat(100 * 1024);
    let define = format!(
        "function(locals)\n  set(L1 \"{local}\")\n  set(L2 \"{local}\")\n  unset(L1)\nendfunction()\nfunction(raise)\n  set(RAISED abc PARENT_SCOPE)\nendfunction()\n"
    );
    assert!(interp.eval_code(define.as_bytes()).is_ok());
    let before = interp.vars.live_bytes();

    assert!(interp.eval_code(b"locals()").is_ok());
    assert_eq!(interp.vars.live_bytes(), before);

    assert!(interp.eval_code(b"raise()").is_ok());
    assert_eq!(
        interp.vars.get(b"RAISED").map(|v| v.as_slice()),
        Some(&b"abc"[..])
    );
    assert_eq!(interp.vars.live_bytes(), before + "RAISED".len() + 3 + 64);
    assert!(!interp.fatal_occurred());
}

#[test]
fn real_frontend_stops_a_command_at_the_argument_cap() {
    let script = format!(
        "set(L \"{}\")\nset(FITS ${{L}})\nset(L \"${{L}};a\")\nset(OVER ${{L}})\nset(AFTER reached)\n",
        vec!["a"; 999].join(";")
    );
    let (mut interp, host) = new_interp(&[("/work/main.cmake", &script)], RunMode::Script);
    interp.frontend = crate::Frontend::default();
    interp.config.limits.max_command_arguments = 1_000;
    let exit = interp.run_script(&PathBuf::from("main.cmake")).exit_code;
    let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
    assert_ne!(exit, 0);
    assert!(
        stderr.contains("Command arguments expanded to more than 1000 elements."),
        "{stderr}"
    );
    assert!(interp.vars.get(b"FITS").is_some());
    assert_eq!(interp.vars.get(b"OVER"), None);
    assert_eq!(interp.vars.get(b"AFTER"), None);
}

/// Run `main` in script mode on a thread with `stack` bytes and a guard budget of `budget`.
fn run_with_stack(main: String, stack: usize, budget: usize) -> (i32, String, Option<usize>) {
    std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || {
            crate::stack_guard::with_budget(budget, || {
                let (mut interp, host) =
                    new_interp(&[("/work/main.cmake", &main)], RunMode::Script);
                interp.frontend = crate::Frontend::default();
                let exit = interp.run_script(&PathBuf::from("main.cmake")).exit_code;
                let stderr =
                    String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
                (exit, stderr, crate::stack_guard::peak_bytes())
            })
        })
        .expect("spawn")
        .join()
        .expect("join")
}

#[test]
fn deep_recursion_stops_at_the_native_stack_guard() {
    let script =
        "set(CMAKE_MAXIMUM_RECURSION_DEPTH 20000)\nfunction(f)\n  f()\nendfunction()\nf()\n";
    // Without the guard, 20,000 levels overflow this 4 MiB stack.
    let (exit, stderr, _) = run_with_stack(script.to_owned(), 4 << 20, 2 << 20);
    assert_eq!(exit, 1, "{stderr}");
    assert!(
        stderr.contains("Maximum recursion depth exceeded: the native stack is full at depth"),
        "{stderr}"
    );
}

#[test]
fn default_recursion_limit_message_does_not_change_under_the_guard() {
    let script = "function(f)\n  f()\nendfunction()\nf()\n";
    let (exit, stderr, _) = run_with_stack(script.to_owned(), 64 << 20, 56 << 20);
    assert_eq!(exit, 1, "{stderr}");
    assert!(
        stderr.contains("Maximum recursion depth of 1000 exceeded"),
        "{stderr}"
    );
}

/// Print the native stack bytes for each level of each recursion path.
/// Run with `cargo test -p rcmake-eval -- --ignored --nocapture stack_bytes_per_level`.
#[test]
#[ignore = "measurement, not a check"]
#[allow(clippy::print_stdout, reason = "The measurement prints its result.")]
fn stack_bytes_per_level() {
    const LEVELS: usize = 200;
    let paths = [
        (
            "function",
            "function(f n)\n  if(NOT n STREQUAL \"END\")\n    f(\"${n}x\")\n  endif()\nendfunction()\nf(x)\n",
        ),
        (
            "macro",
            "macro(m n)\n  if(NOT \"${n}\" STREQUAL \"END\")\n    m(\"${n}x\")\n  endif()\nendmacro()\nm(x)\n",
        ),
        (
            "eval",
            "function(f n)\n  if(NOT n STREQUAL \"END\")\n    cmake_language(EVAL CODE \"f(${n}x)\")\n  endif()\nendfunction()\nf(x)\n",
        ),
    ];
    for (name, body) in paths {
        let script = format!(
            "set(CMAKE_MAXIMUM_RECURSION_DEPTH 20000)\n{}",
            body.replace("END", &"x".repeat(LEVELS))
        );
        let (exit, stderr, peak) = run_with_stack(script, 64 << 20, 56 << 20);
        let peak = peak.unwrap_or_default();
        println!(
            "{name}: exit {exit}, {} bytes per level {stderr}",
            peak / LEVELS
        );
    }
}

/// Run `main` with the real expression frontend and the limits that `limits` sets.
fn run_real(main: &str, limits: impl FnOnce(&mut rcmake_base::Limits)) -> (i32, String, Interp) {
    let (mut interp, host) = new_interp(&[("/work/main.cmake", main)], RunMode::Script);
    interp.frontend = crate::Frontend::default();
    limits(&mut interp.config.limits);
    let exit = interp.run_script(&PathBuf::from("main.cmake")).exit_code;
    let stderr = String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned();
    (exit, stderr, interp)
}

#[test]
fn unset_of_new_names_is_charged() {
    let script = "foreach(i RANGE 20000)\n  unset(NAME_${i})\nendforeach()\nset(AFTER reached)\n";
    let (exit, stderr, interp) = run_with_budget(script);
    assert_ne!(exit, 0);
    assert!(stderr.contains(BUDGET_MESSAGE), "{stderr}");
    assert_eq!(interp.vars.get(b"AFTER"), None);
    assert!(interp.vars.live_bytes() <= BUDGET);
}

#[test]
fn literal_arguments_count_toward_the_argument_cap() {
    let literals = vec!["a"; 1_001].join(" ");
    let script = format!("set(X {literals})\nset(AFTER reached)\n");
    let (exit, stderr, interp) = run_real(&script, |limits| limits.max_command_arguments = 1_000);
    assert_ne!(exit, 0);
    assert!(
        stderr.contains("Command arguments expanded to more than 1000 elements."),
        "{stderr}"
    );
    assert_eq!(interp.vars.get(b"X"), None);
    assert_eq!(interp.vars.get(b"AFTER"), None);
}

#[test]
fn literal_arguments_after_an_expansion_count_toward_the_cap() {
    let script = format!(
        "set(L \"{}\")\nset(OVER ${{L}} b c)\nset(AFTER reached)\n",
        vec!["a"; 999].join(";")
    );
    let (exit, stderr, interp) = run_real(&script, |limits| limits.max_command_arguments = 1_000);
    assert_ne!(exit, 0);
    assert!(
        stderr.contains("Command arguments expanded to more than 1000 elements."),
        "{stderr}"
    );
    assert_eq!(interp.vars.get(b"OVER"), None);
    assert_eq!(interp.vars.get(b"AFTER"), None);
}

#[test]
fn argument_bytes_stop_at_the_byte_cap() {
    let script = format!(
        "set(L \"{}\")\nset(OVER \"${{L}}\" \"${{L}}\")\nset(AFTER reached)\n",
        "x".repeat(600)
    );
    let (exit, stderr, interp) = run_real(&script, |limits| {
        limits.max_command_argument_bytes = 1_000;
    });
    assert_ne!(exit, 0);
    assert!(
        stderr.contains("Command arguments expanded to more than 1000 bytes."),
        "{stderr}"
    );
    assert_eq!(interp.vars.get(b"OVER"), None);
}

#[test]
fn foreach_in_lists_stops_at_the_list_cap() {
    let script = format!(
        "set(L \"{}\")\nforeach(x IN LISTS L L)\nendforeach()\nset(AFTER reached)\n",
        vec!["a"; 600].join(";")
    );
    let (_, stderr, _) = run_real(&script, |limits| limits.max_command_arguments = 1_000);
    assert!(
        stderr.contains("The list operation needs more than 1000 elements."),
        "{stderr}"
    );
}

#[test]
fn foreach_zip_lists_stops_at_the_list_cap() {
    let script = format!(
        "set(L \"{}\")\nforeach(x IN ZIP_LISTS L L)\nendforeach()\n",
        vec!["a"; 600].join(";")
    );
    let (_, stderr, _) = run_real(&script, |limits| limits.max_command_arguments = 1_000);
    assert!(
        stderr.contains("The list operation needs more than 1000 elements."),
        "{stderr}"
    );
}

#[test]
fn cache_writes_stop_at_the_cache_byte_limit() {
    use rcmake_model::{Cache, CacheEntry, CacheType};
    let entry = |value: &str| CacheEntry {
        value: value.into(),
        ty: CacheType::String,
        help: rcmake_base::BString::default(),
        advanced: false,
        props: Default::default(),
    };
    let (mut interp, _) = new_interp(&[], RunMode::Script);
    let base = interp.cx().cache().bytes();
    interp.config.limits.max_cache_bytes = base + 1024;
    interp.cx().insert_cache_entry("SMALL".into(), entry("x"));
    assert!(!interp.fatal_occurred());
    interp
        .cx()
        .insert_cache_entry("BIG".into(), entry(&"y".repeat(2048)));
    assert!(interp.fatal_occurred());
    let mut cx = interp.cx();
    assert!(cx.cache().get(b"BIG").is_none());
    assert!(cx.cache().get(b"SMALL").is_some());

    let mut cache = Cache::default();
    cache.insert("A".into(), entry("abc"));
    let one = cache.bytes();
    if let Some(entry) = cache.get_mut(b"A") {
        entry.value = "abcdef".into();
    }
    assert_eq!(cache.bytes(), one + 3);
    cache.insert("A".into(), entry("a"));
    assert_eq!(cache.bytes(), one - 2);
    assert!(cache.remove(b"A").is_some());
    assert_eq!(cache.bytes(), 0);
}

#[test]
fn zip_lists_charge_generated_names_for_undefined_lists() {
    let prefix = "p".repeat(400);
    let names = (0..20)
        .map(|n| format!("missing{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    let script =
        format!("foreach({prefix} IN ZIP_LISTS {names})\nendforeach()\nset(AFTER reached)\n");
    let (_, stderr, _) = run_real(&script, |limits| limits.max_command_argument_bytes = 4096);
    assert!(
        stderr.contains("The list operation needs more than 4096 bytes."),
        "{stderr}"
    );
}

#[test]
fn foreach_items_share_the_plan_budget() {
    let items = vec!["a"; 600].join(" ");
    let script = format!(
        "set(L \"{}\")\nforeach(x IN LISTS L ITEMS {items})\nendforeach()\n",
        vec!["a"; 600].join(";")
    );
    let (_, stderr, _) = run_real(&script, |limits| limits.max_command_arguments = 1_000);
    assert!(
        stderr.contains("The list operation needs more than 1000 elements."),
        "{stderr}"
    );
}

#[test]
fn nested_macros_share_one_temporary_reservation() {
    // Each level alone fits in the budget. Together they do not.
    let script = format!(
        concat!(
            "set(A \"{}\")\n",
            "macro(level3 v)\n  set(R3 \"${{v}}\")\nendmacro()\n",
            "macro(level2 v)\n  level3(\"${{v}}${{v}}\")\nendmacro()\n",
            "macro(level1 v)\n  level2(\"${{v}}${{v}}\")\nendmacro()\n",
            "level1(\"${{A}}\")\nset(AFTER reached)\n",
        ),
        "x".repeat(50 * 1024)
    );
    let (exit, stderr, interp) = run_with_budget(&script);
    assert_ne!(exit, 0);
    assert!(stderr.contains(BUDGET_MESSAGE), "{stderr}");
    assert_eq!(interp.vars.get(b"R3"), None);
    assert_eq!(interp.temp_bytes, 0, "every reservation is released");

    let alone = format!(
        "set(A \"{}\")\nmacro(level3 v)\n  set(R3 \"${{v}}\")\nendmacro()\nlevel3(\"${{A}}${{A}}\")\n",
        "x".repeat(50 * 1024)
    );
    let (exit, stderr, interp) = run_with_budget(&alone);
    assert_eq!(exit, 0, "{stderr}");
    assert_eq!(interp.temp_bytes, 0);
}

#[test]
fn structured_block_nesting_has_a_depth_limit() {
    let depth = 12;
    let script = format!(
        "set(CMAKE_MAXIMUM_RECURSION_DEPTH 10)\n{}set(INNER reached)\n{}set(AFTER reached)\n",
        "if(TRUE)\n".repeat(depth),
        "endif()\n".repeat(depth)
    );
    let (exit, stderr, interp) = run_real(&script, |_| {});
    assert_ne!(exit, 0);
    assert!(
        stderr.contains("Maximum nesting depth of 10 exceeded by structured blocks."),
        "{stderr}"
    );
    assert_eq!(interp.vars.get(b"INNER"), None);

    let shallow = format!(
        "{}set(INNER reached)\n{}",
        "if(TRUE)\n".repeat(depth),
        "endif()\n".repeat(depth)
    );
    let (exit, stderr, interp) = run_real(&shallow, |_| {});
    assert_eq!(exit, 0, "{stderr}");
    assert!(interp.vars.get(b"INNER").is_some());
}

#[test]
fn zip_lists_save_each_loop_variable_once() {
    let big = "x".repeat(300_000);
    // Empty lists: no iteration changes a variable, so nothing is saved.
    let empty = format!(
        "set(X \"{big}\")\nforeach(X X X X IN ZIP_LISTS e1 e2 e3 e4)\nendforeach()\nset(AFTER reached)\n"
    );
    let (exit, stderr, interp) = run_with_budget(&empty);
    assert_eq!(exit, 0, "{stderr}");
    assert!(interp.vars.get(b"AFTER").is_some());
    assert_eq!(
        interp.vars.get(b"X").map(|value| value.len()),
        Some(big.len())
    );

    // A repeated name is saved once, so its copy fits.
    let repeated = format!(
        "set(X \"{big}\")\nset(L a)\nforeach(X X X X IN ZIP_LISTS L L L L)\nendforeach()\nset(AFTER reached)\n"
    );
    let (exit, stderr, interp) = run_with_budget(&repeated);
    assert_eq!(exit, 0, "{stderr}");
    assert_eq!(
        interp.vars.get(b"X").map(|value| value.len()),
        Some(big.len())
    );
    assert_eq!(interp.temp_bytes, 0);

    // Distinct names each need a saved copy, and the copies do not fit.
    let distinct = format!(
        "set(A1 \"{big}\")\nset(A2 \"{big}\")\nset(A3 \"{big}\")\nset(L a)\nforeach(A1 A2 A3 IN ZIP_LISTS L L L)\nendforeach()\nset(AFTER reached)\n"
    );
    let (exit, stderr, interp) = run_with_budget(&distinct);
    assert_ne!(exit, 0);
    assert!(stderr.contains(BUDGET_MESSAGE), "{stderr}");
    assert_eq!(interp.vars.get(b"AFTER"), None);
    assert_eq!(interp.temp_bytes, 0);
}

#[test]
fn nested_macros_charge_command_names_and_replacement_patterns() {
    // Each copied body holds both spellings of a long command name.
    let name = "n".repeat(300_000);
    let body = format!("  if(FALSE)\n    {name}()\n  endif()\n");
    let inner = format!("macro(inner)\n{body}endmacro()\n");
    let nested =
        format!("{inner}macro(outer)\n  inner()\n{body}endmacro()\nouter()\nset(AFTER reached)\n");
    let (exit, stderr, interp) = run_with_budget(&nested);
    assert_ne!(exit, 0);
    assert!(stderr.contains(BUDGET_MESSAGE), "{stderr}");
    assert_eq!(interp.vars.get(b"AFTER"), None);
    assert_eq!(interp.temp_bytes, 0);
    let (exit, stderr, _) = run_with_budget(&format!("{inner}inner()\n"));
    assert_eq!(exit, 0, "{stderr}");

    // Each call builds a `${<param>}` pattern for a long parameter name.
    let param = |n: char| n.to_string().repeat(400_000);
    let patterns = format!(
        "macro(m3 {})\nendmacro()\nmacro(m2 {})\n  m3(x)\nendmacro()\nmacro(m1 {})\n  m2(x)\nendmacro()\nm1(x)\nset(AFTER reached)\n",
        param('c'),
        param('b'),
        param('a')
    );
    let (exit, stderr, interp) = run_with_budget(&patterns);
    assert_ne!(exit, 0);
    assert!(stderr.contains(BUDGET_MESSAGE), "{stderr}");
    assert_eq!(interp.temp_bytes, 0);
    let alone = format!("macro(m3 {})\nendmacro()\nm3(x)\n", param('c'));
    let (exit, stderr, _) = run_with_budget(&alone);
    assert_eq!(exit, 0, "{stderr}");
}

#[test]
fn macro_command_array_slots_count_toward_the_reservation() {
    // Short commands without arguments: only the copied command array is large.
    let body = "  endif_marker()\n".repeat(20_000);
    let script = format!("macro(many)\n{body}endmacro()\nmany()\nset(AFTER reached)\n");
    let (exit, stderr, interp) = run_with_budget(&script);
    assert_ne!(exit, 0);
    assert!(stderr.contains(BUDGET_MESSAGE), "{stderr}");
    assert_eq!(interp.vars.get(b"AFTER"), None);
    assert_eq!(interp.temp_bytes, 0);
}
