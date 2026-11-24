use rcmake_base::Limits;
use rcmake_cps::{
    CPS_VERSION, CpsComponent, CpsComponentType, CpsPackage, CpsUsage, reader::read_package,
    writer::write_package,
};
use rcmake_json::{ReadOptions, parse, write_compact, write_styled};
use rcmake_presets::{EnvSnapshot, PresetKind, load, resolve};

use super::{Check, in_memory_host, mutate, pick, preview, random_bytes, require, root, setup};
use crate::Rng;

pub(super) fn json(rng: &mut Rng, limits: &Limits) -> Check {
    let options = ReadOptions {
        allow_comments: rng.below(2) == 0,
        dup_keys_last_wins: rng.below(2) == 0,
    };
    let mode = rng.below(16);
    let (input, expected) = match mode {
        0 => (random_bytes(rng), None),
        1 => {
            let depth = *pick(rng, &[32, 63, 64, 65, 999, 1000, 1001]);
            (
                format!("{}0{}", "[".repeat(depth), "]".repeat(depth)).into_bytes(),
                Some(depth <= 1000),
            )
        }
        2 => (br#"{"x": "\uD800"}"#.to_vec(), Some(false)),
        3 => (
            br#"{"x":0,"x":1}"#.to_vec(),
            Some(options.dup_keys_last_wins),
        ),
        4 if rng.below(128) == 0 => (b"{\"x\":\"\xff\"}".to_vec(), None),
        6 => (
            br#"/* comment */ {"x":0}"#.to_vec(),
            Some(options.allow_comments),
        ),
        _ => {
            let depth = rng.below(5);
            let mut input = json_tree(rng, depth).into_bytes();
            if mode == 5 {
                mutate(rng, &mut input, b"\"\xff\",]");
                (input, None)
            } else {
                (input, Some(true))
            }
        }
    };
    check_json(&input, &options, limits, expected)
}

fn json_tree(rng: &mut Rng, depth: u64) -> String {
    if depth != 0 {
        return match rng.below(3) {
            0 => format!(
                "[{},{}]",
                json_tree(rng, depth - 1),
                json_tree(rng, depth - 1)
            ),
            1 => format!(
                r#"{{"a":{},"b":{}}}"#,
                json_tree(rng, depth - 1),
                json_tree(rng, depth - 1)
            ),
            _ => json_tree(rng, 0),
        };
    }
    match rng.below(4) {
        0 => (rng.next_u64() as i64).to_string(),
        1 => rng.next_u64().to_string(),
        2 => format!("{}.125e-12", rng.next_u64() % 1_000_000),
        _ => (*pick(
            rng,
            &[
                "null",
                "false",
                "true",
                "[]",
                "{}",
                r#""\u0000\r\n\t""#,
                r#""\ud83d\ude80""#,
                "\"é漢\"",
            ],
        ))
        .into(),
    }
}

fn check_json(
    input: &[u8],
    options: &ReadOptions,
    limits: &Limits,
    expected: Option<bool>,
) -> Check {
    if input.len() as u64 > limits.max_file_read {
        return Ok(false);
    }
    let result = parse(input, options);
    if let Some(expected) = expected {
        require(result.is_ok() == expected, || {
            format!(
                "Unexpected JSON parse result for {}. {:?}",
                preview(input),
                result.as_ref().err()
            )
        })?;
    }
    let value = match result {
        Ok(value) => value,
        Err(error) => {
            require(
                error.line > 0 && error.column > 0 && !error.message.is_empty(),
                || {
                    format!(
                        "Incomplete JSON diagnostic for {}. {error:?}",
                        preview(input)
                    )
                },
            )?;
            return Ok(false);
        }
    };
    let outputs = [
        ("compact", write_compact(&value)),
        ("styled", write_styled(&value, "  ")),
    ];
    // The parser keeps raw bytes. The writers can replace invalid UTF-8.
    if std::str::from_utf8(input).is_err() {
        return Ok(true);
    }
    for (name, output) in outputs {
        match parse(&output, options) {
            Ok(decoded) if decoded == value => {}
            Ok(_) => {
                return Err(format!(
                    "{name} JSON round trip changed the value. Input {}. Output {}.",
                    preview(input),
                    preview(&output),
                ));
            }
            Err(error) => {
                return Err(format!(
                    "{name} JSON writer output failed to parse. {error} Input {}. Output {}.",
                    preview(input),
                    preview(&output),
                ));
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
#[test]
fn json_writer_can_replace_invalid_utf8() {
    let options = ReadOptions::default();
    let limits = Limits::fuzz();
    assert_eq!(
        check_json(b"{\"x\":\"\xff\"}", &options, &limits, Some(true)),
        Ok(true)
    );
    assert_eq!(
        check_json(br#"{"x":"\ufffd"}"#, &options, &limits, Some(true)),
        Ok(true)
    );
}

pub(super) fn presets(rng: &mut Rng, limits: &Limits) -> Check {
    let host = in_memory_host();
    let project = root().join("project");
    let mode = rng.below(12);
    let depth = if mode == 0 {
        *pick(rng, &[63, 64, 65])
    } else {
        1 + rng.below(8) as usize
    };
    for index in 0..depth {
        let file = if index == 0 {
            "CMakePresets.json".into()
        } else {
            format!("{index}.json")
        };
        let last = index + 1 == depth;
        let next = if last {
            "CMakePresets.json".into()
        } else {
            format!("{}.json", index + 1)
        };
        let mut input = if !last || mode == 1 {
            format!(r#"{{"version":13,"include":["{next}"]}}"#).into_bytes()
        } else {
            let parent = if mode == 2 {
                r#","inherits":"child""#
            } else {
                ""
            };
            format!(r#"{{"version":13,"configurePresets":[{{"name":"base","hidden":true,"generator":"Ninja","binaryDir":"${{sourceDir}}/build"{parent}}},{{"name":"child","inherits":"base"}}]}}"#).into_bytes()
        };
        if last {
            match mode {
                3 => input = br#"{"version":13,"configurePresets":[}"#.to_vec(),
                4 => input = br#"{"version":13,"configurePresets":[{"name":123}]}"#.to_vec(),
                5 => mutate(rng, &mut input, b"\xff\0"),
                6 => input = random_bytes(rng),
                _ => {}
            }
        }
        if input.len() as u64 > limits.max_file_read {
            return Ok(false);
        }
        setup(host.seed_file(&project.join(file), &input))?;
    }
    let result = load(&project, &host);
    match result {
        Ok(presets) => {
            require(!matches!(mode, 1..=4), || {
                format!("Preset loader accepted invalid graph or schema mode {mode}.")
            })?;
            if mode >= 7 {
                let preset = resolve(
                    &presets,
                    PresetKind::Configure,
                    b"child",
                    &EnvSnapshot::from_host(&host),
                )
                .map_err(|err| format!("Cannot resolve generated configure preset. {err}"))?;
                require(
                    preset.binary_dir
                        == Some(rcmake_host::path::from_native_cmake(&project.join("build"))),
                    || {
                        format!(
                            "Unexpected build directory in generated configure preset. {:?}",
                            preset.binary_dir
                        )
                    },
                )?;
            }
            Ok(true)
        }
        Err(error) => {
            let diagnostic = match mode {
                1 => "Cyclic include",
                2 => "Cyclic preset inheritance",
                _ => "",
            };
            require(
                !error.message.is_empty() && error.message.contains(diagnostic) && mode < 7,
                || {
                    format!(
                        "Unexpected preset diagnostic for mode {mode} and graph depth {depth}. {error}"
                    )
                },
            )?;
            Ok(false)
        }
    }
}

const CPS_PATH: &[u8] = b"/sdk/lib/cps/Pkg.cps";

pub(super) fn cps(rng: &mut Rng, limits: &Limits) -> Check {
    let mode = rng.below(8);
    let package = CpsPackage {
        name: format!("Pkg{}", rng.below(256)).into(),
        cps_version: CPS_VERSION.into(),
        version: Some(format!("1.{}.{}", rng.below(10), rng.below(10)).into()),
        description: pick(
            rng,
            &[
                None,
                Some("café"),
                Some("🧪"),
                Some("A \"quote\"\nA line\0"),
            ],
        )
        .map(Into::into),
        cps_path: Some("@prefix@/lib/cps".into()),
        default_components: vec!["lib0".into()],
        components: (0..1 + rng.below(5))
            .map(|i| {
                let component = CpsComponent {
                    ty: CpsComponentType::Interface,
                    usage: CpsUsage {
                        includes: [("*".into(), vec![format!("/sdk/include/{i}").into()])].into(),
                        requires: vec![":headers".into()],
                        ..CpsUsage::default()
                    },
                    license: None,
                    configurations: Default::default(),
                };
                (format!("lib{i}").into(), component)
            })
            .collect(),
        ..CpsPackage::default()
    };
    let written = write_package(&package)
        .map_err(|error| format!("Cannot write generated CPS package. {error}"))?;
    let mut input = written.to_vec();
    match mode {
        0 => input = random_bytes(rng),
        1 => mutate(rng, &mut input, b"\"\xff\":null"),
        2 => input = br#"{"name":"Pkg","cps_version":"0.15.0","components":[]}"#.to_vec(),
        3 => input = br#"{"name":"Pkg","cps_version":"99.0.0","components":{}}"#.to_vec(),
        _ => {}
    }
    if input.len() as u64 > limits.max_file_read {
        return Ok(false);
    }
    let result = read_package(&input, CPS_PATH);
    match result {
        Ok(read) => {
            require(!matches!(mode, 2 | 3), || {
                format!("CPS reader accepted invalid input {}.", preview(&input))
            })?;
            require(mode < 4 || read == package, || {
                format!(
                    "CPS writer round trip changed the generated package. Expected {package:?}. Read {read:?}."
                )
            })?;
            Ok(true)
        }
        Err(error) => {
            require(mode < 4 && !error.to_string().is_empty(), || {
                format!(
                    "Unexpected CPS diagnostic. {error} Input {}.",
                    preview(&input)
                )
            })?;
            Ok(false)
        }
    }
}
