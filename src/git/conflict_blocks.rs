//! Resolve exactly one marker block in a pinned draft, preserving other bytes.
use anyhow::{Result, ensure};
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockChoice {
    Ours,
    Theirs,
    Both,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictBlock {
    pub line: usize,
    pub range: Range<usize>,
    pub ours: Range<usize>,
    pub base: Option<Range<usize>>,
    pub theirs: Range<usize>,
    fingerprint: [u8; 32],
}
fn marker(line: &str) -> Option<(u8, usize)> {
    let line = line.trim_end_matches(['\r', '\n']);
    let first = *line.as_bytes().first()?;
    if !matches!(first, b'<' | b'|' | b'=' | b'>') {
        return None;
    }
    let count = line.bytes().take_while(|&c| c == first).count();
    (count >= 7
        && (count == line.len()
            || line.as_bytes()[count] == b' '
            || line.as_bytes()[count] == b'\t'))
        .then_some((first, count))
}
struct PendingBlock {
    start: usize,
    line: usize,
    width: usize,
    ours_start: usize,
    ours_end: Option<usize>,
    base_start: Option<usize>,
    separator: Option<(usize, usize)>,
}
pub fn conflict_blocks(text: &str) -> Result<Vec<ConflictBlock>> {
    use sha2::Digest;
    let fingerprint = sha2::Sha256::digest(text.as_bytes()).into();
    let mut blocks = Vec::new();
    let mut active: Option<PendingBlock> = None;
    let mut offset = 0;
    for (line_index, line) in text.split_inclusive('\n').enumerate() {
        let end = offset + line.len();
        if let Some((kind, width)) = marker(line) {
            match kind {
                b'<' => {
                    ensure!(
                        active.is_none(),
                        "Nested conflict markers at line {}. Edit the result manually.",
                        line_index + 1
                    );
                    active = Some(PendingBlock {
                        start: offset,
                        line: line_index + 1,
                        width,
                        ours_start: end,
                        ours_end: None,
                        base_start: None,
                        separator: None,
                    });
                }
                b'|' => {
                    let state = active.as_mut().ok_or_else(|| {
                        anyhow::anyhow!("Unexpected base marker at line {}", line_index + 1)
                    })?;
                    ensure!(
                        state.width == width
                            && state.base_start.is_none()
                            && state.separator.is_none(),
                        "Malformed conflict base marker"
                    );
                    state.base_start = Some(end);
                    state.ours_end = Some(offset);
                    // The base range begins after its marker; ours ends before it.
                }
                b'=' => {
                    let state = active.as_mut().ok_or_else(|| {
                        anyhow::anyhow!("Unexpected separator at line {}", line_index + 1)
                    })?;
                    ensure!(
                        state.width == width && state.separator.is_none(),
                        "Malformed conflict separator"
                    );
                    state.separator = Some((offset, end));
                    state.ours_end.get_or_insert(offset);
                }
                b'>' => {
                    let state = active.take().ok_or_else(|| {
                        anyhow::anyhow!("Unexpected end marker at line {}", line_index + 1)
                    })?;
                    ensure!(width == state.width, "Mismatched conflict marker lengths");
                    let (separator_start, theirs_start) = state
                        .separator
                        .ok_or_else(|| anyhow::anyhow!("Missing conflict separator"))?;
                    blocks.push(ConflictBlock {
                        line: state.line,
                        range: state.start..end,
                        ours: state.ours_start..state.ours_end.unwrap(),
                        base: state.base_start.map(|b| b..separator_start),
                        theirs: theirs_start..offset,
                        fingerprint,
                    });
                }
                _ => unreachable!(),
            }
        }
        offset = end;
    }
    ensure!(
        active.is_none(),
        "Incomplete conflict block. Edit the result manually."
    );
    Ok(blocks)
}
pub fn choose_conflict_block(
    text: &str,
    expected: &ConflictBlock,
    choice: BlockChoice,
) -> Result<String> {
    let blocks = conflict_blocks(text)?;
    ensure!(
        blocks.iter().any(|b| b == expected),
        "The conflict draft changed; select the block again."
    );
    let mut result = String::with_capacity(text.len());
    result.push_str(&text[..expected.range.start]);
    if matches!(choice, BlockChoice::Ours | BlockChoice::Both) {
        result.push_str(&text[expected.ours.clone()]);
    }
    if matches!(choice, BlockChoice::Theirs | BlockChoice::Both) {
        result.push_str(&text[expected.theirs.clone()]);
    }
    result.push_str(&text[expected.range.end..]);
    Ok(result)
}
