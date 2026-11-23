// CMake numeric comparisons read a floating-point prefix from each operand.

pub(crate) fn prefix(input: &[u8]) -> Option<f64> {
    parse(input).map(|(value, _)| value)
}

pub(crate) fn whole(input: &[u8]) -> Option<f64> {
    let (value, tail) = parse(input)?;
    tail.is_empty().then_some(value)
}

fn parse(input: &[u8]) -> Option<(f64, &[u8])> {
    let leading_space = input
        .iter()
        .take_while(|&&byte| byte.is_ascii_whitespace() || byte == b'\x0b')
        .count();
    let input = input.get(leading_space..)?;
    let (negative, body) = sign(input);
    let special = consume_word(body, b"inf")
        .map(|tail| {
            let tail = consume_word(tail, b"inity").unwrap_or(tail);
            (f64::INFINITY, tail)
        })
        .or_else(|| {
            consume_word(body, b"nan").map(|tail| {
                let tail = tail
                    .strip_prefix(b"(")
                    .and_then(|payload| {
                        let (_, tail) =
                            digits(payload, |byte| byte.is_ascii_alphanumeric() || byte == b'_');
                        tail.strip_prefix(b")")
                    })
                    .unwrap_or(tail);
                (f64::NAN, tail)
            })
        });
    if let Some((value, tail)) = special.or_else(|| hex(body)) {
        return Some((if negative { -value } else { value }, tail));
    }

    let (integer, tail) = digits(body, |byte| byte.is_ascii_digit());
    let (fraction, tail) = fraction(tail, |byte| byte.is_ascii_digit());
    if integer.is_empty() && fraction.is_empty() {
        return None;
    }

    let (_, tail) = exponent(tail, b'e');
    let prefix = input.get(..input.len() - tail.len())?;
    let value = core::str::from_utf8(prefix).ok()?.parse().ok()?;
    Some((value, tail))
}

fn sign(input: &[u8]) -> (bool, &[u8]) {
    if let Some(rest) = input.strip_prefix(b"-") {
        (true, rest)
    } else {
        (false, input.strip_prefix(b"+").unwrap_or(input))
    }
}

fn consume_word<'a>(input: &'a [u8], word: &[u8]) -> Option<&'a [u8]> {
    let (prefix, tail) = input.split_at_checked(word.len())?;
    prefix.eq_ignore_ascii_case(word).then_some(tail)
}

fn digits(input: &[u8], accepts: impl Fn(u8) -> bool) -> (&[u8], &[u8]) {
    let count = input.iter().take_while(|&&byte| accepts(byte)).count();
    input.split_at_checked(count).unwrap_or((b"", input))
}

fn fraction(input: &[u8], accepts: impl Fn(u8) -> bool) -> (&[u8], &[u8]) {
    input
        .strip_prefix(b".")
        .map_or((b"", input), |rest| digits(rest, accepts))
}

fn exponent(input: &[u8], marker: u8) -> (i64, &[u8]) {
    let Some((&byte, rest)) = input.split_first() else {
        return (0, input);
    };
    if byte.to_ascii_lowercase() != marker {
        return (0, input);
    }
    let (negative, rest) = sign(rest);
    let (digits, tail) = digits(rest, |byte| byte.is_ascii_digit());
    if digits.is_empty() {
        return (0, input);
    }
    let value = digits.iter().fold(0_i64, |value, byte| {
        value
            .saturating_mul(10)
            .saturating_add(i64::from(byte - b'0'))
    });
    (if negative { -value } else { value }, tail)
}

fn hex(input: &[u8]) -> Option<(f64, &[u8])> {
    let body = consume_word(input, b"0x")?;
    let (integer, tail) = digits(body, |byte| byte.is_ascii_hexdigit());
    let (fraction, tail) = fraction(tail, |byte| byte.is_ascii_hexdigit());
    if integer.is_empty() && fraction.is_empty() {
        return None;
    }
    let (scale, tail) = exponent(tail, b'p');
    let fraction_bits = i64::try_from(fraction.len())
        .unwrap_or(i64::MAX)
        .saturating_mul(4);
    let mut significand = 0_u64;
    let mut bit_count = 0_i64;
    let mut sticky = false;
    for &byte in integer.iter().chain(fraction) {
        let digit = char::from(byte).to_digit(16)?;
        if bit_count == 0 {
            if digit != 0 {
                bit_count = i64::from(digit.ilog2()) + 1;
                significand = u64::from(digit);
            }
        } else {
            bit_count = bit_count.saturating_add(4);
            // Keep the leading 53 bits, plus at least one bit for rounding.
            if bit_count <= 57 {
                significand = (significand << 4) | u64::from(digit);
            } else {
                sticky |= digit != 0;
            }
        }
    }

    if bit_count == 0 {
        return Some((0.0, tail));
    }
    let top_bit = scale
        .saturating_add(bit_count - 1)
        .saturating_sub(fraction_bits);
    Some((rounded_float(significand, top_bit, sticky), tail))
}

fn rounded_float(significand: u64, mut top_bit: i64, sticky: bool) -> f64 {
    if top_bit > 1023 {
        return f64::INFINITY;
    }
    if top_bit < -1075 {
        return 0.0;
    }

    let retained = if top_bit >= -1022 { 53 } else { top_bit + 1075 };
    let mut rounded = match (i64::from(significand.ilog2()) + 1) - retained {
        shift if shift <= 0 => significand << -shift,
        shift => {
            let high = significand >> shift;
            let half = 1 << (shift - 1);
            let low = significand & ((half << 1) - 1);
            high + u64::from(low > half || (low == half && (sticky || high & 1 != 0)))
        }
    };

    if top_bit < -1022 {
        return f64::from_bits(rounded);
    }
    if rounded == 1 << 53 {
        rounded >>= 1;
        top_bit += 1;
    }
    let exponent = u64::try_from(top_bit + 1023).unwrap_or_default();
    let fraction = rounded & ((1 << 52) - 1);
    f64::from_bits((exponent << 52) | fraction)
}
