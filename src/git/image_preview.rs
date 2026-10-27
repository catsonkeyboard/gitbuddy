use super::{FileChange, Repository, libgit::raw};
use anyhow::{Context, Result, ensure};
use std::io::{Cursor, Read};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
#[derive(Clone, Debug)]
pub enum ImageKind {
    Png,
    Jpeg,
}
#[derive(Clone, Debug)]
pub struct ImageData {
    pub kind: ImageKind,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}
#[derive(Clone, Debug)]
pub struct ImagePair {
    pub before: Option<ImageData>,
    pub after: Option<ImageData>,
}

fn checked(bytes: Vec<u8>) -> Result<ImageData> {
    ensure!((bytes.len() as u64) <= MAX_BYTES, "Image exceeds 8 MiB");
    let format = image::guess_format(&bytes).context("Unsupported or invalid image")?;
    let kind = match format {
        image::ImageFormat::Png => ImageKind::Png,
        image::ImageFormat::Jpeg => ImageKind::Jpeg,
        _ => anyhow::bail!("Only PNG and JPEG previews are supported"),
    };
    let dimensions =
        image::ImageReader::with_format(Cursor::new(&bytes), format).into_dimensions()?;
    ensure!(
        dimensions.0 > 0
            && dimensions.1 > 0
            && dimensions.0 <= 4096
            && dimensions.1 <= 4096
            && u64::from(dimensions.0) * u64::from(dimensions.1) <= 16_000_000,
        "Image dimensions exceed the preview limit"
    );
    let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(96 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().context("Cannot decode image")?;
    Ok(ImageData {
        kind,
        bytes,
        width: dimensions.0,
        height: dimensions.1,
    })
}
fn blob(repo: &git2::Repository, id: git2::Oid, mode: u32) -> Result<Option<ImageData>> {
    ensure!(
        matches!(mode, 0o100644 | 0o100755),
        "Image must be a regular file"
    );
    let (size, kind) = repo.odb()?.read_header(id)?;
    ensure!(
        kind == git2::ObjectType::Blob && size as u64 <= MAX_BYTES,
        "Image exceeds 8 MiB"
    );
    Ok(Some(checked(repo.find_blob(id)?.content().to_vec())?))
}
impl Repository {
    pub fn preview_work_images(&self, file: &FileChange, staged: bool) -> Result<ImagePair> {
        ensure!(
            !file.conflict(),
            "Resolve the conflict before using this image preview"
        );
        let repo = raw(self)?;
        let index = repo.index()?;
        let path = &file.path;
        ensure!(
            !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|p| matches!(p, std::path::Component::Normal(_))),
            "Invalid image path"
        );
        let indexed = || -> Result<Option<ImageData>> {
            match index.get_path(path, 0) {
                Some(e) => blob(&repo, e.id, e.mode),
                None => Ok(None),
            }
        };
        let (before, after) = if staged {
            let old_path = file.original.as_deref().unwrap_or(path);
            let before = if let Some(tree) = repo.head().ok().and_then(|h| h.peel_to_tree().ok()) {
                match tree.get_path(old_path) {
                    Ok(entry) => blob(&repo, entry.id(), entry.filemode() as u32)?,
                    Err(e) if e.code() == git2::ErrorCode::NotFound => None,
                    Err(e) => return Err(e.into()),
                }
            } else {
                None
            };
            (before, indexed()?)
        } else {
            let before = indexed()?;
            let mut absolute = self.root.clone();
            for part in path.components() {
                let std::path::Component::Normal(name) = part else {
                    unreachable!()
                };
                absolute.push(name);
                match std::fs::symlink_metadata(&absolute) {
                    Ok(meta) => ensure!(
                        !meta.file_type().is_symlink(),
                        "Image paths cannot include symbolic links"
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                    Err(error) => return Err(error.into()),
                }
            }
            let after = match std::fs::File::open(&absolute) {
                Ok(f) => {
                    let mut bytes = Vec::new();
                    f.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
                    Some(checked(bytes)?)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            (before, after)
        };
        ensure!(
            before.is_some() || after.is_some(),
            "No image exists to preview"
        );
        Ok(ImagePair { before, after })
    }
}
