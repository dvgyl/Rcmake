//! Command line of the `rcmake-fuzz` binary:
//! `rcmake-fuzz <surface> [--seed <s>] [--iterations <n>] [--budget-ms <ms>]`.

use std::time::Duration;

use crate::Surface;

/// Options of one fuzz run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuzzOptions {
    /// Fuzzed surface.
    pub surface: Surface,
    /// First seed. Iteration `i` uses seed `seed + i`.
    pub seed: u64,
    /// Number of inputs.
    pub iterations: u64,
    /// Wall-clock budget per input.
    pub budget: Duration,
}

/// Default number of inputs (the `scripts/gate.sh` smoke size).
pub const DEFAULT_ITERATIONS: u64 = 10_000;

/// Default budget per input.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(2);

/// Parse the arguments after the program name.
///
/// # Errors
///
/// Returns a usage message for a missing or unknown surface, an unknown option, or a
/// value that is not a non-negative integer.
pub fn parse_args<S: AsRef<str>>(args: &[S]) -> Result<FuzzOptions, String> {
    let mut args = args.iter().map(AsRef::as_ref);
    let surface = match args.next() {
        Some(name) => {
            Surface::from_name(name).ok_or_else(|| format!("unknown surface '{name}'"))?
        }
        None => return Err("missing surface".into()),
    };
    let mut options = FuzzOptions {
        surface,
        seed: 1,
        iterations: DEFAULT_ITERATIONS,
        budget: DEFAULT_BUDGET,
    };
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value after '{flag}'"))?;
        let number: u64 = value
            .parse()
            .map_err(|_| format!("'{value}' is not a valid value for '{flag}'"))?;
        match flag {
            "--seed" => options.seed = number,
            "--iterations" => options.iterations = number,
            "--budget-ms" => options.budget = Duration::from_millis(number),
            _ => return Err(format!("unknown option '{flag}'")),
        }
    }
    Ok(options)
}

/// Usage text of the binary.
#[must_use]
pub fn usage() -> String {
    let surfaces: Vec<&str> = Surface::ALL.iter().map(|surface| surface.name()).collect();
    format!(
        "usage: rcmake-fuzz <surface> [--seed <s>] [--iterations <n>] [--budget-ms <ms>]\nsurfaces: {}\n",
        surfaces.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_options() {
        let options = parse_args(&[
            "regex",
            "--seed",
            "9",
            "--iterations",
            "5",
            "--budget-ms",
            "30",
        ]);
        assert_eq!(
            options,
            Ok(FuzzOptions {
                surface: Surface::Regex,
                seed: 9,
                iterations: 5,
                budget: Duration::from_millis(30),
            })
        );
    }

    #[test]
    fn uses_defaults() {
        let options = parse_args(&["json"]);
        assert_eq!(
            options.map(|o| (o.seed, o.iterations, o.budget)),
            Ok((1, DEFAULT_ITERATIONS, DEFAULT_BUDGET))
        );
    }

    #[test]
    fn rejects_bad_input() {
        let none: [&str; 0] = [];
        assert_eq!(parse_args(&none), Err("missing surface".into()));
        assert!(parse_args(&["nope"]).is_err());
        assert!(parse_args(&["json", "--seed"]).is_err());
        assert!(parse_args(&["json", "--seed", "-1"]).is_err());
        assert!(parse_args(&["json", "--color", "1"]).is_err());
        assert!(usage().contains("archive"));
    }
}
