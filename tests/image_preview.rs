use gitbuddy::git::{Operation, Repository};
use std::{io::Cursor, path::Path};

fn png(color: [u8; 3]) -> Vec<u8> {
    let pixels = image::RgbImage::from_pixel(2, 3, image::Rgb(color));
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(pixels)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}
#[test]
fn image_preview_reads_index_and_worktree_and_validates_images() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let raw = git2::Repository::open(dir.path()).unwrap();
    let mut config = raw.config().unwrap();
    config.set_bool("commit.gpgsign", false).unwrap();
    config.set_str("core.hooksPath", "/dev/null").unwrap();
    repo.execute(Operation::SetIdentity(
        "Image Test".into(),
        "image@example.invalid".into(),
    ))
    .unwrap();
    let before = png([255, 0, 0]);
    let after = png([0, 255, 0]);
    std::fs::write(dir.path().join("image.png"), &before).unwrap();
    repo.execute(Operation::StageAll).unwrap();
    repo.execute(Operation::Commit("image".into())).unwrap();
    std::fs::write(dir.path().join("image.png"), &after).unwrap();
    let file = repo
        .snapshot(10)
        .unwrap()
        .files
        .into_iter()
        .find(|f| f.path == Path::new("image.png"))
        .unwrap();
    let pair = repo.preview_work_images(&file, false).unwrap();
    assert_eq!(pair.before.unwrap().bytes, before);
    let displayed = pair.after.unwrap();
    assert_eq!((displayed.width, displayed.height), (2, 3));
    assert_eq!(displayed.bytes, after);
    std::fs::write(dir.path().join("image.png"), b"not an image").unwrap();
    assert!(repo.preview_work_images(&file, false).is_err());
}
