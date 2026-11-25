use std::sync::Arc;

use rcmake_diag::source::SourceMap;

use super::*;

fn parse(input: &[u8]) -> Result<TolerantParse, &'static str> {
    let source = SourceMap::default()
        .add("CMakeLists.txt".into(), SourceKind::File, Arc::from([]))
        .ok_or("No source identifier")?;
    Ok(parse_tolerant(source, input))
}

#[test]
fn editor_limits_bound_dense_input_and_output() -> Result<(), &'static str> {
    let input = vec![b'('; MAX_INPUT_BYTES + 1];
    let result = parse(&input)?;
    assert_eq!(result.errors.len(), 1);
    assert!(result.errors.first().is_some_and(|error| {
        error.message.as_slice() == b"Listfile exceeds the editor input size limit."
    }));

    let input = vec![b'('; MAX_NESTING + 1];
    let result = parse(&input)?;
    assert!(result.errors.first().is_some_and(|error| {
        error.message.as_slice() == b"Listfile exceeds the editor parenthesis nesting limit."
    }));

    let input = b"!\n".repeat(MAX_TOKENS / 2 + 1);
    let result = parse(&input)?;
    assert!(result.errors.first().is_some_and(|error| {
        error.message.as_slice() == b"Listfile exceeds the editor token count limit."
    }));

    let input = b"set()\n".repeat(MAX_OUTPUT_TOKENS / 4 + 1);
    let result = parse(&input)?;
    assert!(result.commands.is_empty());
    assert!(result.errors.first().is_some_and(|error| {
        error.message.as_slice() == b"Listfile exceeds the editor output limit."
    }));
    Ok(())
}

#[test]
fn open_quote_can_hide_a_multiline_command() -> Result<(), &'static str> {
    let result = parse(b"message(\"oops\nset(value\n  \"ok\")\nmessage(done)")?;
    assert_eq!(result.commands.len(), 2);
    assert_eq!(result.partial_commands.len(), 1);
    assert!(result.commands.first().is_some_and(|command| {
        command.name.as_slice() == b"set"
            && command
                .args
                .last()
                .is_some_and(|arg| arg.text.as_slice() == b"ok")
    }));
    assert!(result.partial_commands.first().is_some_and(|command| {
        command.unclosed_argument == Some(0)
            && command
                .args
                .last()
                .is_some_and(|arg| arg.text.as_slice() == b"oops\n")
    }));
    Ok(())
}

#[test]
fn recovery_keeps_balanced_brackets_after_repeated_open_quotes() -> Result<(), &'static str> {
    let mut input = b"message(\"oops\n".repeat(20);
    input.extend_from_slice(
        b"set(value [=[\nfake(\"text\")\n]=])\nmessage(\"line\nfake(\nmore\" tail)\n",
    );
    let result = parse(&input)?;
    assert_eq!(result.partial_commands.len(), 20);
    assert_eq!(result.commands.len(), 2);
    assert!(result.commands.first().is_some_and(|command| {
        command.name.as_slice() == b"set"
            && command.args.last().is_some_and(|arg| {
                arg.kind == ArgKind::Bracket && arg.text.as_slice() == b"fake(\"text\")\n"
            })
    }));
    assert!(result.commands.last().is_some_and(|command| {
        command
            .args
            .first()
            .is_some_and(|arg| arg.text.as_slice() == b"line\nfake(\nmore")
    }));
    Ok(())
}

#[test]
fn partial_arguments_record_open_delimiters() -> Result<(), &'static str> {
    let open = parse(b"set(value \"valid\" [=[still open")?;
    assert_eq!(
        open.partial_commands
            .first()
            .and_then(|command| command.unclosed_argument),
        Some(2)
    );

    let closed = parse(b"set(value \"valid\" [=[closed]=]")?;
    assert!(
        closed.partial_commands.first().is_some_and(|command| {
            command.args.len() == 3 && command.unclosed_argument.is_none()
        })
    );
    Ok(())
}

#[test]
fn recovery_reports_open_blocks_in_source_order() -> Result<(), &'static str> {
    let mut input = b"set(x)\n".repeat(2_000);
    input.extend_from_slice(b"while()\nif()\nblock()\nendif()\nforeach()\nendwhile()\nmacro()\n");
    input.extend_from_slice(&b"if(\n".repeat(2_000));
    let result = parse(&input)?;
    assert_eq!(result.unclosed_blocks, [2_002, 2_004, 2_006]);
    assert_eq!(result.partial_commands.len(), 2_000);
    Ok(())
}
