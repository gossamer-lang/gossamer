//! Tests that checked-in docs assets keep volatile repo facts live.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

#[test]
fn docs_repo_header_facts_are_live_and_loaded() {
    let root = workspace_root();
    let mkdocs = std::fs::read_to_string(root.join("mkdocs.yml")).expect("read mkdocs.yml");
    assert!(
        mkdocs.contains("  - js/repo_button.js"),
        "mkdocs.yml must load repo_button.js"
    );

    let script = std::fs::read_to_string(root.join("docs_src/js/repo_button.js"))
        .expect("read repo_button.js");
    assert!(
        script.contains("https://api.github.com/repos/gossamer-lang/gossamer"),
        "repo header facts must come from GitHub at runtime"
    );
    assert!(
        script.contains("cache: \"no-store\""),
        "repo header fetches must not reuse stale browser cache entries"
    );
    assert!(
        script.contains("__source"),
        "Material source-fact session cache must be cleared"
    );
    assert!(
        script.contains("/releases/latest") && script.contains("/tags?per_page=1"),
        "the displayed version must be fetched from releases or tags"
    );
    assert!(
        script.contains("STAR_ICON") && script.contains("FORK_ICON"),
        "star and fork facts must render with icons"
    );

    let css = std::fs::read_to_string(root.join("docs_src/stylesheets/extra.css"))
        .expect("read docs extra.css");
    assert!(
        css.contains(".gos-source-fact-icon") && css.contains("color: currentColor"),
        "repo facts must inherit the visible source link color across themes"
    );
}

/// Every lesson program the tour ships, keyed by its slug, in page order.
fn tour_lessons(source: &str) -> Vec<(String, String)> {
    let mut lessons = Vec::new();
    let mut slugs = source.match_indices("slug: \"").map(|(at, _)| {
        let rest = &source[at + "slug: \"".len()..];
        rest[..rest.find('"').expect("a closed slug")].to_string()
    });
    let mut cursor = 0;
    while let Some(at) = source[cursor..].find("    code: `") {
        let start = cursor + at + "    code: `".len();
        let mut end = start;
        loop {
            end += source[end..].find('`').expect("a closed code literal");
            let escapes = source[..end]
                .chars()
                .rev()
                .take_while(|c| *c == '\\')
                .count();
            if escapes % 2 == 0 {
                break;
            }
            end += 1;
        }
        let slug = slugs.next().expect("a slug for every lesson");
        lessons.push((slug, unescape_template(&source[start..end])));
        cursor = end + 1;
    }
    lessons
}

/// The text a JavaScript template literal with this body denotes.
fn unescape_template(body: &str) -> String {
    let mut text = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(escaped) => text.push(escaped),
                None => text.push('\\'),
            }
        } else {
            text.push(c);
        }
    }
    text
}

/// The tour teaches the language through programs a reader runs, so a
/// lesson that no longer parses or type-checks teaches the wrong
/// spelling. Checking is the deterministic half of that contract: it
/// needs no ports, no clock, and no network.
#[test]
fn every_tour_lesson_still_checks() {
    let root = workspace_root();
    let source = std::fs::read_to_string(root.join("landing/tour/tour.js")).expect("read tour.js");
    let lessons = tour_lessons(&source);
    assert!(
        lessons.len() >= 30,
        "the tour should carry its full lesson set, found {}",
        lessons.len()
    );

    let dir = std::env::temp_dir().join("gos-tour-lessons");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the lesson directory");

    let gos = PathBuf::from(env!("CARGO_BIN_EXE_gos"));
    let mut broken = Vec::new();
    for (slug, code) in &lessons {
        let file = dir.join(format!("{slug}.gos"));
        std::fs::write(&file, code).expect("write the lesson");
        let out = std::process::Command::new(&gos)
            .arg("check")
            .arg(&file)
            .output()
            .expect("spawn gos check");
        if !out.status.success() {
            broken.push(format!(
                "{slug}:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    assert!(
        broken.is_empty(),
        "tour lessons no longer check:\n{}",
        broken.join("\n\n")
    );
}

/// The home page's language tour listings are the first Gossamer a
/// reader sees, and they are static markup no runtime ever exercises,
/// so nothing but a gate keeps them spelling the current language.
#[test]
fn every_landing_listing_still_checks() {
    let root = workspace_root();
    let page = std::fs::read_to_string(root.join("landing/index.html")).expect("read index.html");

    let mut listings = Vec::new();
    let mut cursor = 0;
    while let Some(at) = page[cursor..].find("<code class=\"language-gossamer\">") {
        let start = cursor + at + "<code class=\"language-gossamer\">".len();
        let end = start + page[start..].find("</code>").expect("a closed listing");
        listings.push(unescape_markup(&page[start..end]));
        cursor = end;
    }
    assert!(
        listings.len() >= 5,
        "the home page should carry its full listing set, found {}",
        listings.len()
    );

    let dir = std::env::temp_dir().join("gos-landing-listings");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the listing directory");

    let gos = PathBuf::from(env!("CARGO_BIN_EXE_gos"));
    let mut broken = Vec::new();
    for (index, code) in listings.iter().enumerate() {
        let file = dir.join(format!("listing-{index}.gos"));
        std::fs::write(&file, code).expect("write the listing");
        let out = std::process::Command::new(&gos)
            .arg("check")
            .arg(&file)
            .output()
            .expect("spawn gos check");
        if !out.status.success() {
            broken.push(format!(
                "listing {index}:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    assert!(
        broken.is_empty(),
        "home page listings no longer check:\n{}",
        broken.join("\n\n")
    );
}

/// The text an HTML fragment escaped with the entities these listings
/// use denotes.
fn unescape_markup(fragment: &str) -> String {
    fragment
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// The playground's advertised run shortcut has to outrank `CodeMirror`'s own
/// binding for the same chord.
///
/// `basicSetup` carries the default keymap, whose `Mod-Enter` inserts a blank
/// line. A binding registered at ordinary precedence sits behind it, so the
/// chord the page advertises has to come in at the highest precedence.
#[test]
fn playground_run_shortcut_outranks_the_default_keymap() {
    let root = workspace_root();
    let source = std::fs::read_to_string(root.join("landing/playground/playground.js"))
        .expect("read playground.js");
    let page = std::fs::read_to_string(root.join("landing/playground/index.html"))
        .expect("read playground index.html");
    assert!(
        page.contains("Ctrl / Cmd + Enter to run"),
        "the playground page no longer advertises the run shortcut"
    );
    let binding = source
        .find("\"Mod-Enter\"")
        .expect("playground.js binds Mod-Enter");
    let wrapper = source[..binding]
        .rfind("Prec.highest(")
        .expect("the Mod-Enter binding is registered through Prec.highest");
    let between = &source[wrapper..binding];
    assert!(
        between.contains("keymap.of(") && !between.contains("basicSetup"),
        "the Mod-Enter binding is not the keymap Prec.highest wraps"
    );
}
