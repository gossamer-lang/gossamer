//! `gos skill-prompt` - prints the embedded Gossamer skill card for
//! AI tooling that needs a quick reference. The canonical source is the
//! repo-root `SKILL.md`; `docs_src/skill_card.md` only transcludes it
//! for the docs site, so we embed `SKILL.md` directly.

const SKILL_CARD: &str = include_str!("../../../../SKILL.md");

/// Entry point for `gos skill-prompt`.
pub(crate) fn run() {
    out!("{SKILL_CARD}");
}
