use gitbuddy::updates::UpdateInfo;
#[test]
fn update_manifest_uses_semver_and_only_official_release_links() {
    let json=br#"{"tag_name":"v0.10.0","prerelease":false,"draft":false,"html_url":"https://github.com/catsonkeyboard/gitbuddy/releases/tag/v0.10.0","assets":[{"name":"GitBuddy-0.10.0-macos-arm64.zip","browser_download_url":"https://github.com/catsonkeyboard/gitbuddy/releases/download/v0.10.0/GitBuddy-0.10.0-macos-arm64.zip","digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":1024}]}"#;
    let update = UpdateInfo::parse("0.9.0", json, "macos", "aarch64")
        .unwrap()
        .unwrap();
    assert_eq!(update.version, "0.10.0");
    assert!(update.asset.unwrap().url.ends_with("macos-arm64.zip"));
    assert!(
        UpdateInfo::parse("0.10.0", json, "macos", "aarch64")
            .unwrap()
            .is_none()
    );
    let bad = String::from_utf8(json.to_vec()).unwrap().replace(
        "https://github.com/catsonkeyboard/gitbuddy/releases/download/",
        "https://attacker.invalid/",
    );
    assert!(UpdateInfo::parse("0.9.0", bad.as_bytes(), "macos", "aarch64").is_err());
}
#[test]
fn prereleases_drafts_and_unverified_assets_are_not_installable() {
    let json=br#"{"tag_name":"v1.0.0","prerelease":true,"draft":false,"html_url":"https://github.com/catsonkeyboard/gitbuddy/releases/tag/v1.0.0","assets":[]}"#;
    assert!(
        UpdateInfo::parse("0.1.0", json, "linux", "x86_64")
            .unwrap()
            .is_none()
    );
    let json=br#"{"tag_name":"v1.0.0","prerelease":false,"draft":false,"html_url":"https://github.com/catsonkeyboard/gitbuddy/releases/tag/v1.0.0","assets":[{"name":"GitBuddy-1.0.0-windows-AMD64.zip","browser_download_url":"https://github.com/catsonkeyboard/gitbuddy/releases/download/v1.0.0/file.zip","size":10}]}"#;
    let update = UpdateInfo::parse("0.1.0", json, "windows", "x86_64")
        .unwrap()
        .unwrap();
    assert!(update.asset.is_none());
}

#[test]
fn release_urls_cannot_escape_the_project_through_dot_segments() {
    let base = serde_json::json!({"tag_name":"v1.0.0","prerelease":false,"draft":false,"html_url":"https://github.com/catsonkeyboard/gitbuddy/releases/tag/v1.0.0","assets":[{"name":"GitBuddy-1.0.0-macos-arm64.zip","browser_download_url":"https://github.com/catsonkeyboard/gitbuddy/releases/download/v1.0.0/file.zip","digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":10}]});
    let mut asset_escape = base.clone();
    asset_escape["assets"][0]["browser_download_url"] = serde_json::json!(
        "https://github.com/catsonkeyboard/gitbuddy/releases/download/../../../../../other-owner/other-repo/payload.zip"
    );
    assert!(
        UpdateInfo::parse(
            "0.1.0",
            &serde_json::to_vec(&asset_escape).unwrap(),
            "macos",
            "aarch64"
        )
        .is_err()
    );
    let mut page_escape = base;
    page_escape["html_url"] = serde_json::json!(
        "https://github.com/catsonkeyboard/gitbuddy/releases/tag/../../../../../other-owner/other-repo"
    );
    assert!(
        UpdateInfo::parse(
            "0.1.0",
            &serde_json::to_vec(&page_escape).unwrap(),
            "macos",
            "aarch64"
        )
        .is_err()
    );
}

#[test]
#[ignore = "queries the public GitBuddy release endpoint"]
fn published_release_endpoint_is_readable() {
    let result = gitbuddy::updates::check_updates().unwrap();
    match result {
        gitbuddy::updates::UpdateCheck::NoReleases | gitbuddy::updates::UpdateCheck::Current => {}
        gitbuddy::updates::UpdateCheck::Available(update) => assert!(
            update
                .release_url
                .starts_with("https://github.com/catsonkeyboard/gitbuddy/releases/tag/")
        ),
    }
}
