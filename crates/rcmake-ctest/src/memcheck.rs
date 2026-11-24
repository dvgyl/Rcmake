use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rcmake_base::BString;
use rcmake_host::{FileKind, Host, ReadRange, RunRequest, Stream, path};

use crate::CtestError;
use crate::cli::Invocation;
use crate::dart::{Configuration, Tag};
use crate::junit::{escape, write_file};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);

pub(crate) struct Session {
    directory: PathBuf,
    sanitizer: Sanitizer,
    options: BString,
}

pub(crate) fn requested(invocation: &Invocation) -> bool {
    invocation.actions.iter().any(|action| action == "memcheck")
}

impl Session {
    pub(crate) fn start(
        host: &dyn Host,
        dir: &Path,
        invocation: &Invocation,
    ) -> Result<Option<Self>, CtestError> {
        if !requested(invocation) {
            return Ok(None);
        }
        let config = Configuration::load(host, dir, &invocation.overwrite_configuration_file)
            .map_err(|source| CtestError::Access {
                path: dir.to_path_buf(),
                source,
            })?;
        let sanitizer = Sanitizer::parse(&config.memory_check_type).ok_or_else(|| {
            CtestError::Schedule(format!(
                "Memory check type \"{}\" is not supported.",
                config.memory_check_type
            ))
        })?;
        let nonce = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        let directory = dir
            .join("Testing/Temporary")
            .join(format!("MemoryChecker-{}-{nonce}", host.system().pid));
        host.create_dir_all(&directory)
            .map_err(|source| CtestError::Access {
                path: directory.clone(),
                source,
            })?;
        Ok(Some(Self {
            directory,
            sanitizer,
            options: config.memory_check_options,
        }))
    }

    pub(crate) fn apply(&self, request: &mut RunRequest, index: usize) {
        let key = self.sanitizer.variable();
        let file = path::from_native(&self.directory.join(format!("MemoryChecker.{index}.log")));
        let mut value = format!("log_path='{file}'");
        if !self.options.is_empty() {
            let _ = write!(value, ":{}", self.options);
        }
        request
            .env
            .retain(|(name, _)| name.as_slice() != key.as_bytes());
        request.env.push((key.into(), value.into()));
    }

    pub(crate) fn finish(self, host: &dyn Host, dir: &Path, quiet: bool) -> Result<(), CtestError> {
        let access = |source| CtestError::Access {
            path: self.directory.clone(),
            source,
        };
        let mut logs = host.read_dir(&self.directory).map_err(access)?;
        logs.sort_by(|left, right| left.name.cmp(&right.name));
        let mut defects = Vec::<(String, usize)>::new();
        let mut remaining = 64 * 1024 * 1024;
        for log in logs
            .iter()
            .filter(|log| log.kind == FileKind::File && log.name.starts_with(b"MemoryChecker."))
        {
            let file = self.directory.join(path::to_native(&log.name));
            let content = host
                .read(&file, ReadRange::All { limit: remaining })
                .map_err(access)?;
            remaining = remaining.saturating_sub(content.len() as u64);
            let content = String::from_utf8_lossy(&content);
            for name in content
                .lines()
                .filter_map(|line| self.sanitizer.defect(line))
            {
                if let Some((_, count)) = defects.iter_mut().find(|(known, _)| known == name) {
                    *count += 1;
                } else {
                    defects.push((name.to_owned(), 1));
                }
            }
        }
        if !quiet {
            let mut summary = String::from("Memory checking results:\n");
            for (name, count) in &defects {
                let _ = writeln!(summary, "{name} - {count}");
            }
            host.write_out(Stream::Stdout, summary.as_bytes());
        }
        let tag = Tag::read(host, dir).map_err(access)?;
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Site><DynamicAnalysis>\n<DefectList>\n",
        );
        for (name, count) in defects {
            let name = escape(name.as_bytes(), true);
            let _ = writeln!(xml, "<Defect Type=\"{name}\">{count}</Defect>");
        }
        xml.push_str("</DefectList>\n</DynamicAnalysis></Site>\n");
        write_file(
            host,
            &dir.join("Testing")
                .join(tag.stamp())
                .join("DynamicAnalysis.xml"),
            xml.as_bytes(),
        )
        .map_err(access)
    }
}

#[derive(Clone, Copy)]
enum Sanitizer {
    Address,
    Leak,
    Memory,
    Thread,
    UndefinedBehavior,
}

impl Sanitizer {
    fn parse(value: &[u8]) -> Option<Self> {
        match value {
            b"AddressSanitizer" => Some(Self::Address),
            b"LeakSanitizer" => Some(Self::Leak),
            b"MemorySanitizer" => Some(Self::Memory),
            b"ThreadSanitizer" => Some(Self::Thread),
            b"UndefinedBehaviorSanitizer" => Some(Self::UndefinedBehavior),
            _ => None,
        }
    }

    fn variable(self) -> &'static str {
        match self {
            Self::Address => "ASAN_OPTIONS",
            Self::Leak => "LSAN_OPTIONS",
            Self::Memory => "MSAN_OPTIONS",
            Self::Thread => "TSAN_OPTIONS",
            Self::UndefinedBehavior => "UBSAN_OPTIONS",
        }
    }

    fn defect(self, line: &str) -> Option<&str> {
        match self {
            Self::Address => {
                let (_, description) = line.split_once("ERROR: AddressSanitizer: ")?;
                Some(
                    description
                        .split_once(" on ")
                        .map_or(description, |(name, _)| name),
                )
            }
            Self::Leak => {
                let (name, _) = line.split_once(" of ")?;
                matches!(name, "Direct leak" | "Indirect leak").then_some(name)
            }
            Self::Memory => {
                let (_, description) = line.split_once("WARNING: MemorySanitizer: ")?;
                Some(description.trim())
            }
            Self::Thread => {
                let (_, description) = line.split_once("WARNING: ThreadSanitizer: ")?;
                Some(
                    description
                        .split_once(" (pid=")
                        .map_or(description, |(name, _)| name),
                )
            }
            Self::UndefinedBehavior => {
                let (_, description) = line.split_once("runtime error: ")?;
                Some(description.trim())
            }
        }
    }
}
