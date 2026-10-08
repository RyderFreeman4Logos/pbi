//! Invocation-owned, bounded deadline recovery; never a cache or ranking result.
use super::*;
use std::io::{Seek, SeekFrom};
use std::time::Duration;

#[derive(Debug)]
pub(crate) struct Location {
    pub file: String,
    pub line: usize,
}

pub(super) struct Source {
    pub bytes: Vec<u8>,
    file: File,
    directories: Vec<File>,
    relative: PathBuf,
}
impl Source {
    pub(super) fn new(
        bytes: Vec<u8>,
        file: File,
        directories: Vec<File>,
        relative: PathBuf,
    ) -> Self {
        Self {
            bytes,
            file,
            directories,
            relative,
        }
    }
}

#[derive(Default)]
pub(super) struct Progress {
    sources: Vec<(Source, Vec<Location>)>,
    locations: usize,
}
impl Progress {
    pub(super) fn retain(
        &mut self,
        source: Source,
        lines: impl Iterator<Item = usize>,
        limits: &SearchLimits,
    ) -> Result<(), SearchFailure> {
        let capacity = limits.max_results.min(8).saturating_sub(self.locations);
        if capacity == 0 {
            #[cfg(test)]
            tests::after_file()?;
            return Ok(());
        }
        let Some(relative) = source.relative.to_str() else {
            return Ok(());
        };
        if relative.chars().any(char::is_control) {
            return Ok(());
        }
        let count = std::str::from_utf8(&source.bytes).map_or(0, |text| text.lines().count());
        let mut locations = Vec::new();
        for line in lines.filter(|line| *line > 0 && *line <= count) {
            if locations.len() >= capacity {
                break;
            }
            if locations
                .iter()
                .any(|location: &Location| location.line == line)
            {
                continue;
            }
            locations.push(Location {
                file: relative.to_owned(),
                line,
            });
        }
        self.locations += locations.len();
        if !locations.is_empty() {
            self.sources.push((source, locations));
        }
        #[cfg(test)]
        tests::after_file()?;
        Ok(())
    }

    fn verify(self, root: &Path, limits: &SearchLimits) -> Vec<Location> {
        let compiler = std::sync::Mutex::new(PolicyCompiler::default());
        let mut verified = Vec::new();
        let mut output_bytes = 0;
        for (mut source, locations) in self.sources {
            let valid = (|| {
                crate::extract::check_deadline(limits)?;
                check_source_namespace(root, &source.relative, &source.directories, &source.file)?;
                let device = source
                    .file
                    .metadata()
                    .map_err(|_| SearchFailure::Unavailable)?
                    .dev();
                if !policy_admitted(
                    root,
                    &source.relative,
                    &source.directories,
                    device,
                    limits,
                    false,
                    &compiler,
                )? {
                    return Err(SearchFailure::Unavailable);
                }
                source
                    .file
                    .seek(SeekFrom::Start(0))
                    .map_err(|_| SearchFailure::Unavailable)?;
                let bytes = read_source_file(
                    source
                        .file
                        .try_clone()
                        .map_err(|_| SearchFailure::Unavailable)?,
                    device,
                )?;
                if bytes.as_deref() != Some(source.bytes.as_slice()) {
                    return Err(SearchFailure::Unavailable);
                }
                check_source_namespace(root, &source.relative, &source.directories, &source.file)?;
                if !policy_admitted(
                    root,
                    &source.relative,
                    &source.directories,
                    device,
                    limits,
                    false,
                    &compiler,
                )? {
                    return Err(SearchFailure::Unavailable);
                }
                crate::extract::check_deadline(limits)
            })();
            // Any failed custody check invalidates the complete recovery batch.
            if valid.is_err() {
                return Vec::new();
            }
            for location in locations {
                output_bytes += location.file.len() + 24;
                if output_bytes > MAX_OUTPUT_BYTES {
                    return Vec::new();
                }
                verified.push(location);
            }
        }
        verified
    }
}

pub(super) fn run<T>(
    root: &Path,
    limits: &SearchLimits,
    search: impl FnOnce(&SearchLimits, &mut Progress) -> Result<T, SearchFailure>,
) -> Result<T, SearchFailure> {
    let now = Instant::now();
    // Reserve part of the existing total budget for custody verification, not
    // extra time after expiry. Walk-only failures have no owned source to retain.
    let remaining = limits.deadline.saturating_duration_since(now);
    let reserve = (remaining / 10).min(Duration::from_millis(100));
    let scan_limits = SearchLimits {
        deadline: limits.deadline.checked_sub(reserve).unwrap_or(now),
        max_results: limits.max_results,
        language: limits.language.clone(),
        ignores: limits.ignores.clone(),
    };
    let mut progress = Progress::default();
    let result = search(&scan_limits, &mut progress);
    let result = match result {
        Ok(_) if Instant::now() >= scan_limits.deadline => Err(SearchFailure::Deadline),
        result => result,
    };
    match result {
        Err(SearchFailure::Deadline) => {
            let locations = progress.verify(root, limits);
            if locations.is_empty() {
                Err(SearchFailure::Deadline)
            } else {
                Err(SearchFailure::PartialDeadline(locations))
            }
        }
        result => result,
    }
}

#[cfg(test)]
#[path = "search_partial_tests.rs"]
mod tests;
