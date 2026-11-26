use std::ops::Range;

use rcmake_base::{BString, truth};
use rcmake_model::{ProjectModel, Target};

use crate::lower::{
    definition,
    shell::{rule_invocations, simple_args_spanned},
};
use crate::{
    Invocation, LinkKind, LinkResponse, LinkStep, ResponseQuoting, ResponseRole, ResponseSpan,
};

type Markers = (&'static [u8], &'static [u8], &'static [u8], ResponseRole);

const MARKERS: [Markers; 2] = [
    (
        b"OBJECTS",
        b"__RCMAKE_OBJECTS_BEGIN__",
        b"__RCMAKE_OBJECTS_END__",
        ResponseRole::Objects,
    ),
    (
        b"LINK_LIBRARIES",
        b"__RCMAKE_LIBRARIES_BEGIN__",
        b"__RCMAKE_LIBRARIES_END__",
        ResponseRole::Libraries,
    ),
];

pub(super) fn expand(
    model: &ProjectModel,
    target: &Target,
    step: &LinkStep,
    msvc: bool,
    template: &[u8],
    lookup: impl Fn(&[u8]) -> Option<BString>,
) -> Vec<Invocation> {
    let mut ordinary = rule_invocations(model, target.dir, template, &lookup);
    let Some(settings) = settings(model, target, step, msvc) else {
        return ordinary;
    };
    let marked_template = whole_argument_placeholders(template);
    if marked_template == template {
        return ordinary;
    }
    let marked = rule_invocations(model, target.dir, &marked_template, |key| {
        let Some((name, begin, end, _)) = MARKERS
            .iter()
            .find(|(name, ..)| Some(*name) == key.strip_prefix(b"RCMAKE_RESPONSE_"))
        else {
            return lookup(key);
        };
        let value = lookup(name)?;
        if value.iter().all(u8::is_ascii_whitespace) {
            return Some(value);
        }
        Some([*begin, b" ", value.as_slice(), b" ", *end].concat().into())
    });
    if marked.len() != ordinary.len() {
        return ordinary;
    }
    for (ordinary, marked) in ordinary.iter_mut().zip(marked) {
        if let Some(spans) = spans(ordinary, &marked) {
            ordinary.link_response = Some(Box::new(LinkResponse {
                spans,
                ..settings.clone()
            }));
        }
    }
    ordinary
}

fn whole_argument_placeholders(template: &[u8]) -> BString {
    let mut result = BString::default();
    let mut rest = template;
    let mut previous = None;
    while let Some((&byte, tail)) = rest.split_first() {
        result.push(byte);
        if byte == b'<'
            && previous.is_none_or(boundary)
            && MARKERS.iter().any(|(name, ..)| {
                tail.strip_prefix(*name)
                    .and_then(|tail| tail.strip_prefix(b">"))
                    .is_some_and(|tail| tail.first().copied().is_none_or(boundary))
            })
        {
            result.extend_from_slice(b"RCMAKE_RESPONSE_");
        }
        previous = Some(byte);
        rest = tail;
    }
    result
}

fn boundary(byte: u8) -> bool {
    byte.is_ascii_whitespace() || byte == b';'
}

fn spans(ordinary: &Invocation, marked: &Invocation) -> Option<Vec<ResponseSpan>> {
    let (ordinary_args, ranges) = simple_args_spanned(&ordinary.shell)?;
    if ordinary_args != ordinary.argv
        || ordinary_args.iter().any(|arg| {
            MARKERS
                .iter()
                .any(|(_, begin, end, _)| arg == *begin || arg == *end)
        })
    {
        return None;
    }
    let mut args = Vec::new();
    let mut spans = Vec::new();
    let mut marked = marked.argv.iter();
    while let Some(arg) = marked.next() {
        let Some((_, _, end, role)) = MARKERS.iter().find(|(_, begin, ..)| arg == *begin) else {
            args.push(arg);
            continue;
        };
        let start = args.len();
        loop {
            let arg = marked.next()?;
            if arg == *end {
                break;
            }
            args.push(arg);
        }
        let argv = start..args.len();
        spans.push(ResponseSpan {
            role: *role,
            shell: shell_range(&ranges, &argv)?,
            argv,
        });
    }
    (!spans.is_empty() && args.into_iter().eq(&ordinary_args)).then_some(spans)
}

fn shell_range(ranges: &[Range<usize>], argv: &Range<usize>) -> Option<Range<usize>> {
    let ranges = ranges.get(argv.clone())?;
    Some(ranges.first()?.start..ranges.last()?.end)
}

pub(super) fn rebase(
    invocation: &mut Invocation,
    original: &[BString],
    mut annotation: Box<LinkResponse>,
) {
    if !invocation.argv.ends_with(original) {
        return;
    }
    let Some((args, ranges)) = simple_args_spanned(&invocation.shell) else {
        return;
    };
    if args != invocation.argv {
        return;
    }
    let offset = args.len() - original.len();
    for span in &mut annotation.spans {
        span.argv = span.argv.start + offset..span.argv.end + offset;
        let Some(range) = shell_range(&ranges, &span.argv) else {
            return;
        };
        span.shell = range;
    }
    let controls = wrapper_controls(&args);
    if !controls.is_empty() {
        let mut spans = Vec::new();
        for span in annotation.spans {
            let mut start = span.argv.start;
            for index in controls
                .iter()
                .copied()
                .filter(|index| span.argv.contains(index))
                .chain(std::iter::once(span.argv.end))
            {
                let argv = start..index;
                start = index.saturating_add(1);
                if let Some(shell) = shell_range(&ranges, &argv) {
                    spans.push(ResponseSpan {
                        role: span.role,
                        argv,
                        shell,
                    });
                }
            }
        }
        annotation.spans = spans;
    }
    if !annotation.spans.is_empty() {
        invocation.link_response = Some(annotation);
    }
}

fn wrapper_controls(args: &[BString]) -> Vec<usize> {
    let Some(wrapper) = args.windows(2).position(|pair| {
        matches!(pair, [option, helper] if option == b"-E" && matches!(helper.as_slice(), b"vs_link_exe" | b"vs_link_dll"))
    }) else {
        return Vec::new();
    };
    let Some(separator) = args
        .iter()
        .enumerate()
        .skip(wrapper + 2)
        .find(|(_, arg)| *arg == b"--")
        .map(|(index, _)| index)
    else {
        return Vec::new();
    };
    let mut controls = Vec::new();
    let mut takes_value = false;
    for (index, arg) in args.iter().enumerate().skip(separator + 2) {
        let option = arg
            .as_slice()
            .strip_prefix(b"/")
            .or_else(|| arg.as_slice().strip_prefix(b"-"));
        let upper = option.map(|option| option.to_ascii_uppercase());
        let upper = upper.as_deref().unwrap_or_default();
        if takes_value
            || upper.starts_with(b"OUT:")
            || option.is_some_and(|option| option.starts_with(b"Fe"))
            || matches!(
                upper,
                b"FE" | b"MANIFEST:NO" | b"INCREMENTAL" | b"INCREMENTAL:YES" | b"INCREMENTAL:NO"
            )
        {
            controls.push(index);
        }
        takes_value = !takes_value && matches!(upper, b"OUT:" | b"FE");
    }
    controls
}

fn settings(
    model: &ProjectModel,
    target: &Target,
    step: &LinkStep,
    msvc: bool,
) -> Option<LinkResponse> {
    let language = &step.language;
    if language == b"RC" {
        return None;
    }
    let get = |suffix: &[u8]| {
        definition(
            model,
            target.dir,
            &[b"CMAKE_", language.as_slice(), suffix].concat(),
        )
    };
    let flag = (step.kind == LinkKind::StaticArchive)
        .then(|| get(b"_RESPONSE_FILE_ARCHIVE_FLAG"))
        .flatten()
        .or_else(|| get(b"_RESPONSE_FILE_LINK_FLAG"));
    if language == b"CUDA" && flag.is_none() {
        return None;
    }
    let setting = |suffix| {
        get(suffix)
            .filter(|value| !value.is_empty())
            .map(|value| truth::is_on(value))
    };
    Some(LinkResponse {
        flag: flag.cloned().unwrap_or_else(|| "@".into()),
        quoting: if msvc {
            ResponseQuoting::Msvc
        } else {
            ResponseQuoting::Gnu
        },
        objects: setting(b"_USE_RESPONSE_FILE_FOR_OBJECTS"),
        libraries: setting(b"_USE_RESPONSE_FILE_FOR_LIBRARIES"),
        spans: Vec::new(),
        archive_batch: false,
    })
}
