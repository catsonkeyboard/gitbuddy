//! Shared pinned conversion snapshots for canonical Git content and worktree bytes.
//! Arbitrary custom filter drivers remain unsupported.
#[path = "patch_encoding.rs"]
mod encoding;
#[path = "conversion_file.rs"]
mod file;
use crate::git::lfs::{self, AttributeLookup, AttributeQuery, AttributeState, query_attributes};
use anyhow::{Context, Result, ensure};
pub(in crate::git) use file::{FileStamp, WorkingFile};
#[path = "conversion_inspect.rs"]
mod inspect;
use gix_filter::eol::{self, AttributesDigest, AutoCrlf, Configuration, Mode};
pub(in crate::git) use inspect::Inspector;
#[path = "conversion_diff.rs"]
mod diff;
pub(in crate::git) use diff::working_diff;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

const ATTRS: &[&str] = &[
    "crlf",
    "text",
    "eol",
    "ident",
    "filter",
    "working-tree-encoding",
];
const MAX_FILE: usize = 16 * 1024 * 1024;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(in crate::git) enum WorkingLine<'a> {
    Keep(std::ops::Range<usize>),
    Restore(&'a [u8]),
}

fn codec_for(
    path: &Path,
    query: &AttributeQuery,
    config: &Config,
) -> Result<Option<encoding::Codec>> {
    if let Some(AttributeState::Value(driver)) = query.values.get("filter") {
        config.filters.validate(driver, path)?;
    }
    match query.values.get("working-tree-encoding") {
        None | Some(AttributeState::Unspecified | AttributeState::Unset) => Ok(None),
        Some(AttributeState::Set) => {
            anyhow::bail!("working-tree-encoding requires a named encoding: {path:?}")
        }
        Some(AttributeState::Value(label)) => Ok(Some(encoding::Codec::parse(label)?)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::git) enum CleanPolicy {
    Add,
    PatchApply,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::git) struct Limits {
    pub max_bytes: usize,
}
impl Limits {
    pub(in crate::git) const PATCH: Self = Self {
        max_bytes: MAX_FILE,
    };
    pub(in crate::git) const ENCODED: Self = Self {
        max_bytes: 16 * 1024 * 1024,
    };
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RawValue {
    Missing,
    Implicit,
    Explicit(Vec<u8>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Config {
    raw: Vec<RawValue>,
    autocrlf: AutoCrlf,
    eol: Option<Mode>,
    filemode: bool,
    filters: FilterDrivers,
}
impl Config {
    fn capture(repo: &git2::Repository) -> Result<Self> {
        // Do not reuse a native config cache between confirmation and writes.
        let fresh = git2::Repository::open(repo.path())?;
        let mut config = fresh.config()?;
        let config = config.snapshot()?;
        Self::read(&config)
    }
    fn read(config: &git2::Config) -> Result<Self> {
        let mut raw = Vec::new();
        for name in [
            "core.autocrlf",
            "core.eol",
            "core.safecrlf",
            "core.checkroundtripencoding",
            "core.filemode",
            "core.symlinks",
        ] {
            raw.push(match config.get_entry(name) {
                Ok(entry) if entry.has_value() => RawValue::Explicit(entry.value_bytes().to_vec()),
                Ok(_) => RawValue::Implicit,
                Err(error) if error.code() == git2::ErrorCode::NotFound => RawValue::Missing,
                Err(error) => return Err(error.into()),
            });
        }
        let autocrlf = if matches!(&raw[0], RawValue::Explicit(value) if value.eq_ignore_ascii_case(b"input"))
        {
            AutoCrlf::Input
        } else {
            match config.get_bool("core.autocrlf") {
                Ok(true) => AutoCrlf::Enabled,
                Ok(false) => AutoCrlf::Disabled,
                Err(error) if error.code() == git2::ErrorCode::NotFound => AutoCrlf::Disabled,
                Err(error) => return Err(error).context("Invalid core.autocrlf"),
            }
        };
        ensure!(
            !matches!(raw[1], RawValue::Implicit),
            "core.eol requires an explicit value"
        );
        ensure!(
            !matches!(raw[3], RawValue::Implicit),
            "core.checkRoundtripEncoding requires an explicit value"
        );
        let eol = match &raw[1] {
            RawValue::Explicit(value) if value == b"lf" => Some(Mode::Lf),
            RawValue::Explicit(value) if value == b"crlf" => Some(Mode::CrLf),
            // Git's core.eol parser is case-sensitive; other strings use native EOL.
            _ => None,
        };
        let filemode = match config.get_bool("core.filemode") {
            Ok(value) => value,
            Err(error) if error.code() == git2::ErrorCode::NotFound => true,
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            raw,
            autocrlf,
            eol,
            filemode,
            filters: FilterDrivers(active_filter_drivers(config)?),
        })
    }
    fn eol_config(&self) -> Configuration {
        Configuration {
            auto_crlf: self.autocrlf,
            eol: self.eol,
        }
    }
}

fn active_filter_drivers(config: &git2::Config) -> Result<BTreeSet<Vec<u8>>> {
    let mut keys = BTreeSet::new();
    config.entries(None)?.for_each(|entry| {
        if entry.name_bytes().starts_with(b"filter.") {
            keys.insert(entry.name_bytes().to_vec());
        }
    })?;
    let mut active = BTreeSet::new();
    for key in keys {
        let fields = &key[7..];
        let Some(separator) = fields.iter().rposition(|byte| *byte == b'.') else {
            continue;
        };
        let (driver, action) = (&fields[..separator], &fields[separator + 1..]);
        if driver == b"lfs" || active.contains(driver) {
            continue;
        }
        let relevant = matches!(action, b"clean" | b"smudge" | b"process" | b"required");
        if !relevant {
            continue;
        }
        let enabled = match std::str::from_utf8(&key) {
            Ok(name) if action == b"required" => config.get_bool(name).unwrap_or(true),
            Ok(name) => {
                let entry = config.get_entry(name)?;
                !entry.has_value() || !entry.value_bytes().is_empty()
            }
            // libgit2 only exposes effective lookup via a UTF-8 key. A
            // matching configured non-UTF-8 driver is also unsupported.
            Err(_) => true,
        };
        if enabled {
            active.insert(driver.to_vec());
        }
    }
    Ok(active)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::git) struct FilterDrivers(BTreeSet<Vec<u8>>);
impl FilterDrivers {
    pub(in crate::git) fn capture(repo: &git2::Repository) -> Result<Self> {
        let fresh = git2::Repository::open(repo.path())?;
        let config = fresh.config()?.snapshot()?;
        Ok(Self(active_filter_drivers(&config)?))
    }
    pub(in crate::git) fn validate(&self, driver: &[u8], path: &Path) -> Result<()> {
        self.validate_driver(driver, Some(path))
    }
    fn validate_driver(&self, driver: &[u8], path: Option<&Path>) -> Result<()> {
        ensure!(
            !self.0.contains(driver),
            "Content conversion does not run custom filter drivers: {}{}",
            String::from_utf8_lossy(driver),
            path.map_or_else(String::new, |path| format!(" for {path:?}")),
        );
        Ok(())
    }
}

pub(in crate::git) fn validate_filter_driver(repo: &git2::Repository, driver: &[u8]) -> Result<()> {
    if driver == b"lfs" {
        return Ok(());
    }
    FilterDrivers::capture(repo)?.validate_driver(driver, None)
}

impl Config {
    fn add_roundtrip<'a>(
        &self,
        path: &'a Path,
        policy: CleanPolicy,
    ) -> Result<Option<eol::convert_to_git::RoundTripCheck<'a>>> {
        if policy != CleanPolicy::Add {
            return Ok(None);
        }
        use eol::convert_to_git::RoundTripCheck;
        Ok(match &self.raw[2] {
            RawValue::Missing => None,
            RawValue::Implicit => Some(RoundTripCheck::Fail { rela_path: path }),
            RawValue::Explicit(value) if value.eq_ignore_ascii_case(b"warn") => {
                Some(RoundTripCheck::Warn { rela_path: path })
            }
            RawValue::Explicit(value) => {
                if git2::Config::parse_bool(std::str::from_utf8(value)?)? {
                    Some(RoundTripCheck::Fail { rela_path: path })
                } else {
                    None
                }
            }
        })
    }
    fn checks_encoding(&self, attribute: Option<&AttributeState>) -> Result<bool> {
        let Some(AttributeState::Value(label)) = attribute else {
            return Ok(false);
        };
        let list = match &self.raw[3] {
            RawValue::Missing => b"SHIFT-JIS".as_slice(),
            RawValue::Explicit(value) => value.as_slice(),
            RawValue::Implicit => {
                anyhow::bail!("core.checkRoundtripEncoding requires an explicit value")
            }
        };
        Ok(list
            .split(|byte| byte.is_ascii_whitespace() || *byte == b',')
            .any(|name| name.eq_ignore_ascii_case(label)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::git) struct Snapshot {
    config: Config,
    paths: BTreeMap<PathBuf, AttributeQuery>,
    lfs: Option<[u8; 32]>,
    lookup: AttributeLookup,
    encodings: BTreeMap<PathBuf, encoding::Codec>,
}
impl Snapshot {
    pub(in crate::git) fn has_encoding(&self, path: &Path) -> bool {
        self.encodings.contains_key(path)
    }
    pub(in crate::git) fn same_conversion_rules(&self, other: &Self, path: &Path) -> Result<bool> {
        Ok(self.rules(path)? == other.rules(path)?
            && self.encodings.get(path) == other.encodings.get(path)
            && self.has_lfs(path) == other.has_lfs(path)
            && (!self.has_lfs(path) || self.lfs == other.lfs))
    }
    pub(in crate::git) fn canonical_mode(&self, actual: u32, indexed: Option<u32>) -> u32 {
        if self.config.filemode {
            actual
        } else {
            indexed
                .filter(|mode| matches!(mode, 0o100644 | 0o100755))
                .unwrap_or(0o100644)
        }
    }
    pub(in crate::git) fn validate_add_settings(&self, path: &Path) -> Result<()> {
        self.config
            .add_roundtrip(path, CleanPolicy::Add)
            .map(|_| ())
    }
    pub(in crate::git) fn plain_lfs(&self, path: &Path) -> Result<bool> {
        let (digest, ident) = self.rules(path)?;
        Ok(self.has_lfs(path)
            && !self.has_encoding(path)
            && digest == AttributesDigest::Binary
            && !ident)
    }
    pub(in crate::git) fn native_equivalent(
        &self,
        repo: &git2::Repository,
        path: &Path,
    ) -> Result<bool> {
        if self.has_encoding(path) || self.has_lfs(path) {
            return Ok(false);
        }
        let flags = match self.lookup {
            AttributeLookup::WorktreeThenIndex => git2::AttrCheckFlags::FILE_THEN_INDEX,
            AttributeLookup::IndexThenWorktree => git2::AttrCheckFlags::INDEX_THEN_FILE,
            AttributeLookup::TargetTree(_) => return Ok(false),
        };
        let expected = self
            .paths
            .get(path)
            .context("Missing conversion snapshot")?;
        for name in ATTRS {
            let native =
                match git2::AttrValue::always_bytes(repo.get_attr_bytes(path, name, flags)?) {
                    git2::AttrValue::Unspecified => AttributeState::Unspecified,
                    git2::AttrValue::True => AttributeState::Set,
                    git2::AttrValue::False => AttributeState::Unset,
                    git2::AttrValue::Bytes(bytes) => AttributeState::Value(bytes.to_vec()),
                    git2::AttrValue::String(value) => {
                        AttributeState::Value(value.as_bytes().to_vec())
                    }
                };
            if expected
                .values
                .get(*name)
                .unwrap_or(&AttributeState::Unspecified)
                != &native
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub(in crate::git) fn try_native_stage(
        &self,
        repo: &git2::Repository,
        index: &mut git2::Index,
        path: &Path,
    ) -> Result<bool> {
        self.validate_add_settings(path)?;
        let fresh = git2::Repository::open(repo.path())?;
        let mut config = fresh.config()?;
        let config = config.snapshot()?;
        ensure!(
            Config::read(&config)? == self.config,
            "Conversion configuration changed before staging"
        );
        let previous = repo.config()?;
        struct Restore<'a> {
            repo: &'a git2::Repository,
            config: git2::Config,
        }
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                let _ = self.repo.set_config(&self.config);
            }
        }
        repo.set_config(&config)?;
        let _restore = Restore {
            repo,
            config: previous,
        };
        if !self.native_equivalent(repo, path)? {
            return Ok(false);
        }
        index.add_path(path)?;
        self.validate_path(repo, path)?;
        Ok(true)
    }
    pub(in crate::git) fn capture(
        repo: &git2::Repository,
        paths: &BTreeSet<PathBuf>,
        lookup: AttributeLookup,
    ) -> Result<Self> {
        let config = Config::capture(repo)?;
        let mut queries = BTreeMap::new();
        let mut encodings = BTreeMap::new();
        for path in paths {
            let query = query_attributes(repo, Path::new(path), lookup, ATTRS)?;
            if let Some(codec) = codec_for(path, &query, &config)? {
                encodings.insert(path.clone(), codec);
            }
            queries.insert(path.clone(), query);
        }
        ensure!(
            config == Config::capture(repo)?,
            "Conversion configuration changed while reading; retry"
        );
        let lfs = queries
            .values()
            .any(|query| matches!(query.values.get("filter"), Some(AttributeState::Value(value)) if value == b"lfs"))
            .then(|| lfs::conversion_fingerprint(repo))
            .transpose()?;
        let snapshot = Self {
            config,
            paths: queries,
            lfs,
            lookup,
            encodings,
        };
        snapshot.validate(repo)?;
        Ok(snapshot)
    }
    pub(in crate::git) fn validate(&self, repo: &git2::Repository) -> Result<()> {
        ensure!(
            self.config == Config::capture(repo)?,
            "Conversion configuration changed; review the patch again"
        );
        self.validate_lfs(repo)?;
        for path in self.paths.keys() {
            self.validate_path(repo, path)?;
        }
        Ok(())
    }
    pub(in crate::git) fn validate_path(&self, repo: &git2::Repository, path: &Path) -> Result<()> {
        ensure!(
            self.config == Config::capture(repo)?,
            "Conversion configuration changed; review the patch again"
        );
        self.validate_lfs(repo)?;
        if let Some(expected) = self.paths.get(path) {
            let actual = query_attributes(repo, Path::new(path), self.lookup, ATTRS)?;
            ensure!(
                &actual == expected,
                "Conversion attributes changed; review the patch again: {path:?}"
            );
        }
        Ok(())
    }
    fn validate_lfs(&self, repo: &git2::Repository) -> Result<()> {
        if let Some(expected) = self.lfs {
            ensure!(
                lfs::conversion_fingerprint(repo)? == expected,
                "LFS conversion configuration changed; review the patch again"
            );
        }
        Ok(())
    }
    pub(in crate::git) fn has_lfs(&self, path: &Path) -> bool {
        matches!(self.paths.get(path).and_then(|query| query.values.get("filter")), Some(AttributeState::Value(value)) if value == b"lfs")
    }
    fn rules(&self, path: &Path) -> Result<(AttributesDigest, bool)> {
        let attrs = &self
            .paths
            .get(path)
            .context("Missing conversion snapshot")?
            .values;
        let state = |name: &str| attrs.get(name).unwrap_or(&AttributeState::Unspecified);
        let crlf = |state: &AttributeState| match state {
            AttributeState::Set => Some(AttributesDigest::Text),
            AttributeState::Unset => Some(AttributesDigest::Binary),
            AttributeState::Value(value) if value == b"input" => Some(AttributesDigest::TextInput),
            AttributeState::Value(value) if value == b"auto" => Some(AttributesDigest::TextAuto),
            _ => None,
        };
        let mut digest = crlf(state("text")).or_else(|| crlf(state("crlf")));
        if digest != Some(AttributesDigest::Binary) {
            digest = match (digest, state("eol")) {
                (Some(AttributesDigest::TextAuto), AttributeState::Value(v)) if v == b"lf" => {
                    Some(AttributesDigest::TextAutoInput)
                }
                (Some(AttributesDigest::TextAuto), AttributeState::Value(v)) if v == b"crlf" => {
                    Some(AttributesDigest::TextAutoCrlf)
                }
                (_, AttributeState::Value(v)) if v == b"lf" => Some(AttributesDigest::TextInput),
                (_, AttributeState::Value(v)) if v == b"crlf" => Some(AttributesDigest::TextCrlf),
                (other, _) => other,
            };
        }
        let config = self.config.eol_config();
        let digest = match digest {
            Some(AttributesDigest::Text) => config.to_eol().into(),
            Some(value) => value,
            None => config.auto_crlf.into(),
        };
        Ok((digest, matches!(state("ident"), AttributeState::Set)))
    }
    pub(in crate::git) fn clean(
        &self,
        repo: &git2::Repository,
        path: &Path,
        bytes: &[u8],
        indexed: &[u8],
        policy: CleanPolicy,
        limits: Limits,
    ) -> Result<Vec<u8>> {
        ensure!(
            bytes.len() <= limits.max_bytes,
            "Working file exceeds the conversion byte limit"
        );
        let lfs_bytes = self
            .has_lfs(path)
            .then(|| {
                lfs::clean_content_at_bounded(repo, bytes, Path::new(path), limits.max_bytes as u64)
            })
            .transpose()?;
        let bytes = lfs_bytes.as_deref().unwrap_or(bytes);
        let decoded = self
            .encodings
            .get(path)
            .map(|codec| codec.decode(bytes, limits.max_bytes))
            .transpose()?;
        if policy == CleanPolicy::Add
            && let (Some(codec), Some(decoded)) = (self.encodings.get(path), decoded.as_ref())
            && self.config.checks_encoding(
                self.paths
                    .get(path)
                    .and_then(|query| query.values.get("working-tree-encoding")),
            )?
        {
            ensure!(
                codec.encode(decoded, limits.max_bytes)? == bytes,
                "Encoding to UTF-8 and back is not the same: {path:?}"
            );
        }
        let bytes = decoded.as_deref().unwrap_or(bytes);
        let (digest, ident) = self.rules(path)?;
        let mut output = Vec::new();
        // Like Git apply, skip git-add-only safecrlf/source encoding byte
        // round-trip checks. The caller still verifies the canonical postimage
        // survives encode+clean exactly, without any character replacement.
        let converted = eol::convert_to_git(
            bytes,
            digest,
            &mut output,
            &mut |buf| {
                buf.extend_from_slice(indexed);
                Ok(Some(()))
            },
            eol::convert_to_git::Options {
                round_trip_check: self.config.add_roundtrip(path, policy)?,
                config: self.config.eol_config(),
            },
        )
        .map_err(|error| anyhow::anyhow!("Cannot clean patch source: {error}"))?;
        let bytes = if converted { output } else { bytes.to_vec() };
        let mut clean = Vec::new();
        if ident && gix_filter::ident::undo(&bytes, &mut clean)? {
            Ok(clean)
        } else {
            Ok(bytes)
        }
    }
    pub(in crate::git) fn smudge(
        &self,
        repo: &git2::Repository,
        path: &Path,
        canonical: &[u8],
        oid: git2::Oid,
        limits: Limits,
    ) -> Result<Vec<u8>> {
        ensure!(
            canonical.len() <= limits.max_bytes,
            "Canonical content exceeds the conversion byte limit"
        );
        let (digest, ident) = self.rules(path)?;
        let bytes = if ident {
            expand_ident(canonical, oid, limits.max_bytes)?
        } else {
            canonical.to_vec()
        };
        let config = self.config.eol_config();
        let stats = eol::Stats::from_bytes(&bytes);
        if stats.will_convert_lf_to_crlf(digest, config) {
            ensure!(
                bytes
                    .len()
                    .checked_add(stats.lone_lf)
                    .is_some_and(|size| size <= limits.max_bytes),
                "Converted working file exceeds the conversion byte limit"
            );
        }
        let mut output = Vec::new();
        let bytes = if eol::convert_to_worktree(&bytes, digest, &mut output, config)
            .map_err(|error| anyhow::anyhow!("Cannot convert patched file: {error}"))?
        {
            output
        } else {
            bytes
        };
        let bytes = match self.encodings.get(path) {
            Some(codec) => codec.encode(&bytes, limits.max_bytes)?,
            None => bytes,
        };
        if self.has_lfs(path) {
            ensure!(
                lfs::Pointer::parse(&bytes).is_some(),
                "LFS patch result must be a supported canonical pointer: {path:?}"
            );
            // Validate canonical size before the inverse engine reads its cache.
            let pointer = lfs::clean_content_at_bounded(
                repo,
                &bytes,
                Path::new(path),
                limits.max_bytes as u64,
            )?;
            lfs::local_smudge_content_at(repo, &pointer, Path::new(path), limits.max_bytes as u64)
        } else {
            Ok(bytes)
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(in crate::git) fn splice_working_lines(
        &self,
        path: &Path,
        raw: &[u8],
        current: &[u8],
        result: &[u8],
        lines: &[WorkingLine<'_>],
    ) -> Result<Vec<u8>> {
        let limit = Limits::ENCODED.max_bytes;
        ensure!(
            result.len() <= limit,
            "Partial result exceeds the conversion byte limit"
        );
        if raw.is_empty() && result.is_empty() {
            return Ok(Vec::new());
        }
        let codec = self
            .encodings
            .get(path)
            .context("Missing partial encoding")?;
        let layout = codec.line_layout(raw)?;
        let width = layout.delimiter.len();
        let mut ranges = layout
            .body
            .chunks_exact(width)
            .enumerate()
            .filter_map(|(index, unit)| (unit == layout.delimiter).then_some((index + 1) * width))
            .chain(
                (!layout.body.is_empty() && !layout.body.ends_with(layout.delimiter))
                    .then_some(layout.body.len()),
            )
            .scan(0, |start, end| Some(std::mem::replace(start, end)..end));
        let mut raw_cursor = 0;
        let (digest, ident) = self.rules(path)?;
        let crlf = eol::Stats::from_bytes(result)
            .will_convert_lf_to_crlf(digest, self.config.eol_config());
        let oid = git2::Oid::hash_object(git2::ObjectType::Blob, result)?;
        let mut output = layout.prefix.to_vec();
        let mut restore = Vec::new();
        let append = |output: &mut Vec<u8>, bytes: &[u8]| -> Result<()> {
            ensure!(
                output.len().saturating_add(bytes.len()) <= limit,
                "Partial working result exceeds the conversion byte limit"
            );
            output.extend_from_slice(bytes);
            Ok(())
        };
        let flush = |output: &mut Vec<u8>, restore: &mut Vec<u8>| -> Result<()> {
            if restore.is_empty() {
                return Ok(());
            }
            let expanded = ident
                .then(|| expand_ident(restore, oid, limit))
                .transpose()?;
            let content = expanded.as_deref().unwrap_or(restore);
            let mut normalized = Vec::new();
            let content = if crlf
                && eol::convert_to_worktree(
                    content,
                    AttributesDigest::TextCrlf,
                    &mut normalized,
                    self.config.eol_config(),
                )? {
                &normalized
            } else {
                content
            };
            let encoded = layout.fragments.encode(content, limit)?;
            append(output, &encoded)?;
            restore.clear();
            Ok(())
        };
        for line in lines {
            match line {
                WorkingLine::Keep(lines) => {
                    flush(&mut output, &mut restore)?;
                    let skip = lines
                        .start
                        .checked_sub(raw_cursor)
                        .context("Original working lines are out of order")?;
                    let first = ranges.nth(skip).context("Missing original working line")?;
                    let end = if lines.len() > 1 {
                        ranges
                            .nth(lines.len() - 2)
                            .context("Missing original working line")?
                            .end
                    } else {
                        first.end
                    };
                    raw_cursor = lines.end;
                    append(&mut output, &layout.body[first.start..end])?;
                }
                WorkingLine::Restore(bytes) => restore.extend_from_slice(bytes),
            }
        }
        ensure!(
            raw_cursor + ranges.count() == current.split_inclusive(|&b| b == b'\n').count(),
            "Cannot safely map canonical lines to their original working bytes"
        );
        flush(&mut output, &mut restore)?;
        Ok(output)
    }
}

fn expand_ident(bytes: &[u8], oid: git2::Oid, limit: usize) -> Result<Vec<u8>> {
    let mut count = 0usize;
    let mut cursor = 0usize;
    while let Some(at) = bytes[cursor..].windows(4).position(|w| w == b"$Id$") {
        count += 1;
        cursor += at + 4;
    }
    let replacement = format!("$Id: {oid} $");
    let size = count
        .checked_mul(replacement.len() - 4)
        .and_then(|extra| bytes.len().checked_add(extra));
    ensure!(
        size.is_some_and(|size| size <= limit),
        "Converted ident working file exceeds 16 MiB"
    );
    let mut output = Vec::with_capacity(size.unwrap_or(0));
    cursor = 0;
    while let Some(at) = bytes[cursor..].windows(4).position(|w| w == b"$Id$") {
        output.extend_from_slice(&bytes[cursor..cursor + at]);
        output.extend_from_slice(replacement.as_bytes());
        cursor += at + 4;
    }
    output.extend_from_slice(&bytes[cursor..]);
    Ok(output)
}
