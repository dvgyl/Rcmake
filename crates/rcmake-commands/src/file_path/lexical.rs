use rcmake_base::BString;

#[derive(Clone, Copy)]
pub(super) struct Lexical<'a> {
    raw: &'a [u8],
    root_name_len: usize,
    relative_start: usize,
}

impl<'a> Lexical<'a> {
    pub(super) fn new(raw: &'a [u8]) -> Self {
        let root_name_len = root_name_len(raw);
        let relative_start = root_name_len
            + raw
                .get(root_name_len..)
                .unwrap_or_default()
                .iter()
                .take_while(|&&b| b == b'/')
                .count();
        Self {
            raw,
            root_name_len,
            relative_start,
        }
    }

    pub(super) fn root_name(self) -> &'a [u8] {
        self.raw.get(..self.root_name_len).unwrap_or_default()
    }

    pub(super) fn root_directory(self) -> &'static [u8] {
        if self.relative_start > self.root_name_len {
            b"/"
        } else {
            b""
        }
    }

    pub(super) fn root_path(self) -> BString {
        [self.root_name(), self.root_directory()].concat().into()
    }

    pub(super) fn absolute(self) -> bool {
        !self.root_directory().is_empty() && (!cfg!(windows) || !self.root_name().is_empty())
    }

    pub(super) fn relative_part(self) -> &'a [u8] {
        self.raw.get(self.relative_start..).unwrap_or_default()
    }

    pub(super) fn filename(self) -> &'a [u8] {
        self.relative_part()
            .rsplit(|&b| b == b'/')
            .next()
            .unwrap_or_default()
    }

    pub(super) fn without_filename(self) -> &'a [u8] {
        self.raw
            .get(..self.raw.len() - self.filename().len())
            .unwrap_or_default()
    }

    pub(super) fn parent(self) -> &'a [u8] {
        if self.relative_part().is_empty() {
            return self.raw;
        }
        let prefix = self.without_filename();
        let len = prefix.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
        prefix
            .get(..len.max(self.relative_start))
            .unwrap_or_default()
    }

    pub(super) fn stem_extension(self, last_only: bool) -> (&'a [u8], &'a [u8]) {
        let name = self.filename();
        if matches!(name, b"." | b"..") {
            return (name, b"");
        }
        let mut dots = name.iter().enumerate().skip(1).filter(|(_, b)| **b == b'.');
        let dot = if last_only {
            dots.next_back()
        } else {
            dots.next()
        };
        name.split_at(dot.map_or(name.len(), |(i, _)| i))
    }

    pub(super) fn components(self) -> impl Iterator<Item = &'a [u8]> {
        self.relative_part()
            .split(|&b| b == b'/')
            .filter(|part| !part.is_empty())
    }

    fn trailing(self) -> bool {
        !self.relative_part().is_empty() && self.raw.ends_with(b"/")
    }

    fn same_root(self, other: Self) -> bool {
        self.root_name() == other.root_name() && self.root_directory() == other.root_directory()
    }

    pub(super) fn equal(self, other: Self) -> bool {
        self.same_root(other)
            && self.components().eq(other.components())
            && self.trailing() == other.trailing()
    }

    pub(super) fn prefix(self, other: Self) -> bool {
        if !self.same_root(other) {
            return false;
        }
        let mut target = other.components();
        self.components().all(|part| target.next() == Some(part))
            && (!self.trailing() || target.next().is_some() || other.trailing())
    }

    pub(super) fn normalize(self) -> BString {
        if self.raw.is_empty() {
            return BString::default();
        }
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut trailing = self.trailing();
        for part in self.components() {
            trailing = match part {
                b"." => true,
                b".." if parts.last().is_some_and(|previous| *previous != b"..") => {
                    parts.pop();
                    true
                }
                b".." if !self.root_directory().is_empty() => true,
                _ => {
                    parts.push(part);
                    false
                }
            };
        }
        let mut output = self.root_path();
        output.extend_from_slice(&parts.join(&b'/'));
        if output.is_empty() {
            output.push(b'.');
        } else if !parts.is_empty()
            && parts.last() != Some(&b"..".as_slice())
            && (trailing || self.trailing())
        {
            output.push(b'/');
        }
        output
    }

    pub(super) fn append(self, input: &[u8]) -> BString {
        let rhs = Lexical::new(input);
        if rhs.absolute() || (!rhs.root_name().is_empty() && self.root_name() != rhs.root_name()) {
            return input.into();
        }
        if !rhs.root_directory().is_empty() {
            return [
                self.root_name(),
                input.get(rhs.root_name_len..).unwrap_or_default(),
            ]
            .concat()
            .into();
        }
        let separator: &[u8] = if !self.filename().is_empty() {
            b"/"
        } else {
            b""
        };
        [self.raw, separator, rhs.relative_part()].concat().into()
    }

    pub(super) fn relative(self, base: Self) -> Option<BString> {
        if !self.same_root(base) {
            return None;
        }
        let mut target = self.components().peekable();
        let mut base_parts = base.components().peekable();
        while target.peek().is_some() && target.peek() == base_parts.peek() {
            target.next();
            base_parts.next();
        }
        let mut parents = 0_i64;
        for part in base_parts {
            match part {
                b"." => {}
                b".." => parents -= 1,
                _ => parents += 1,
            }
        }
        let parents = usize::try_from(parents).ok()?;
        let mut parts = vec![b"..".as_slice(); parents];
        let has_target = target.peek().is_some();
        parts.extend(target);
        if parts.is_empty() {
            return Some(BString::from("."));
        }
        if self.trailing() && has_target {
            parts.push(b"");
        }
        Some(parts.join(&b'/').into())
    }
}

fn root_name_len(raw: &[u8]) -> usize {
    if cfg!(windows) {
        if let [drive, b':', ..] = raw
            && drive.is_ascii_alphabetic()
        {
            return 2;
        }
        if let Some(rest) = raw.strip_prefix(b"//")
            && rest.first().is_some_and(|&b| b != b'/')
        {
            return 2 + rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
        }
    }
    0
}

pub(super) const NATIVE_LIST_SEPARATOR: u8 = if cfg!(windows) { b';' } else { b':' };

pub(super) fn native_list(input: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut remaining = Some((input, 0));
    std::iter::from_fn(move || {
        let (rest, skip) = remaining.take()?;
        let end = rest
            .iter()
            .enumerate()
            .skip(skip)
            .find(|(_, b)| **b == NATIVE_LIST_SEPARATOR);
        if let Some((end, _)) = end {
            let (part, tail) = rest.split_at(end);
            remaining = tail.get(1..).map(|tail| (tail, 1));
            Some(part)
        } else {
            Some(rest)
        }
    })
}

pub(super) fn cmake_style(input: &[u8]) -> BString {
    if cfg!(windows) {
        let input = input.strip_prefix(br"\\?\").unwrap_or(input);
        input
            .iter()
            .map(|&b| if b == b'\\' { b'/' } else { b })
            .collect::<Vec<_>>()
            .into()
    } else {
        input.into()
    }
}

pub(super) fn native_style(input: &[u8]) -> BString {
    if cfg!(windows) {
        input
            .iter()
            .map(|&b| if b == b'/' { b'\\' } else { b })
            .collect::<Vec<_>>()
            .into()
    } else {
        input.into()
    }
}
