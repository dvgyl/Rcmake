#![allow(clippy::expect_used)]

use super::*;

fn capture(pattern: &[u8], input: &[u8]) -> Option<Captures> {
    Regex::new(pattern)
        .expect("valid pattern")
        .find_at(input, 0, MatchOptions::default(), &mut 100_000)
        .expect("sufficient budget")
}

fn spans(pattern: &[u8], input: &[u8], count: usize) -> Vec<Option<(usize, usize)>> {
    capture(pattern, input)
        .expect("matching input")
        .groups
        .into_iter()
        .take(count)
        .collect()
}

#[test]
fn searches_raw_bytes_and_includes_end_of_input() {
    assert_eq!(spans(b"^...$", &[0xc3, 0xa9, b'\n'], 1), vec![Some((0, 3))]);
    assert_eq!(spans(b".", b"\0", 1), vec![Some((0, 1))]);
    assert_eq!(spans(b"a{2}", b"aa a{2}", 1), vec![Some((3, 7))]);
    assert_eq!(spans(br"\d\1\.", b"xd1.", 1), vec![Some((1, 4))]);
    assert_eq!(spans(b"$", b"a\n", 1), vec![Some((2, 2))]);
    assert!(capture(b"a$", b"a\n").is_none());
    assert!(capture(b"^a", b"ba").is_none());
    assert_eq!(spans(b"", b"", 1), vec![Some((0, 0))]);
}

#[test]
fn parses_cmake_bracket_classes() {
    let cases: &[(&[u8], &[u8], usize)] = &[
        (b"[a-z]", b"9m", 1),
        (b"[]a]", b"]a", 0),
        (b"[^]a]", b"]ab", 2),
        (b"[-ab]", b"-", 0),
        (b"[ab-]", b"-", 0),
        (b"[--a]", b"0", 0),
        (b"[a-b-c]", b"-c", 1),
        (b"[a-a]", b"a", 0),
        (br"[\]", br"\", 0),
        (br"[\n]", br"\", 0),
        (b"[\x80-\xff]", b"a\xc3", 1),
    ];
    for &(pattern, input, start) in cases {
        assert_eq!(spans(pattern, input, 1), vec![Some((start, start + 1))]);
    }
    assert_eq!(spans(b"[[:alpha:]]", b"x]:]", 1), vec![Some((2, 4))]);
    assert!(capture(b"[[:alpha:]]", b"z]").is_none());
}

#[test]
fn rejects_invalid_cmake_syntax() {
    let cases: &[(&[u8], &str)] = &[
        (b"(a", "Unmatched parentheses."),
        (b"a)", "Unmatched parentheses."),
        (b"[a", "Unmatched []."),
        (b"[]", "Unmatched []."),
        (b"[^]", "Unmatched []."),
        (b"[z-a]", "Invalid range in []."),
        (b"[a--]", "Invalid range in []."),
        (b"\\", "Trailing backslash."),
        (b"a**", "Nested *?+."),
        (b"a+?", "Nested *?+."),
        (b"a??", "Nested *?+."),
        (b"*a", "?+* follows nothing."),
        (b"+a", "?+* follows nothing."),
        (b"?a", "?+* follows nothing."),
        (b"(?:a)", "?+* follows nothing."),
        (b"(?=a)", "?+* follows nothing."),
        (b"(?i)", "?+* follows nothing."),
    ];
    for &(pattern, message) in cases {
        let error = Regex::new(pattern).expect_err("invalid pattern");
        assert_eq!(
            error.message,
            format!("RegularExpression::compile(): {message}")
        );
    }

    for pattern in ["(a*)*", "(a|)*", "(a|)+", "(a?|bc)*", "^*", "$+"] {
        let error = Regex::new(pattern.as_bytes()).expect_err("invalid repetition");
        assert_eq!(
            error.message,
            "RegularExpression::compile() : *+ operand could be empty."
        );
    }
    assert_eq!(
        spans(b"(a?b)*", b"bab", 2),
        vec![Some((0, 3)), Some((1, 3))]
    );
    assert_eq!(spans(b"^?a", b"a", 1), vec![Some((0, 1))]);
}

#[test]
fn limits_the_cmake_bytecode_size() {
    for (atom, max) in [(b"a".as_slice(), 65_523), (b".", 21_842), (b"[a-z]", 2_184)] {
        assert!(Regex::new(&atom.repeat(max)).is_ok());
        let error = Regex::new(&atom.repeat(max + 1)).expect_err("large pattern");
        assert_eq!(
            error.message,
            "RegularExpression::compile(): Expression too big."
        );
    }
}

#[test]
fn backtracks_in_order_and_restores_captures() {
    assert_eq!(spans(b"(a|aa)", b"aa", 2), vec![Some((0, 1)), Some((0, 1))]);
    assert_eq!(
        spans(b"(a|aa)$", b"aa", 2),
        vec![Some((0, 2)), Some((0, 2))]
    );
    assert_eq!(
        spans(b"(a+)a$", b"aaa", 2),
        vec![Some((0, 3)), Some((0, 2))]
    );
    assert_eq!(
        spans(b"(a?)(a)", b"a", 3),
        vec![Some((0, 1)), Some((0, 0)), Some((0, 1))]
    );
    assert_eq!(
        spans(b"(a(b)?)+", b"aba", 3),
        vec![Some((0, 3)), Some((2, 3)), Some((1, 2))]
    );
    assert_eq!(
        spans(b"(a(b?))+", b"aba", 3),
        vec![Some((0, 3)), Some((2, 3)), Some((3, 3))]
    );
    assert_eq!(
        spans(b"(a(b)|a)c", b"ac", 3),
        vec![Some((0, 2)), Some((0, 1)), None]
    );
    assert_eq!(
        spans(b"((a)|b)*c", b"bac", 3),
        vec![Some((0, 3)), Some((1, 2)), Some((1, 2))]
    );
}

#[test]
fn captures_thirty_one_groups_and_exports_ten_match_variables() {
    let pattern = b"(a)".repeat(31);
    let input = b"a".repeat(31);
    assert_eq!(Regex::new(&pattern).expect("31 groups").group_count(), 31);
    let captures = capture(&pattern, &input).expect("31 captures");
    assert_eq!(captures.groups.last(), Some(&Some((30, 31))));
    let (vars, count) = captures.match_vars(&input);
    assert_eq!(
        vars.first().map(|var| var.as_slice()),
        Some(input.as_slice())
    );
    assert!(vars.iter().skip(1).all(|var| var.as_slice() == b"a"));
    assert_eq!(count, Some(9));

    let error = Regex::new(&b"()".repeat(32)).expect_err("32 groups");
    assert_eq!(
        error.message,
        "RegularExpression::compile(): Too many parentheses."
    );
    assert_eq!(
        capture(b"(a?)(a)", b"a").expect("match").match_vars(b"a").1,
        Some(2)
    );
    assert_eq!(
        capture(b"a()", b"a").expect("match").match_vars(b"a").1,
        Some(0)
    );
    assert_eq!(capture(b"()", b"").expect("match").match_vars(b"").1, None);
}

#[test]
fn controls_beginning_anchor_at_nonzero_offsets() {
    let regex = Regex::new(b"^.").expect("valid pattern");
    for (bol_at_start, start, expected) in [
        (true, 0, Some((0, 1))),
        (true, 1, Some((1, 2))),
        (false, 0, Some((0, 1))),
        (false, 1, None),
        (true, 3, None),
    ] {
        let options = MatchOptions {
            bol_at_start,
            ..MatchOptions::default()
        };
        let found = regex
            .find_at(b"abc", start, options, &mut 100)
            .expect("budget");
        assert_eq!(
            found.and_then(|caps| caps.groups.first().copied().flatten()),
            expected
        );
    }
}

#[test]
fn retries_after_an_empty_match() {
    let regex = Regex::new(b"a*").expect("valid pattern");
    let mut from = 0;
    let mut options = MatchOptions {
        bol_at_start: false,
        allow_empty_at_start: true,
    };
    let mut matches = Vec::new();
    let mut budget = 100;
    while let Some(caps) = regex
        .find_at(b"ba", from, options, &mut budget)
        .expect("budget")
    {
        let (start, end) = caps.groups.first().copied().flatten().expect("full match");
        matches.push((start, end));
        from = end;
        options.allow_empty_at_start = start != end;
    }
    assert_eq!(matches, vec![(0, 0), (1, 2), (2, 2)]);

    let regex = Regex::new(b"a*|b").expect("valid pattern");
    let found = regex.find_at(b"bb", 0, options, &mut 100).expect("budget");
    assert_eq!(
        found.and_then(|caps| caps.groups.first().copied().flatten()),
        Some((0, 1))
    );
    let found = Regex::new(b"")
        .expect("valid pattern")
        .find_at(b"b", 0, options, &mut 100)
        .expect("budget");
    assert_eq!(
        found.and_then(|caps| caps.groups.first().copied().flatten()),
        Some((1, 1))
    );
}

#[test]
fn limits_work_during_backtracking_and_literal_comparison() {
    let regex = Regex::new(b"^(a|aa)*b$").expect("valid pattern");
    let mut budget = 100;
    assert_eq!(
        regex.find_at(&[b'a'; 50], 0, MatchOptions::default(), &mut budget),
        Err(RegexLimit)
    );
    assert_eq!(budget, 0);
    let regex = Regex::new(b"abcdef").expect("valid pattern");
    assert_eq!(
        regex.find_at(b"abcdef", 0, MatchOptions::default(), &mut 2),
        Err(RegexLimit)
    );
    assert_eq!(
        regex.find_at(b"", 0, MatchOptions::default(), &mut 0),
        Err(RegexLimit)
    );
}

#[test]
fn expands_cmake_replacement_escapes() {
    let caps = capture(b"(a)(b)?", b"a").expect("match");
    let output = expand_replacement(br"\0-\10-\2-\\-\n", b"a", &caps, 2).expect("replacement");
    assert_eq!(output.as_slice(), b"a-a0--\\-\n");
    for (template, diagnostic) in [
        (br"\3".as_slice(), "out-of-range escape"),
        (br"\t", "Unknown escape"),
        (b"\\", "ends in a backslash"),
    ] {
        let error = expand_replacement(template, b"a", &caps, 2).expect_err("invalid replacement");
        assert!(error.message.contains(diagnostic));
    }
}

#[test]
fn cache_reuses_compiled_patterns() {
    let mut cache = RegexCache::default();
    let first = cache.get_or_compile(b"(a)").expect("valid pattern");
    let second = cache.get_or_compile(b"(a)").expect("valid pattern");
    assert!(Arc::ptr_eq(&first, &second));
    assert!(cache.get_or_compile(b"(").is_err());
}

#[test]
fn converts_cmake_glob_patterns() {
    let cases: &[(&[u8], &[u8], &[u8])] = &[
        (b"a*.?", b"a.b", b"ba.b"),
        (b"a?", b"ab", b"a"),
        (b".*", b".hidden", b"hidden"),
        (b"?", b"\n", b"\xc3\xa9"),
        (b"[a-c]", b"c", b"d"),
        (b"[!ab]", b"c", b"a"),
        (b"[^ab]", b"c", b"b"),
        (b"[]a]", b"]", b"b"),
        (b"[!]a]", b"b", b"]"),
        (b"[]", b"[]", b"]"),
        (b"[!]", b"[!]", b"]"),
        (b"[a*", b"[abc", b"abc"),
        (b"[[]", b"[", b"]"),
        (br"a\*\?", b"a*?", b"abx"),
        (br"a.()+|^$", b"a.()+|^$", b"abc"),
        (br"a\\", br"a\", b"a"),
    ];
    for &(pattern, input, other) in cases {
        let regex = glob::to_regex(pattern);
        assert!(capture(&regex, input).is_some(), "{pattern:?} on {input:?}");
        assert!(capture(&regex, other).is_none(), "{pattern:?} on {other:?}");
    }
    assert_eq!(glob::to_regex(b"a*.?").as_slice(), br"^a.*\..$");
    assert!(capture(&glob::to_regex(b""), b"").is_some());
    assert!(capture(&glob::to_regex(b"*"), b"").is_some());
    assert!(Regex::new(&glob::to_regex(b"[z-a]")).is_err());
}

#[test]
fn matcher_reuses_scratch_after_success_and_budget_exhaustion() -> Result<(), RegexError> {
    let re = Regex::new(b"(a+)(b)|c")?;
    let mut scratch = Scratch::default();
    let options = MatchOptions::default();
    let input = b"aabc";
    let first = re.find_at_with_scratch(input, 0, options, &mut 100, &mut scratch);
    assert!(
        first.is_ok_and(|caps| {
            caps.is_some_and(|caps| caps.groups.get(2) == Some(&Some((2, 3))))
        })
    );
    assert_eq!(
        re.find_at_with_scratch(input, 0, options, &mut 4, &mut scratch),
        Err(RegexLimit)
    );
    let expected = re.find_at(input, 3, options, &mut 100);
    assert!(expected.as_ref().is_ok_and(|caps| caps.is_some()));
    assert_eq!(
        re.find_at_with_scratch(input, 3, options, &mut 100, &mut scratch),
        expected
    );
    Ok(())
}
