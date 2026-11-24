//! Generate inputs and check engine properties.

mod archive;
mod documents;
mod expr;
mod genex;
mod lexical;
mod syntax;
mod tolerant;

// For W0, add both lines under [dependencies] in crates/rcmake-fuzz/Cargo.toml.
// rcmake-fortran.workspace = true
// rcmake-store.workspace = true
// Then register fortran and store_frame here and add their Surface entries.
// Register binfmt when rcmake-base exports the binfmt module.
// Register diagnostics when rcmake-diag exports annotate, rich, and suggest
// and adds the annotations field to Diagnostic.

use std::path::{Path, PathBuf};

use rcmake_base::{BString, Limits};
use rcmake_host::PureHost;

use crate::{Rng, Surface};

type Check = Result<bool, String>;

pub(super) fn run(surface: Surface, seed: u64) -> Check {
    let mut rng = Rng::new(seed);
    let limits = Limits::fuzz();
    match surface {
        Surface::Syntax => syntax::check(&mut rng, &limits),
        Surface::Expr => expr::check(&mut rng, &limits),
        Surface::Regex => lexical::regex(&mut rng, &limits),
        Surface::Genex => genex::check(&mut rng, &limits),
        Surface::Json => documents::json(&mut rng, &limits),
        Surface::Presets => documents::presets(&mut rng, &limits),
        Surface::Cps => documents::cps(&mut rng, &limits),
        Surface::Archive => archive::check(&mut rng, &limits),
        Surface::Path => lexical::path_list(&mut rng, &limits),
        Surface::Tolerant => tolerant::check(&mut rng),
    }
}

fn pick<'a, T, const N: usize>(rng: &mut Rng, choices: &'a [T; N]) -> &'a T {
    const { assert!(N > 0) };
    let [first, ..] = choices.as_slice() else {
        unreachable!("Fuzz choices must contain at least one item");
    };
    choices.get(rng.below(N as u64) as usize).unwrap_or(first)
}

fn random_bytes(rng: &mut Rng) -> Vec<u8> {
    let len = *pick(rng, &[0, 1, 2, 7, 31, 63, 64, 65, 255, 256, 257]);
    rng.bytes(len)
}

fn mutate(rng: &mut Rng, input: &mut Vec<u8>, splice: &[u8]) {
    if input.is_empty() {
        input.extend_from_slice(splice);
        return;
    }
    let start = rng.below(input.len() as u64) as usize;
    let end = start + 1 + rng.below((input.len() - start).min(32) as u64) as usize;
    match rng.below(5) {
        0 => {
            if let Some(byte) = input.get_mut(start) {
                *byte ^= 1 << rng.below(8);
            }
        }
        1 => input.insert(start, rng.next_u64() as u8),
        2 => {
            input.drain(start..end);
        }
        3 => {
            input.splice(start..end, splice.iter().copied());
        }
        _ => {
            if let Some(repeat) = input.get(start..end).map(<[u8]>::to_vec) {
                input.splice(end..end, repeat);
            }
        }
    }
}

fn preview(input: &[u8]) -> String {
    let head = BString::from(input.get(..96).unwrap_or(input));
    format!("{head:?} ({} bytes)", input.len())
}

fn require(condition: bool, detail: impl FnOnce() -> String) -> Result<(), String> {
    if condition { Ok(()) } else { Err(detail()) }
}

fn setup<T, E: std::fmt::Display>(result: Result<T, E>) -> Result<T, String> {
    result.map_err(|error| format!("Test fixture error. {error}"))
}

fn root() -> &'static Path {
    Path::new(if cfg!(windows) { "C:\\" } else { "/" })
}

fn in_memory_host() -> PureHost {
    let root = root();
    PureHost::new(PathBuf::from(root), root.join("cmake"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FuzzOptions, options::DEFAULT_BUDGET, run as run_surface};

    #[test]
    fn runs_all_surfaces_with_fixed_seeds() -> Result<(), String> {
        for surface in Surface::ALL {
            let report = run_surface(&FuzzOptions {
                surface,
                seed: 1,
                iterations: 32,
                budget: DEFAULT_BUDGET,
            })
            .map_err(|error| error.to_string())?;
            assert_eq!(report.iterations, 32, "{surface:?}");
            assert!(report.rejected < report.iterations, "{surface:?}");
            assert!(
                report.failures.is_empty(),
                "{surface:?}: {:?}",
                report.failures
            );
        }
        Ok(())
    }
}
