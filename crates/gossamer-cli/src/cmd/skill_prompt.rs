//! `gos skill-prompt` - prints the embedded Gossamer skill card for
//! AI tooling that needs a quick reference. The canonical source is the
//! repo-root `SKILL.md`, which this embeds directly.

const SKILL_CARD: &str = include_str!("../../../../SKILL.md");

/// Entry point for `gos skill-prompt`.
pub(crate) fn run() {
    out!("{SKILL_CARD}");
}
