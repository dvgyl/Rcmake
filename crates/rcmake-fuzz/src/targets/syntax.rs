use std::sync::Arc;

use rcmake_base::Limits;
use rcmake_diag::{SourceMap, source::SourceKind};
use rcmake_syntax::parse_list_file_with_warnings;

use super::{Check, mutate, pick, preview, random_bytes, require};
use crate::Rng;

pub(super) fn check(rng: &mut Rng, limits: &Limits) -> Check {
    let kind = *pick(rng, &[SourceKind::File, SourceKind::VirtualEval]);
    let mode = rng.below(6);
    if mode == 0 {
        return parse(random_bytes(rng), kind, limits, None);
    }
    let depth = if mode == 1 {
        limits.max_recursion as usize
    } else {
        rng.below(16) as usize
    };
    let blocks: Vec<_> = (0..depth)
        .map(|_| {
            *pick(
                rng,
                &[
                    ("IF((ON AND ON))", "endif()"),
                    ("foreach(x IN ITEMS a b)", "endforeach()"),
                    ("block()", "endblock()"),
                ],
            )
        })
        .collect();
    let mut script = Vec::new();
    for (open, _) in &blocks {
        script.extend_from_slice(open.as_bytes());
        script.push(b'\n');
    }
    let crlf = if kind == SourceKind::File {
        "\r\n"
    } else {
        "\n"
    };
    let width = *pick(rng, &[0, 1, 63, 64, 65, 255, 256, 257, 1024]);
    script.extend_from_slice(
        format!("set(var [==[{crlf}{}]==] \"a;b\")\n", "x".repeat(width)).as_bytes(),
    );
    for (_, close) in blocks.iter().rev() {
        script.extend_from_slice(close.as_bytes());
        script.push(b'\n');
    }
    let expected = match mode {
        2 => {
            script.extend_from_slice(b"if(ON)\nendforeach()\n");
            Some(false)
        }
        3 => {
            mutate(rng, &mut script, b"\xff#[=[");
            None
        }
        _ => Some(true),
    };
    parse(script, kind, limits, expected)
}

fn parse(input: Vec<u8>, kind: SourceKind, limits: &Limits, expected: Option<bool>) -> Check {
    let mut limits = limits.clone();
    limits.max_file_read = limits.max_file_read.min(1024 * 1024);
    limits.max_value_bytes = limits.max_value_bytes.min(1024 * 1024);
    let input: Arc<[u8]> = input.into();
    let mut sources = SourceMap::default();
    let source = sources
        .add("fuzz.cmake".into(), kind, Arc::clone(&input))
        .ok_or("Cannot register listfile source.")?;
    let result = parse_list_file_with_warnings(&input, source, kind, &limits);
    if let Some(expected) = expected {
        require(result.is_ok() == expected, || match &result {
            Ok(_) => "Parser accepted mismatched flow-control commands.".into(),
            Err((error, _)) => format!(
                "Parser rejected a generated listfile. {error} Input {}.",
                preview(&input)
            ),
        })?;
    }
    match result {
        Ok(listfile) => {
            require(listfile.source == source, || {
                "Parser changed the source identifier.".into()
            })?;
            Ok(true)
        }
        Err((error, _)) => {
            require(!error.message.is_empty(), || {
                "Parser returned an empty diagnostic.".into()
            })?;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_file_size_and_flow_control() {
        let limits = Limits::fuzz();
        let mut input = vec![b' '; 1024 * 1024];
        assert_eq!(
            parse(input.clone(), SourceKind::File, &limits, Some(true)),
            Ok(true)
        );
        input.push(b' ');
        assert_eq!(
            parse(input, SourceKind::File, &limits, Some(false)),
            Ok(false)
        );
        assert_eq!(
            parse(
                b"if(ON)\nendforeach()".to_vec(),
                SourceKind::File,
                &limits,
                Some(false)
            ),
            Ok(false)
        );
    }
}
